// SPDX-License-Identifier: MIT OR Apache-2.0

//! The audio router as an actor (#147).
//!
//! The graph thread owns the [`AudioRouter`] and runs it one [`Message`] at a
//! time: a message is one uninterrupted read–decide–write sequence, with
//! nothing shared left to lock. A
//! [`crate::router_handle::RouterHandle`] sends each message in an
//! [`Envelope`] through a [`Transport`] and awaits its `oneshot` reply, so a
//! waiting request costs a suspended task.
//!
//! [`Queue`] holds the rules of the wait — which message starts next, and what
//! the others are answered — as functions of a `now` it is handed; it owns no
//! clock. [`Actor`] runs the message the queue yields against the router. The
//! PipeWire loop thread and the in-runtime actor of the tests share both.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use blue2th_proto::SpeakerTarget;
use tokio::sync::{oneshot, watch};

use crate::audio::{AudioError, AudioRouter, CombineBranch};
use crate::graph::Graph;
use crate::graph_pw::COMMAND_TIMEOUT;
use crate::router_handle::RouterError;
use crate::spotify::{spotify_target_sink, COMBINED_SINK_NAME};

/// Where the actor sends the answer to one message.
pub(crate) type Reply<T> = oneshot::Sender<Result<T, RouterError>>;

/// What one repair pass did to the graph.
#[derive(Debug)]
pub struct RepairOutcome {
    /// What `route_for_targets` answered.
    pub routed: Result<(), AudioError>,
    /// Whether the graph accepted at least one change from this pass.
    pub changed: bool,
    /// Whether the pass created the combined sink and could not re-target the
    /// streams onto it (#139).
    pub retarget_failed: bool,
}

/// One operation on the router, carrying its reply.
pub(crate) enum Message {
    /// Route the graph to `speakers`.
    Route {
        speakers: Vec<SpeakerTarget>,
        reply: Reply<()>,
    },
    /// Read the live volume of each speaker in `macs`, in order.
    SinkVolumes {
        macs: Vec<String>,
        reply: Reply<Vec<Option<f32>>>,
    },
    /// Set every speaker in `macs` to `level`, stopping at the first failure.
    SetSinkVolumes {
        macs: Vec<String>,
        level: f32,
        reply: Reply<()>,
    },
    /// Retune `branch` in place inside the combined sink `sink_name`.
    Retune {
        sink_name: String,
        branch: CombineBranch,
        reply: Reply<()>,
    },
    /// Route the graph to `speakers`, then answer the node name `librespot`
    /// is to be pointed at.
    RouteForSpotify {
        speakers: Vec<SpeakerTarget>,
        reply: Reply<String>,
    },
    /// Apply the selection the routing applier read at routing generation
    /// `generation`: tear the combined sink down when it is empty, route
    /// otherwise.
    ApplySelection {
        speakers: Vec<SpeakerTarget>,
        generation: u64,
        reply: Reply<()>,
    },
    /// One repair pass over `speakers`.
    Repair {
        speakers: Vec<SpeakerTarget>,
        reply: Reply<RepairOutcome>,
    },
}

impl Message {
    /// The variant's name, which is all the log of an expiry says of a
    /// message: never its arguments.
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Message::Route { .. } => "Route",
            Message::SinkVolumes { .. } => "SinkVolumes",
            Message::SetSinkVolumes { .. } => "SetSinkVolumes",
            Message::Retune { .. } => "Retune",
            Message::RouteForSpotify { .. } => "RouteForSpotify",
            Message::ApplySelection { .. } => "ApplySelection",
            Message::Repair { .. } => "Repair",
        }
    }

    /// Whether the caller stopped waiting for the answer: it gave up at its
    /// bound, or its task was cancelled.
    fn caller_left(&self) -> bool {
        match self {
            Message::Route { reply, .. } => reply.is_closed(),
            Message::SinkVolumes { reply, .. } => reply.is_closed(),
            Message::SetSinkVolumes { reply, .. } => reply.is_closed(),
            Message::Retune { reply, .. } => reply.is_closed(),
            Message::RouteForSpotify { reply, .. } => reply.is_closed(),
            Message::ApplySelection { reply, .. } => reply.is_closed(),
            Message::Repair { reply, .. } => reply.is_closed(),
        }
    }

    /// Answer `refusal` on the message's own reply instead of running it.
    ///
    /// The `match` has no wildcard arm on purpose: a new variant does not
    /// compile until it answers its refusal, where a wildcard would drop its
    /// reply and its caller would read an actor that died.
    fn refuse(self, refusal: RouterError) {
        match self {
            Message::Route { reply, .. } => answer(reply, Err(refusal)),
            Message::SinkVolumes { reply, .. } => answer(reply, Err(refusal)),
            Message::SetSinkVolumes { reply, .. } => answer(reply, Err(refusal)),
            Message::Retune { reply, .. } => answer(reply, Err(refusal)),
            Message::RouteForSpotify { reply, .. } => answer(reply, Err(refusal)),
            Message::ApplySelection { reply, .. } => answer(reply, Err(refusal)),
            Message::Repair { reply, .. } => answer(reply, Err(refusal)),
        }
    }
}

/// Send `result` on `reply`. A reply nobody waits for any more — the caller
/// gave up while the message ran — is dropped.
fn answer<T, E: Into<RouterError>>(reply: Reply<T>, result: Result<T, E>) {
    let _ = reply.send(result.map_err(Into::into));
}

/// A [`Message`] on its way to the actor, with the instant past which the
/// actor no longer starts it. The deadline is a property of the send, not of
/// the variant: a request carries one, a background message carries `None`
/// and is started however long it waited.
pub(crate) struct Envelope {
    pub(crate) start_by: Option<Instant>,
    pub(crate) message: Message,
}

/// The way from a handle to whatever runs the actor: the PipeWire loop thread
/// in production, an actor inside the tokio runtime in the tests.
pub(crate) trait Transport: Send {
    /// Hand `envelope` to the actor, starting one when there is none and
    /// replacing one that has died. Never blocks. An `Err` is "no actor at
    /// all": the envelope was dropped, its reply with it.
    fn send(&mut self, envelope: Envelope) -> Result<(), AudioError>;
}

/// What a handle and every actor started for it have in common: the routing
/// generation the handle advances and the actor reads, the confirmation
/// due time the actor publishes and the confirmation timer reads, the
/// applier's wake-up, and the re-apply owed to a routing message a stalled
/// daemon left unanswered (#152).
#[derive(Clone)]
pub(crate) struct Shared {
    generation: Arc<AtomicU64>,
    confirmation_due: Arc<watch::Sender<Option<Instant>>>,
    /// Wakes the background routing applier. A `watch` rather than a queue:
    /// every request made before the applier marks it seen folds into one
    /// pass, which reads the selection current at that moment.
    routing_requests: Arc<watch::Sender<()>>,
    /// Whether a routing message ended `Unanswered` since the graph last
    /// answered again. Held here rather than by one actor: an actor dies
    /// with its loop thread, and the debt outlives it, for the actor of the
    /// thread replacing it to pay.
    reapply_owed: Arc<AtomicBool>,
}

impl Default for Shared {
    fn default() -> Self {
        Self::new()
    }
}

impl Shared {
    /// Generation zero, and no confirmation due.
    pub(crate) fn new() -> Self {
        let (confirmation_due, _) = watch::channel(None);
        let (routing_requests, _) = watch::channel(());
        Self {
            generation: Arc::default(),
            confirmation_due: Arc::new(confirmation_due),
            routing_requests: Arc::new(routing_requests),
            reapply_owed: Arc::default(),
        }
    }

    /// The current routing generation.
    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Advance the routing generation by one, and return the new value.
    pub(crate) fn advance_generation(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Ask the background applier to route the graph to the current
    /// selection (#152): the generation moves, then the applier is woken.
    /// Never waits, and needs no handle: the loop thread pays an owed
    /// re-apply through it. The generation moves before the applier is
    /// woken, so the pass this wake starts stamps its selection with a
    /// generation that already counts this request.
    pub(crate) fn request_routing(&self) {
        self.advance_generation();
        self.routing_requests.send_replace(());
    }

    /// A receiver of [`Self::request_routing`] wakes, for the applier. Only
    /// the requests made after this call wake it.
    pub(crate) fn routing_requests(&self) -> watch::Receiver<()> {
        self.routing_requests.subscribe()
    }

    /// A routing message ended `Unanswered`: one re-apply is owed (#152).
    fn owe_reapply(&self) {
        self.reapply_owed.store(true, Ordering::SeqCst);
    }

    /// Clear the owed re-apply; answers whether one was owed.
    fn take_reapply(&self) -> bool {
        self.reapply_owed.swap(false, Ordering::SeqCst)
    }

    /// A receiver of the published confirmation due time.
    pub(crate) fn confirmation_due(&self) -> watch::Receiver<Option<Instant>> {
        self.confirmation_due.subscribe()
    }

    /// Publish `due` as the earliest confirmation due time. A due time left
    /// in place wakes nobody: the confirmation timer waits on a change, and
    /// the same instant published again after an unrelated message is none.
    pub(crate) fn publish_confirmation_due(&self, due: Option<Instant>) {
        self.confirmation_due.send_if_modified(|published| {
            let moved = *published != due;
            *published = due;
            moved
        });
    }
}

/// The messages waiting for the actor, in the order they were sent.
#[derive(Default)]
pub(crate) struct Queue {
    waiting: VecDeque<Envelope>,
}

impl Queue {
    /// An empty queue.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Queue `envelope` behind every message already waiting.
    pub(crate) fn push(&mut self, envelope: Envelope) {
        self.waiting.push_back(envelope);
    }

    /// How many messages are waiting.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.waiting.len()
    }

    /// Whether no message is waiting.
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.waiting.is_empty()
    }

    /// The next message to run at `now`, `generation` being the current
    /// routing generation; `None` once nothing is left to run.
    ///
    /// The messages are taken out in the order they were sent. One that is
    /// not to run is answered on the way and the next is looked at, the
    /// checks running in this order: its reply is closed (dropped, no
    /// answer); it carries a `start_by` that `now` is past
    /// ([`AudioError::Expired`]); it is a volume set whose every sink a later
    /// queued set, its reply still open, names
    /// ([`RouterError::Superseded`]); it is an apply-selection stamped with a
    /// generation older than `generation` ([`RouterError::Outdated`]).
    ///
    /// This is the one place the next message is chosen.
    pub(crate) fn take_next(&mut self, now: Instant, generation: u64) -> Option<Message> {
        while let Some(Envelope { start_by, message }) = self.waiting.pop_front() {
            if message.caller_left() {
                continue;
            }
            if let Some(late) = start_by.and_then(|start_by| late_by(now, start_by)) {
                let line = expiry_line(&message, late);
                message.refuse(RouterError::Audio(AudioError::Expired));
                tracing::warn!("{line}");
                continue;
            }
            if self.is_superseded(&message) {
                message.refuse(RouterError::Superseded);
                continue;
            }
            if is_outdated(&message, generation) {
                message.refuse(RouterError::Outdated);
                continue;
            }
            return Some(message);
        }
        None
    }

    /// Whether `message` is a volume set that a set still waiting replaces:
    /// one whose reply is open and which names every sink `message` names.
    ///
    /// A set naming no sink is never replaced: "every sink of none" holds of
    /// any later set, so the empty list is guarded here rather than left to
    /// the containment test.
    fn is_superseded(&self, message: &Message) -> bool {
        let Message::SetSinkVolumes { macs, .. } = message else {
            return false;
        };
        if macs.is_empty() {
            return false;
        }
        self.waiting.iter().any(|later| match &later.message {
            Message::SetSinkVolumes {
                macs: later_macs,
                reply,
                ..
            } => !reply.is_closed() && macs.iter().all(|mac| later_macs.contains(mac)),
            _ => false,
        })
    }

    /// Hand `answer`, the answer of the volume read for `macs` that just ran,
    /// to every queued read for the same speakers in the same order, and take
    /// those reads out of the queue — whatever their `start_by`: the read they
    /// get is newer than their request. One whose reply is closed is dropped.
    /// Every other message stays where it is.
    pub(crate) fn answer_duplicate_reads(
        &mut self,
        macs: &[String],
        answer: &Result<Vec<Option<f32>>, RouterError>,
    ) {
        for envelope in std::mem::take(&mut self.waiting) {
            match envelope.message {
                Message::SinkVolumes { macs: asked, reply } if asked.as_slice() == macs => {
                    // Cloned: the one answer goes to every caller that asked
                    // for it.
                    let _ = reply.send(answer.clone());
                },
                message => self.waiting.push_back(Envelope {
                    start_by: envelope.start_by,
                    message,
                }),
            }
        }
    }
}

/// The line logged for `message`, taken out of the queue `late` past its
/// `start_by`: the message is named by its variant alone, as #146 names a
/// command — its arguments are not logged.
fn expiry_line(message: &Message, late: std::time::Duration) -> String {
    format!(
        "router message {} expired: taken out of the queue {} ms past its start_by",
        message.name(),
        late.as_millis()
    )
}

/// How far past `start_by` the instant `now` is; `None` while it is not
/// strictly past it.
fn late_by(now: Instant, start_by: Instant) -> Option<std::time::Duration> {
    (now > start_by).then(|| now.duration_since(start_by))
}

/// Whether `message` is a selection stamped with a routing generation older
/// than `generation`: the selection changed after it was read.
fn is_outdated(message: &Message, generation: u64) -> bool {
    matches!(message, Message::ApplySelection { generation: stamped, .. } if *stamped < generation)
}

/// The router and what it shares with its handle: what the graph thread owns.
///
/// Generic over the router's graph, as [`AudioRouter`] is: the loop thread's
/// actor owns the loop's own state, and the default is the actor that can be
/// moved to another thread.
pub(crate) struct Actor<G: Graph + ?Sized = dyn Graph + Send> {
    router: AudioRouter<G>,
    shared: Shared,
}

impl<G: Graph + ?Sized> Actor<G> {
    /// An actor over `router`. Publishes the router's confirmation due time
    /// at once, so a due time an earlier actor published is not left behind.
    pub(crate) fn new(router: AudioRouter<G>, shared: Shared) -> Self {
        let actor = Self { router, shared };
        actor.publish_confirmation_due();
        actor
    }

    /// The graph the router owns, for the thread that runs this actor.
    pub(crate) fn graph_mut(&mut self) -> &mut G {
        self.router.graph_mut()
    }

    /// The graph answers again (#152) — the late `done` of a stalled sync
    /// arrived, or a lost connection is back: pay an owed re-apply with one
    /// [`Shared::request_routing`], and clear the debt. Nothing owed,
    /// nothing published.
    pub(crate) fn graph_answers_again(&mut self) {
        if self.shared.take_reapply() {
            tracing::info!("the audio graph answers again: re-applying the routing");
            self.shared.request_routing();
        }
    }

    /// Record a re-apply owed when a routing message's `result` is a stall
    /// (#152). An answered failure owes nothing: re-sending gets the same
    /// answer.
    fn note_routing<T>(&self, result: &Result<T, AudioError>) {
        if matches!(result, Err(AudioError::Unanswered)) {
            self.shared.owe_reapply();
        }
    }

    /// Publish the router's earliest confirmation due time.
    fn publish_confirmation_due(&self) {
        self.shared
            .publish_confirmation_due(self.router.next_confirmation_due());
    }

    /// Take the next message out of `queue` at `now` and run it whole:
    /// one deadline for all its graph calls, `COMMAND_TIMEOUT` after `now`;
    /// its answer on its own reply, and on the reply of every queued duplicate
    /// of a volume read; then the router's earliest confirmation due time,
    /// published. Answers whether a message ran: `false` means the queue holds
    /// nothing more to run.
    ///
    /// `queue` is not borrowed while the message runs.
    pub(crate) fn run_next(&mut self, queue: &RefCell<Queue>, now: Instant) -> bool {
        let next = queue.borrow_mut().take_next(now, self.shared.generation());
        let Some(message) = next else {
            return false;
        };
        self.router.set_deadline(now + COMMAND_TIMEOUT);
        self.run(message, queue);
        self.publish_confirmation_due();
        true
    }

    /// Run `message` against the router and answer it. `queue` is borrowed
    /// only once a volume read has run, to answer its duplicates.
    fn run(&mut self, message: Message, queue: &RefCell<Queue>) {
        match message {
            Message::Route { speakers, reply } => {
                let routed = self.router.route_for_targets(&speakers);
                self.note_routing(&routed);
                answer(reply, routed);
            },
            Message::SinkVolumes { macs, reply } => {
                let levels = self.router.sink_volumes(&macs).map_err(RouterError::from);
                queue.borrow_mut().answer_duplicate_reads(&macs, &levels);
                answer(reply, levels);
            },
            Message::SetSinkVolumes { macs, level, reply } => {
                let set = macs
                    .iter()
                    .try_for_each(|mac| self.router.set_sink_volume(mac, level));
                answer(reply, set);
            },
            Message::Retune {
                sink_name,
                branch,
                reply,
            } => {
                let retuned = self.retune(&sink_name, &branch);
                self.note_routing(&retuned);
                answer(reply, retuned);
            },
            Message::RouteForSpotify { speakers, reply } => {
                let routed = self.route_for_spotify(&speakers);
                self.note_routing(&routed);
                answer(reply, routed);
            },
            Message::ApplySelection {
                speakers, reply, ..
            } => {
                let applied = if speakers.is_empty() {
                    self.router.teardown(COMBINED_SINK_NAME)
                } else {
                    self.router.route_for_targets(&speakers)
                };
                self.note_routing(&applied);
                answer(reply, applied);
            },
            Message::Repair { speakers, reply } => {
                let before = self.router.graph_changes();
                let routed = self.router.route_for_targets(&speakers);
                self.note_routing(&routed);
                let outcome = RepairOutcome {
                    routed,
                    changed: self.router.graph_changes() != before,
                    retarget_failed: self.router.last_retarget_failed(),
                };
                // A pass nobody waits for any more has still run.
                let _ = reply.send(Ok(outcome));
            },
        }
    }

    /// Retune `branch` in place inside `sink_name`; nothing to do while that
    /// sink is not loaded. A sink list that cannot be read is that error,
    /// not an absent sink.
    fn retune(&mut self, sink_name: &str, branch: &CombineBranch) -> Result<(), AudioError> {
        if self.router.combined_sink_exists(sink_name)? {
            self.router.retune_branch(sink_name, branch)?;
        }
        Ok(())
    }

    /// Route the graph to `speakers`, then resolve the node `librespot` is to
    /// be pointed at. The spawn itself stays with the caller, off this thread
    /// (#122).
    fn route_for_spotify(&mut self, speakers: &[SpeakerTarget]) -> Result<String, AudioError> {
        self.router.route_for_targets(speakers)?;
        self.router
            .resolve_target_sink(&spotify_target_sink(speakers))
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! What the tests of the handle and of the handlers send their messages
    //! through: a closure standing for a transport, and an actor inside the
    //! tokio runtime over a [`FakeGraph`].

    use super::{Actor, Envelope, Message, Queue, Shared, Transport};
    use crate::audio::{AudioError, AudioRouter};
    use crate::graph::fake::FakeGraph;
    use std::cell::RefCell;
    use std::sync::Arc;
    use std::time::Instant;
    use tokio::sync::{mpsc, watch};

    /// A closure is a transport: it is handed every envelope sent.
    impl<F> Transport for F
    where
        F: FnMut(Envelope) -> Result<(), AudioError> + Send,
    {
        fn send(&mut self, envelope: Envelope) -> Result<(), AudioError> {
            self(envelope)
        }
    }

    /// `message` on one line: its variant, then its arguments in declaration
    /// order. What a test compares to tell which message reached where.
    pub(crate) fn describe(message: &Message) -> String {
        fn speakers_of(speakers: &[blue2th_proto::SpeakerTarget]) -> String {
            speakers
                .iter()
                .map(|s| format!("{}@{}", s.address, s.offset_ms))
                .collect::<Vec<_>>()
                .join(",")
        }
        match message {
            Message::Route { speakers, .. } => format!("Route [{}]", speakers_of(speakers)),
            Message::SinkVolumes { macs, .. } => format!("SinkVolumes [{}]", macs.join(",")),
            Message::SetSinkVolumes { macs, level, .. } => {
                format!("SetSinkVolumes [{}] {level}", macs.join(","))
            },
            Message::Retune {
                sink_name, branch, ..
            } => format!("Retune {sink_name} {} {}", branch.sink, branch.latency_ms),
            Message::RouteForSpotify { speakers, .. } => {
                format!("RouteForSpotify [{}]", speakers_of(speakers))
            },
            Message::ApplySelection {
                speakers,
                generation,
                ..
            } => format!("ApplySelection [{}] {generation}", speakers_of(speakers)),
            Message::Repair { speakers, .. } => format!("Repair [{}]", speakers_of(speakers)),
        }
    }

    /// Whether the caller of `message` stopped waiting for its answer: the
    /// queue's own reading, so a test and the queue cannot disagree on it.
    pub(crate) fn reply_is_closed(message: &Message) -> bool {
        message.caller_left()
    }

    /// The clock a fake actor's router reads.
    pub(crate) type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

    /// Holds a [`FakeActor`]: while one of these is alive the actor starts no
    /// message. What a test taking the router's lock used to do.
    pub(crate) struct ActorHold {
        holds: Arc<watch::Sender<usize>>,
    }

    impl Drop for ActorHold {
        fn drop(&mut self) {
            self.holds
                .send_modify(|holds| *holds = holds.saturating_sub(1));
        }
    }

    /// The means to hold a [`FakeActor`] from outside the handle that owns it.
    #[derive(Clone)]
    pub(crate) struct ActorHolder {
        holds: Arc<watch::Sender<usize>>,
    }

    impl ActorHolder {
        /// Hold the actor until the value returned is dropped.
        pub(crate) fn hold(&self) -> ActorHold {
            self.holds.send_modify(|holds| *holds += 1);
            ActorHold {
                holds: Arc::clone(&self.holds),
            }
        }
    }

    /// An actor inside the tokio runtime, over a [`FakeGraph`].
    ///
    /// Started on the first message, as the loop thread is. A real thread
    /// cannot stand in for it under tokio's paused clock: auto-advance would
    /// fire the handle's timeout while the thread is still working.
    ///
    /// Each message is taken out at tokio's `now`, read once for it, after
    /// everything sent so far was queued: what was sent behind a hold is all
    /// in the queue when the first of them is looked at.
    pub(crate) struct FakeActor {
        graph: FakeGraph,
        clock: Clock,
        shared: Shared,
        holds: Arc<watch::Sender<usize>>,
        inbox: Option<mpsc::UnboundedSender<Envelope>>,
    }

    impl FakeActor {
        /// An actor over `graph`, whose router reads `clock`. Starts nothing.
        pub(crate) fn new(graph: &FakeGraph, clock: Clock, shared: Shared) -> Self {
            let (holds, _) = watch::channel(0);
            Self {
                // A clone of the fake is a handle onto the same state.
                graph: graph.clone(),
                clock,
                shared,
                holds: Arc::new(holds),
                inbox: None,
            }
        }

        /// The means to hold this actor.
        pub(crate) fn holder(&self) -> ActorHolder {
            ActorHolder {
                holds: Arc::clone(&self.holds),
            }
        }

        /// Start the actor's task, over a router of its own.
        fn start(&self) -> mpsc::UnboundedSender<Envelope> {
            let (inbox, received) = mpsc::unbounded_channel();
            let clock = Arc::clone(&self.clock);
            let router =
                AudioRouter::with_clock(Box::new(self.graph.clone()), Box::new(move || clock()));
            // Cloned: every actor started shares what the handle holds.
            let actor = Actor::new(router, self.shared.clone());
            tokio::spawn(run(actor, received, self.holds.subscribe()));
            inbox
        }
    }

    impl Transport for FakeActor {
        fn send(&mut self, envelope: Envelope) -> Result<(), AudioError> {
            let inbox = match self.inbox.take() {
                Some(inbox) if !inbox.is_closed() => inbox,
                _ => self.start(),
            };
            let sent = inbox
                .send(envelope)
                .map_err(|_| AudioError::PipeWire("the fake actor is not running".into()));
            self.inbox = Some(inbox);
            sent
        }
    }

    /// The actor's task: queue what arrives, and run it unless held.
    async fn run(
        mut actor: Actor,
        mut inbox: mpsc::UnboundedReceiver<Envelope>,
        mut holds: watch::Receiver<usize>,
    ) {
        let queue = RefCell::new(Queue::new());
        while let Some(first) = inbox.recv().await {
            queue.borrow_mut().push(first);
            loop {
                while *holds.borrow_and_update() > 0 {
                    if holds.changed().await.is_err() {
                        return;
                    }
                }
                while let Ok(envelope) = inbox.try_recv() {
                    queue.borrow_mut().push(envelope);
                }
                let now = tokio::time::Instant::now().into_std();
                if !actor.run_next(&queue, now) {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
