// SPDX-License-Identifier: MIT OR Apache-2.0

//! The one way into the audio router (#145, #147).
//!
//! The router lives in the graph thread, which runs it one message at a time
//! (see [`crate::router_actor`]). A [`RouterHandle`] holds nothing of it: each
//! operation is one message sent through a [`Transport`], answered on a
//! `oneshot`, so a call that waits costs a suspended task and no thread.
//!
//! A request waits at most [`REQUEST_BOUND`], and its message carries the
//! instant past which the thread no longer starts it: while the PipeWire
//! daemon does not answer, a request neither queues for as long as the freeze
//! lasts nor succeeds late with nothing telling the user anything was wrong.
//! Background tasks send without a start deadline and wait without a bound —
//! a repair delayed is better than a repair dropped — and a selection change
//! hands its routing to the single background applier through
//! [`RouterHandle::request_routing`], so it is never lost to the bound.

use std::{
    fmt,
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

use blue2th_proto::SpeakerTarget;
use tokio::sync::{oneshot, watch};

use crate::audio::{AudioError, CombineBranch};
use crate::graph_pw::{PipeWireGraph, COMMAND_TIMEOUT, REPLY_MARGIN, START_BUDGET};
use crate::router_actor::{Envelope, Message, RepairOutcome, Reply, Shared, Transport};

/// How long a request waits for the answer to its message before giving up
/// (#147): the time the graph thread has to start it, the time the message's
/// graph calls may take, and the margin that lets the answer of a message
/// started at the last instant reach a caller that is still waiting.
pub const REQUEST_BOUND: Duration = START_BUDGET
    .saturating_add(COMMAND_TIMEOUT)
    .saturating_add(REPLY_MARGIN);

/// Why a router operation produced no result.
// `Clone`: one volume read answers every caller queued for it.
#[derive(Debug, Clone)]
pub enum RouterError {
    /// No answer in time: nothing reaches the graph for the request
    /// afterwards.
    TimedOut,
    /// The operation ran and the graph call failed — refused, or left
    /// unanswered by the daemon until its deadline ([`AudioError::Unanswered`],
    /// #147) — or the graph thread did not start it in time
    /// ([`AudioError::Expired`]).
    Audio(AudioError),
    /// A volume set that a later one, queued behind it for the same sinks,
    /// replaced before it started (#147): nothing was sent to the graph for
    /// it, and the later one's level is the one applied.
    Superseded,
    /// A selection the routing applier sent, stamped with a routing
    /// generation older than the one current when it reached the head of the
    /// queue (#147): nothing was sent to the graph for it.
    Outdated,
}

impl fmt::Display for RouterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RouterError::TimedOut => write!(f, "the audio router did not answer in time"),
            RouterError::Audio(err) => write!(f, "{err}"),
            RouterError::Superseded => {
                write!(f, "the volume set was superseded by a later one")
            },
            RouterError::Outdated => {
                write!(f, "the selection changed before its routing started")
            },
        }
    }
}

impl std::error::Error for RouterError {}

impl From<AudioError> for RouterError {
    fn from(err: AudioError) -> Self {
        RouterError::Audio(err)
    }
}

/// A cloneable handle onto the audio router the graph thread owns.
#[derive(Clone)]
pub struct RouterHandle {
    /// The way to whatever runs the actor. The lock is held for the send
    /// alone, which never blocks: no caller waits for an answer under it.
    transport: Arc<Mutex<Box<dyn Transport>>>,
    /// Wakes the background routing applier. A `watch` rather than a queue:
    /// every request made before the applier marks it seen folds into one
    /// pass, which reads the selection current at that moment.
    routing_requests: Arc<watch::Sender<()>>,
    /// The routing generation and the confirmation due time, shared with
    /// every actor started for this handle (#147).
    shared: Shared,
    /// The means to hold the actor, when it is a fake one.
    #[cfg(test)]
    holder: Option<crate::router_actor::testing::ActorHolder>,
}

impl RouterHandle {
    /// A handle sending through `transport`, to actors sharing `shared`
    /// (#147).
    pub(crate) fn over(transport: Box<dyn Transport>, shared: Shared) -> Self {
        let (routing_requests, _) = watch::channel(());
        Self {
            transport: Arc::new(Mutex::new(transport)),
            routing_requests: Arc::new(routing_requests),
            shared,
            #[cfg(test)]
            holder: None,
        }
    }

    /// A handle onto the router `graph`'s loop thread owns.
    pub fn over_graph(graph: PipeWireGraph) -> Self {
        let shared = graph.shared();
        Self::over(Box::new(graph), shared)
    }

    /// Ask the background applier to route the graph to the current selection.
    /// Never waits. The generation moves before the applier is woken, so the
    /// pass this wake starts stamps its selection with a generation that
    /// already counts this request.
    pub fn request_routing(&self) {
        self.shared.advance_generation();
        self.routing_requests.send_replace(());
    }

    /// A receiver of [`Self::request_routing`] wakes, for the applier. Only
    /// the requests made after this call wake it.
    pub fn routing_requests(&self) -> watch::Receiver<()> {
        self.routing_requests.subscribe()
    }

    /// The current routing generation (#147): what the applier stamps the
    /// selection it sends with, read before it reads that selection.
    /// [`Self::request_routing`] advances it.
    pub fn routing_generation(&self) -> u64 {
        self.shared.generation()
    }

    /// The earliest instant a confirming reload falls due (#147), as the
    /// actor published it after its last message; `None` when none is armed.
    pub fn confirmation_due(&self) -> watch::Receiver<Option<Instant>> {
        self.shared.confirmation_due()
    }

    /// Send the message `make` builds around a fresh reply, to be started by
    /// `start_by`, and hand back the end its answer arrives on.
    fn send<T>(
        &self,
        start_by: Option<Instant>,
        make: impl FnOnce(Reply<T>) -> Message,
    ) -> Result<oneshot::Receiver<Result<T, RouterError>>, RouterError> {
        let (reply, answer) = oneshot::channel();
        let envelope = Envelope {
            start_by,
            message: make(reply),
        };
        // A poisoned lock only says an earlier send panicked: the transport
        // starts a new actor when the previous one has died.
        self.transport
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .send(envelope)?;
        Ok(answer)
    }

    /// Send a request: stamped with the start deadline, and waited for
    /// [`REQUEST_BOUND`] at most. Both run from the one instant read before
    /// the send, so a graph thread that was slow to start shortens what is
    /// left of the wait instead of lengthening it. Giving up drops the reply's
    /// receiving end, which is what lets the actor skip the message when it
    /// reaches it.
    async fn request<T>(&self, make: impl FnOnce(Reply<T>) -> Message) -> Result<T, RouterError> {
        let sent_at = tokio::time::Instant::now();
        let answer = self.send(Some(sent_at.into_std() + START_BUDGET), make)?;
        match tokio::time::timeout_at(sent_at + REQUEST_BOUND, answer).await {
            Ok(answered) => answered.unwrap_or_else(|_| Err(dropped_reply())),
            Err(_) => Err(RouterError::TimedOut),
        }
    }

    /// Send a background message: no start deadline, and no bound on the
    /// wait.
    async fn in_background<T>(
        &self,
        make: impl FnOnce(Reply<T>) -> Message,
    ) -> Result<T, RouterError> {
        let answer = self.send(None, make)?;
        answer.await.unwrap_or_else(|_| Err(dropped_reply()))
    }

    /// Route the graph to `speakers`.
    pub async fn route(&self, speakers: &[SpeakerTarget]) -> Result<(), RouterError> {
        // Owned: the message leaves for the graph thread.
        let speakers = speakers.to_vec();
        self.request(|reply| Message::Route { speakers, reply })
            .await
    }

    /// The live volume of each speaker in `macs`, over one read of the sink
    /// list (see [`crate::audio::AudioRouter::sink_volumes`]).
    pub async fn sink_volumes(&self, macs: &[String]) -> Result<Vec<Option<f32>>, RouterError> {
        // Owned: the message leaves for the graph thread.
        let macs = macs.to_vec();
        self.request(|reply| Message::SinkVolumes { macs, reply })
            .await
    }

    /// Set every speaker in `macs` to `level`, stopping at the first failure.
    /// [`RouterError::Superseded`] when a later set for the same speakers
    /// replaced this one before it started.
    pub async fn set_sink_volumes(&self, macs: &[String], level: f32) -> Result<(), RouterError> {
        // Owned: the message leaves for the graph thread.
        let macs = macs.to_vec();
        self.request(|reply| Message::SetSinkVolumes { macs, level, reply })
            .await
    }

    /// Retune `branch` in place inside the combined sink `sink_name`; nothing
    /// to do while that sink is not loaded.
    pub async fn retune(&self, sink_name: &str, branch: &CombineBranch) -> Result<(), RouterError> {
        let sink_name = sink_name.to_string();
        // Cloned: the message leaves for the graph thread.
        let branch = branch.clone();
        self.request(|reply| Message::Retune {
            sink_name,
            branch,
            reply,
        })
        .await
    }

    /// Route the graph to `speakers` and answer the node name `librespot` is
    /// to be pointed at (#147). A request: bounded like [`Self::route`].
    pub async fn route_for_spotify(
        &self,
        speakers: &[SpeakerTarget],
    ) -> Result<String, RouterError> {
        // Owned: the message leaves for the graph thread.
        let speakers = speakers.to_vec();
        self.request(|reply| Message::RouteForSpotify { speakers, reply })
            .await
    }

    /// [`Self::route_for_spotify`] for the background routing applier (#147):
    /// no start deadline, no bound — a respawn delayed is better than a
    /// `librespot` left stopped.
    pub async fn route_for_spotify_in_background(
        &self,
        speakers: &[SpeakerTarget],
    ) -> Result<String, RouterError> {
        // Owned: the message leaves for the graph thread.
        let speakers = speakers.to_vec();
        self.in_background(|reply| Message::RouteForSpotify { speakers, reply })
            .await
    }

    /// Apply `speakers`, the selection read at routing generation
    /// `generation` (#147): tear the combined sink down when it is empty,
    /// route otherwise. [`RouterError::Outdated`] when the generation moved on
    /// before the message started. A background call: no start deadline, no
    /// bound.
    pub async fn apply_selection(
        &self,
        speakers: &[SpeakerTarget],
        generation: u64,
    ) -> Result<(), RouterError> {
        // Owned: the message leaves for the graph thread.
        let speakers = speakers.to_vec();
        self.in_background(|reply| Message::ApplySelection {
            speakers,
            generation,
            reply,
        })
        .await
    }

    /// One repair pass over `speakers` (#147). A background call: no start
    /// deadline, no bound.
    pub async fn repair(&self, speakers: &[SpeakerTarget]) -> Result<RepairOutcome, RouterError> {
        // Owned: the message leaves for the graph thread.
        let speakers = speakers.to_vec();
        self.in_background(|reply| Message::Repair { speakers, reply })
            .await
    }
}

/// What a caller is answered when its reply was dropped unanswered: the actor
/// died with the message in its queue. Not [`AudioError::Expired`]: nothing
/// says the message did not run.
fn dropped_reply() -> RouterError {
    RouterError::Audio(AudioError::PipeWire(
        "the PipeWire graph thread dropped the message without answering".into(),
    ))
}

#[cfg(test)]
impl RouterHandle {
    /// A handle over a [`crate::router_actor::testing::FakeActor`] on `fake`,
    /// whose router reads the system clock.
    pub(crate) fn over_fake(fake: &crate::graph::fake::FakeGraph) -> Self {
        Self::over_fake_with_clock(fake, Arc::new(Instant::now))
    }

    /// A handle over a fake actor on `fake`, whose router reads `clock`.
    pub(crate) fn over_fake_with_clock(
        fake: &crate::graph::fake::FakeGraph,
        clock: crate::router_actor::testing::Clock,
    ) -> Self {
        let shared = Shared::new();
        // Cloned: the actor shares what the handle reads.
        let actor = crate::router_actor::testing::FakeActor::new(fake, clock, shared.clone());
        let holder = actor.holder();
        let mut handle = Self::over(Box::new(actor), shared);
        handle.holder = Some(holder);
        handle
    }

    /// Hold the fake actor until the value returned is dropped: it starts no
    /// message meanwhile, and what is sent queues behind the hold. `None` for
    /// a handle that is not over a fake actor.
    pub(crate) fn hold_actor(&self) -> Option<crate::router_actor::testing::ActorHold> {
        self.holder.as_ref().map(|holder| holder.hold())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::bluez_sink_prefix;
    use crate::graph::fake::{FakeGraph, GraphCall, GraphOp};
    use crate::graph_pw::{COMMAND_TIMEOUT, REPLY_MARGIN, START_BUDGET};
    use crate::router_actor::testing::{describe, reply_is_closed};
    use crate::router_actor::{Envelope, Message, RepairOutcome, Shared};
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Mutex;

    const JBL: &str = "2C:FD:B4:D3:AC:21";
    const JBL_SINK: &str = "bluez_output.2C_FD_B4_D3_AC_21.1";
    const SONY: &str = "80:99:E7:63:50:29";
    const SONY_SINK: &str = "bluez_output.80_99_E7_63_50_29.1";
    const COMBINED: &str = "blue2th_combined";

    fn target(mac: &str, offset_ms: u32) -> SpeakerTarget {
        SpeakerTarget {
            address: mac.to_string(),
            offset_ms,
        }
    }

    fn macs(list: &[&str]) -> Vec<String> {
        list.iter().map(|mac| mac.to_string()).collect()
    }

    /// The Sony's branch at `latency_ms`, as `apply_offset_live` builds it.
    fn sony_branch(latency_ms: u32) -> CombineBranch {
        CombineBranch {
            sink: bluez_sink_prefix(SONY),
            latency_ms,
        }
    }

    /// Let every task that is ready run, without moving the paused clock.
    async fn settle() {
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
    }

    /// What tokio's clock reads, as the `Instant` an envelope is stamped with.
    fn now() -> Instant {
        tokio::time::Instant::now().into_std()
    }

    /// The envelopes a parking transport was handed, in order.
    type Parked = Arc<Mutex<Vec<Envelope>>>;

    /// A handle whose transport keeps every envelope and never answers: an
    /// actor that is there and stuck.
    fn parking() -> (RouterHandle, Parked) {
        let parked: Parked = Arc::default();
        let keep = Arc::clone(&parked);
        let handle = RouterHandle::over(
            Box::new(move |envelope: Envelope| {
                keep.lock().unwrap().push(envelope);
                Ok(())
            }),
            Shared::new(),
        );
        (handle, parked)
    }

    /// A handle whose transport takes every envelope and drops it unanswered,
    /// reply included: an actor that died with the message in its queue.
    fn dropping() -> RouterHandle {
        RouterHandle::over(
            Box::new(|envelope: Envelope| {
                drop(envelope);
                Ok(())
            }),
            Shared::new(),
        )
    }

    /// A handle whose transport has no actor at all, as a detached graph.
    fn refusing() -> RouterHandle {
        RouterHandle::over(
            Box::new(|_envelope: Envelope| {
                Err(AudioError::PipeWire(
                    "the PipeWire graph thread is not running".into(),
                ))
            }),
            Shared::new(),
        )
    }

    /// One call on a handle, its answer reduced to whether it failed and how.
    type Call = fn(RouterHandle) -> Pin<Box<dyn Future<Output = Result<(), RouterError>> + Send>>;

    /// The five request-path calls, each with distinct arguments, and the
    /// message each one is: `(method, call, message)`.
    fn request_calls() -> Vec<(&'static str, Call, String)> {
        vec![
            (
                "route",
                |handle| Box::pin(async move { handle.route(&[target(JBL, 40)]).await }),
                format!("Route [{JBL}@40]"),
            ),
            (
                "sink_volumes",
                |handle| {
                    Box::pin(
                        async move { handle.sink_volumes(&macs(&[SONY, JBL])).await.map(|_| ()) },
                    )
                },
                format!("SinkVolumes [{SONY},{JBL}]"),
            ),
            (
                "set_sink_volumes",
                |handle| {
                    Box::pin(async move { handle.set_sink_volumes(&macs(&[JBL, SONY]), 0.6).await })
                },
                format!("SetSinkVolumes [{JBL},{SONY}] 0.6"),
            ),
            (
                "retune",
                |handle| Box::pin(async move { handle.retune(COMBINED, &sony_branch(120)).await }),
                format!("Retune {COMBINED} bluez_output.80_99_E7_63_50_29 120"),
            ),
            (
                "route_for_spotify",
                |handle| {
                    Box::pin(async move {
                        handle
                            .route_for_spotify(&[target(SONY, 70)])
                            .await
                            .map(|_| ())
                    })
                },
                format!("RouteForSpotify [{SONY}@70]"),
            ),
        ]
    }

    /// The three background calls, likewise.
    fn background_calls() -> Vec<(&'static str, Call, String)> {
        vec![
            (
                "apply_selection",
                |handle| {
                    Box::pin(async move {
                        handle
                            .apply_selection(&[target(JBL, 0), target(SONY, 90)], 7)
                            .await
                    })
                },
                format!("ApplySelection [{JBL}@0,{SONY}@90] 7"),
            ),
            (
                "repair",
                |handle| {
                    Box::pin(async move { handle.repair(&[target(SONY, 30)]).await.map(|_| ()) })
                },
                format!("Repair [{SONY}@30]"),
            ),
            (
                "route_for_spotify_in_background",
                |handle| {
                    Box::pin(async move {
                        handle
                            .route_for_spotify_in_background(&[target(JBL, 20)])
                            .await
                            .map(|_| ())
                    })
                },
                format!("RouteForSpotify [{JBL}@20]"),
            ),
        ]
    }

    // Criterion (#147, replaces `test_router_wait_is_two_seconds`): a
    // request-path call waits at most `START_BUDGET + COMMAND_TIMEOUT +
    // REPLY_MARGIN`, which is 2 s. The value lives here, in the test's name.
    // What the handle really waits is measured in
    // `test_every_request_call_gives_up_2_s_after_it_was_made_and_closes_its_reply`.
    #[test]
    fn test_request_bound_is_two_seconds() {
        assert_eq!(REQUEST_BOUND, Duration::from_secs(2));
        assert_eq!(REQUEST_BOUND, START_BUDGET + COMMAND_TIMEOUT + REPLY_MARGIN);
    }

    // Criterion: each request-path call sends one message — its own variant,
    // carrying its own arguments, given distinct values so a swap shows —
    // stamped with a `start_by` 300 ms after the send, on tokio's clock. The
    // calls are made a second apart, so a stamp taken once for the handle's
    // life, or off another clock, differs from the instant read here.
    #[tokio::test(start_paused = true)]
    async fn test_every_request_call_sends_its_own_message_stamped_300_ms_after_the_send() {
        let (handle, parked) = parking();
        let mut expected = Vec::new();
        let mut waiting = Vec::new();

        for (_, call, message) in request_calls() {
            tokio::time::advance(Duration::from_secs(1)).await;
            expected.push((message, Some(now() + Duration::from_millis(300))));
            // Cloned: each call runs on a task of its own.
            waiting.push(tokio::spawn(call(handle.clone())));
            settle().await;
        }

        let received: Vec<(String, Option<Instant>)> = parked
            .lock()
            .unwrap()
            .iter()
            .map(|envelope| (describe(&envelope.message), envelope.start_by))
            .collect();
        assert_eq!(received, expected);
    }

    // Criterion: each background call sends one message — its own variant
    // and arguments — carrying no `start_by`: a background message is
    // started however long it waited. `route_for_spotify_in_background`
    // sends the very message `route_for_spotify` does: the deadline is a
    // property of the send, not of the variant.
    #[tokio::test(start_paused = true)]
    async fn test_every_background_call_sends_its_own_message_without_a_start_by() {
        let (handle, parked) = parking();
        let mut expected = Vec::new();
        let mut waiting = Vec::new();

        for (_, call, message) in background_calls() {
            expected.push((message, None));
            // Cloned: each call runs on a task of its own.
            waiting.push(tokio::spawn(call(handle.clone())));
            settle().await;
        }

        let received: Vec<(String, Option<Instant>)> = parked
            .lock()
            .unwrap()
            .iter()
            .map(|envelope| (describe(&envelope.message), envelope.start_by))
            .collect();
        assert_eq!(received, expected);
    }

    // Criteria: a request-path call with no answer waits 2 s — not a
    // millisecond less — then answers `RouterError::TimedOut`; and dropping
    // the wait closes the reply, which is what lets the graph thread skip
    // the message when it reaches it. On each of the five request calls.
    #[tokio::test(start_paused = true)]
    async fn test_every_request_call_gives_up_2_s_after_it_was_made_and_closes_its_reply() {
        for (method, call, _) in request_calls() {
            let (handle, parked) = parking();

            let request = tokio::spawn(call(handle));
            settle().await;
            assert_eq!(parked.lock().unwrap().len(), 1, "{method} sent its message");
            assert!(
                !reply_is_closed(&parked.lock().unwrap()[0].message),
                "{method} waits for its answer"
            );
            tokio::time::advance(Duration::from_millis(1999)).await;
            settle().await;
            assert!(!request.is_finished(), "{method} waits the whole bound");
            tokio::time::advance(Duration::from_millis(1)).await;
            settle().await;
            assert!(request.is_finished(), "{method} gives up at the bound");

            let answer = request.await.expect("the call's task ends");
            assert!(
                matches!(answer, Err(RouterError::TimedOut)),
                "{method} answered {answer:?}"
            );
            assert!(
                reply_is_closed(&parked.lock().unwrap()[0].message),
                "{method} left its reply open after giving up"
            );
        }
    }

    /// Answer the one envelope `parked` holds as a healthy actor would.
    fn answer_the_parked_message(parked: &Parked) {
        let envelope = parked.lock().unwrap().pop();
        match envelope.map(|envelope| envelope.message) {
            Some(Message::ApplySelection { reply, .. }) => drop(reply.send(Ok(()))),
            Some(Message::Repair { reply, .. }) => drop(reply.send(Ok(RepairOutcome {
                routed: Ok(()),
                changed: true,
                retarget_failed: false,
            }))),
            Some(Message::RouteForSpotify { reply, .. }) => {
                drop(reply.send(Ok(COMBINED.to_string())));
            },
            _ => {},
        }
    }

    // Criterion (guard, a background call has no bound): each background
    // call is still waiting 5 s after it was made — past the request bound
    // whatever its value — with its reply open, and takes the answer when it
    // comes. The near miss is the wait's length: a handle applying the bound
    // everywhere gives up at 2 s.
    #[tokio::test(start_paused = true)]
    async fn test_every_background_call_still_waits_after_5_s_and_takes_the_answer_when_it_comes() {
        for (method, call, _) in background_calls() {
            let (handle, parked) = parking();

            let pass = tokio::spawn(call(handle));
            settle().await;
            tokio::time::advance(Duration::from_secs(5)).await;
            settle().await;
            assert!(!pass.is_finished(), "{method} is still waiting");
            assert_eq!(parked.lock().unwrap().len(), 1, "{method} sent its message");
            assert!(
                !reply_is_closed(&parked.lock().unwrap()[0].message),
                "{method} still waits for its answer"
            );

            answer_the_parked_message(&parked);
            settle().await;
            assert!(pass.is_finished(), "{method} took the answer");
            let answer = pass.await.expect("the call's task ends");
            assert!(answer.is_ok(), "{method} answered {answer:?}");
        }
    }

    // Criterion: a call hands back what the actor answered, as it is — the
    // levels of a read, the node name of a Spotify route, the three fields
    // of a repair — and the typed refusals: an expiry stays
    // `AudioError::Expired` (it maps to a 503), a superseded set stays
    // `Superseded` (it maps to a 200), an outdated selection stays
    // `Outdated`. The near miss is an answer passed through a catch-all into
    // another error.
    #[tokio::test(start_paused = true)]
    async fn test_every_call_hands_back_what_the_actor_answered() {
        let handle = RouterHandle::over(
            Box::new(|envelope: Envelope| {
                match envelope.message {
                    Message::Route { reply, .. } => drop(reply.send(Ok(()))),
                    Message::SinkVolumes { reply, .. } => {
                        drop(reply.send(Ok(vec![Some(0.4), None])));
                    },
                    Message::SetSinkVolumes { reply, .. } => {
                        drop(reply.send(Err(RouterError::Superseded)));
                    },
                    Message::Retune { reply, .. } => {
                        drop(reply.send(Err(RouterError::Audio(AudioError::Expired))));
                    },
                    Message::RouteForSpotify { reply, .. } => {
                        drop(reply.send(Ok("blue2th_combined.7".to_string())));
                    },
                    Message::ApplySelection { reply, .. } => {
                        drop(reply.send(Err(RouterError::Outdated)));
                    },
                    Message::Repair { reply, .. } => drop(reply.send(Ok(RepairOutcome {
                        routed: Err(AudioError::PipeWire("no daemon".to_string())),
                        changed: true,
                        retarget_failed: true,
                    }))),
                }
                Ok(())
            }),
            Shared::new(),
        );
        let jbl = [target(JBL, 0)];

        assert!(matches!(handle.route(&jbl).await, Ok(())));
        assert_eq!(
            handle.sink_volumes(&macs(&[JBL, SONY])).await.ok(),
            Some(vec![Some(0.4), None])
        );
        let superseded = handle.set_sink_volumes(&macs(&[JBL]), 0.3).await;
        assert!(
            matches!(superseded, Err(RouterError::Superseded)),
            "got {superseded:?}"
        );
        let expired = handle.retune(COMBINED, &sony_branch(120)).await;
        assert!(
            matches!(expired, Err(RouterError::Audio(AudioError::Expired))),
            "got {expired:?}"
        );
        assert_eq!(
            handle.route_for_spotify(&jbl).await.ok(),
            Some("blue2th_combined.7".to_string())
        );
        assert_eq!(
            handle.route_for_spotify_in_background(&jbl).await.ok(),
            Some("blue2th_combined.7".to_string())
        );
        let outdated = handle.apply_selection(&jbl, 0).await;
        assert!(
            matches!(outdated, Err(RouterError::Outdated)),
            "got {outdated:?}"
        );
        let repaired = handle.repair(&jbl).await;
        assert!(
            matches!(
                &repaired,
                Ok(RepairOutcome {
                    routed: Err(AudioError::PipeWire(m)),
                    changed: true,
                    retarget_failed: true
                }) if m == "no daemon"
            ),
            "got {repaired:?}"
        );
    }

    /// One call's method, its answer when it answered within the guard, and
    /// whether the clock moved while it ran.
    type Answered = (&'static str, Option<Result<(), RouterError>>, bool);

    /// Run every call of `calls` on a handle `make` builds, each under a
    /// minute's guard so a call that never answers fails rather than hangs.
    async fn answers_of(
        calls: Vec<(&'static str, Call, String)>,
        make: fn() -> RouterHandle,
    ) -> Vec<Answered> {
        let mut answers = Vec::new();
        for (method, call, _) in calls {
            let before = tokio::time::Instant::now();
            let answer = tokio::time::timeout(Duration::from_secs(60), call(make()))
                .await
                .ok();
            answers.push((method, answer, tokio::time::Instant::now() != before));
        }
        answers
    }

    // Criterion: a reply dropped without an answer is an error for its
    // caller, on both the request and the background path — at once: a
    // dropped reply is not a slow one, so the clock has not moved and the
    // answer is not `TimedOut`. It is not `Expired` either: nothing says
    // the message did not run. The near miss is the background path, which
    // has no timeout to fall back on: there, a dropped reply taken for a
    // slow one is an endless wait.
    #[tokio::test(start_paused = true)]
    async fn test_a_reply_dropped_without_an_answer_is_an_error_at_once_on_both_paths() {
        let mut calls = request_calls();
        calls.extend(background_calls());

        for (method, answer, clock_moved) in answers_of(calls, dropping).await {
            assert!(
                matches!(
                    answer,
                    Some(Err(RouterError::Audio(AudioError::PipeWire(_))))
                ),
                "{method} answered {answer:?}"
            );
            assert!(!clock_moved, "{method} waited before answering");
        }
    }

    // Criterion (non-nominal): with no graph thread at all every message
    // errs at once, with the transport's own error, on both paths — no bound
    // is waited out.
    #[tokio::test(start_paused = true)]
    async fn test_every_call_errs_at_once_when_there_is_no_actor_at_all() {
        let mut calls = request_calls();
        calls.extend(background_calls());

        for (method, answer, clock_moved) in answers_of(calls, refusing).await {
            assert!(
                matches!(
                    &answer,
                    Some(Err(RouterError::Audio(AudioError::PipeWire(m)))) if m.contains("not running")
                ),
                "{method} answered {answer:?}"
            );
            assert!(!clock_moved, "{method} waited before answering");
        }
    }

    // Criterion: a waiting call suspends. On a single-threaded runtime, with
    // an actor that does not answer, a task started after the call still
    // runs to its end while the call waits — a call blocking its thread
    // would have finished, on its own timeout, before that task ever ran.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_a_waiting_call_suspends_and_leaves_the_only_thread_to_other_tasks() {
        let (handle, parked) = parking();

        let waiting = tokio::spawn(async move { handle.route(&[target(JBL, 0)]).await });
        let unrelated = tokio::spawn(async { 7 });
        settle().await;

        assert!(unrelated.is_finished(), "the other task ran");
        assert_eq!(unrelated.await.ok(), Some(7));
        assert_eq!(parked.lock().unwrap().len(), 1, "the call sent its message");
        assert!(!waiting.is_finished(), "and the call is still waiting");
    }

    // Criterion: `request_routing()` advances the routing generation, by one
    // each time, and wakes the applier; it never waits — it is not even
    // `async` — and sends the actor nothing.
    #[tokio::test]
    async fn test_request_routing_advances_the_generation_and_wakes_the_applier_without_a_message()
    {
        let (handle, parked) = parking();
        let mut wakes = handle.routing_requests();
        let before = handle.routing_generation();
        assert_eq!(wakes.has_changed().ok(), Some(false));

        handle.request_routing();

        assert_eq!(handle.routing_generation(), before + 1);
        assert_eq!(wakes.has_changed().ok(), Some(true));
        wakes.borrow_and_update();

        handle.request_routing();
        handle.request_routing();

        assert_eq!(handle.routing_generation(), before + 3);
        assert_eq!(wakes.has_changed().ok(), Some(true));
        assert!(parked.lock().unwrap().is_empty());
    }

    // Criterion: a freshly built handle publishes no confirmation due time.
    #[test]
    fn test_confirmation_due_of_a_fresh_handle_is_none() {
        let (handle, _) = parking();

        assert_eq!(*handle.confirmation_due().borrow(), None);
    }

    // ─── Over a fake actor: the handle, the queue and the router together ───

    /// A graph with the JBL's and the Sony's sinks, at distinct levels.
    fn two_speakers() -> FakeGraph {
        let fake = FakeGraph::with_sinks(&[JBL_SINK, SONY_SINK]);
        fake.set_volume(JBL_SINK, 0.4);
        fake.set_volume(SONY_SINK, 0.7);
        fake
    }

    /// How many times `fake` was asked for its sink list.
    fn sink_list_reads(fake: &FakeGraph) -> usize {
        fake.all_calls()
            .iter()
            .filter(|call| matches!(call, GraphCall::Sinks))
            .count()
    }

    /// How many times `fake` was asked to create the combined sink.
    fn rebuilds(fake: &FakeGraph) -> usize {
        fake.calls()
            .iter()
            .filter(|call| matches!(call, GraphCall::CreateCombinedSink { .. }))
            .count()
    }

    // Criterion: over a free actor each call runs its message against the
    // graph and answers its result — nothing is held, nothing is waited for.
    #[tokio::test(start_paused = true)]
    async fn test_calls_over_a_free_actor_run_against_the_graph_and_answer() {
        let fake = two_speakers();
        let handle = RouterHandle::over_fake(&fake);
        let before = tokio::time::Instant::now();

        assert!(matches!(handle.route(&[target(JBL, 40)]).await, Ok(())));
        assert_eq!(
            fake.routing_calls(),
            vec![
                GraphCall::ClearStaleDefaultSink {
                    sink_name: COMBINED.to_string()
                },
                GraphCall::Teardown {
                    sink_name: COMBINED.to_string()
                },
                GraphCall::CreateCombinedSink {
                    sink_name: COMBINED.to_string()
                },
                GraphCall::LoadBranch {
                    sink_name: COMBINED.to_string(),
                    real_sink: JBL_SINK.to_string(),
                    latency_ms: 40
                },
            ]
        );
        assert_eq!(
            handle.sink_volumes(&macs(&[SONY, JBL])).await.ok(),
            Some(vec![Some(0.7), Some(0.4)])
        );
        assert!(matches!(
            handle.set_sink_volumes(&macs(&[SONY]), 0.55).await,
            Ok(())
        ));
        assert_eq!(
            handle.sink_volumes(&macs(&[SONY])).await.ok(),
            Some(vec![Some(0.55)])
        );
        assert_eq!(
            handle.route_for_spotify(&[target(JBL, 40)]).await.ok(),
            Some(COMBINED.to_string())
        );
        assert_eq!(tokio::time::Instant::now(), before, "no wait was needed");
    }

    // Criterion: several identical volume reads queued behind a held actor —
    // once it is released the read runs once, one read of the sink list, and
    // every waiting caller gets that answer.
    #[tokio::test(start_paused = true)]
    async fn test_reads_queued_behind_a_held_actor_are_answered_by_one_read_of_the_graph() {
        let fake = two_speakers();
        let handle = RouterHandle::over_fake(&fake);
        let held = handle.hold_actor();

        let mut polls = Vec::new();
        for _ in 0..3 {
            // Cloned: each poll runs on a task of its own.
            let handle = handle.clone();
            polls.push(tokio::spawn(async move {
                handle.sink_volumes(&macs(&[JBL, SONY])).await
            }));
            settle().await;
            tokio::time::advance(Duration::from_millis(50)).await;
        }
        assert!(polls.iter().all(|poll| !poll.is_finished()));
        assert_eq!(fake.all_calls(), Vec::<GraphCall>::new());

        drop(held);
        settle().await;

        for poll in polls {
            assert!(poll.is_finished(), "every poll was answered");
            assert_eq!(
                poll.await.ok().and_then(Result::ok),
                Some(vec![Some(0.4), Some(0.7)])
            );
        }
        assert_eq!(sink_list_reads(&fake), 1, "calls: {:?}", fake.all_calls());
    }

    // Criterion: a volume set queued behind a held actor and followed by
    // another one for the same speaker is superseded — it answers
    // `Superseded`, not success, and its level never reaches the graph; the
    // later one is applied.
    #[tokio::test(start_paused = true)]
    async fn test_a_set_followed_by_another_for_the_same_speaker_behind_a_held_actor_is_superseded()
    {
        let fake = two_speakers();
        let handle = RouterHandle::over_fake(&fake);
        let held = handle.hold_actor();

        let older = tokio::spawn({
            // Cloned: each request runs on a task of its own.
            let handle = handle.clone();
            async move { handle.set_sink_volumes(&macs(&[JBL]), 0.3).await }
        });
        settle().await;
        let newer = tokio::spawn({
            // Cloned: each request runs on a task of its own.
            let handle = handle.clone();
            async move { handle.set_sink_volumes(&macs(&[JBL]), 0.5).await }
        });
        settle().await;
        tokio::time::advance(Duration::from_millis(100)).await;
        settle().await;
        assert!(!older.is_finished() && !newer.is_finished());

        drop(held);
        settle().await;

        let older = older.await.expect("the call's task ends");
        assert!(
            matches!(older, Err(RouterError::Superseded)),
            "got {older:?}"
        );
        assert!(matches!(newer.await.expect("the call's task ends"), Ok(())));
        assert_eq!(
            fake.calls(),
            vec![GraphCall::SetSinkVolume {
                sink: JBL_SINK.to_string(),
                level: 0.5
            }]
        );
    }

    // Criteria (#146 kept; guard, a background message never expires): a
    // request whose message the actor reaches 301 ms after it was sent is
    // not run and answers `AudioError::Expired` — at once, not at the 2 s
    // bound — while the background repair sent at the same instant, which
    // waited behind the same held actor just as long, runs. One hold, both
    // messages: the graph receives the repair's build and nothing of the
    // set.
    #[tokio::test(start_paused = true)]
    async fn test_a_request_reached_301_ms_late_expires_beside_a_background_call_that_runs() {
        let fake = two_speakers();
        let handle = RouterHandle::over_fake(&fake);
        let held = handle.hold_actor();

        let request = tokio::spawn({
            // Cloned: each call runs on a task of its own.
            let handle = handle.clone();
            async move { handle.set_sink_volumes(&macs(&[JBL]), 0.8).await }
        });
        let background = tokio::spawn({
            // Cloned: each call runs on a task of its own.
            let handle = handle.clone();
            async move { handle.repair(&[target(JBL, 0)]).await }
        });
        settle().await;
        tokio::time::advance(Duration::from_millis(301)).await;
        settle().await;
        assert!(!request.is_finished() && !background.is_finished());

        drop(held);
        settle().await;

        assert!(request.is_finished(), "the expiry is answered at once");
        let expired = request.await.expect("the call's task ends");
        assert!(
            matches!(expired, Err(RouterError::Audio(AudioError::Expired))),
            "got {expired:?}"
        );
        let repaired = background.await.expect("the call's task ends");
        assert!(
            matches!(
                &repaired,
                Ok(RepairOutcome {
                    routed: Ok(()),
                    changed: true,
                    retarget_failed: false
                })
            ),
            "got {repaired:?}"
        );
        assert_eq!(rebuilds(&fake), 1);
        assert!(
            !fake
                .calls()
                .iter()
                .any(|call| matches!(call, GraphCall::SetSinkVolume { .. })),
            "the expired set never reached the graph: {:?}",
            fake.calls()
        );
    }

    // Criterion (#146, guard): expired means strictly past the start budget.
    // The near miss is a message the actor reaches exactly 300 ms after it
    // was sent: it runs.
    #[tokio::test(start_paused = true)]
    async fn test_a_request_reached_300_ms_after_it_was_sent_still_runs() {
        let fake = two_speakers();
        let handle = RouterHandle::over_fake(&fake);
        let held = handle.hold_actor();

        let request = tokio::spawn({
            // Cloned: the request runs on a task of its own.
            let handle = handle.clone();
            async move { handle.set_sink_volumes(&macs(&[JBL]), 0.8).await }
        });
        settle().await;
        tokio::time::advance(Duration::from_millis(300)).await;
        settle().await;
        assert!(!request.is_finished());

        drop(held);
        settle().await;

        assert!(matches!(
            request.await.expect("the call's task ends"),
            Ok(())
        ));
        assert_eq!(
            fake.calls(),
            vec![GraphCall::SetSinkVolume {
                sink: JBL_SINK.to_string(),
                level: 0.8
            }]
        );
    }

    // Criterion (guard, a closed reply is never run): a request that gave up
    // at the bound sends nothing to the graph once the actor is released —
    // the log is read after the release, and is empty.
    #[tokio::test(start_paused = true)]
    async fn test_a_request_that_timed_out_reaches_no_graph_once_the_actor_is_released() {
        let fake = two_speakers();
        let handle = RouterHandle::over_fake(&fake);
        let held = handle.hold_actor();

        let request = tokio::spawn({
            // Cloned: the request runs on a task of its own.
            let handle = handle.clone();
            async move { handle.route(&[target(JBL, 0)]).await }
        });
        settle().await;
        tokio::time::advance(Duration::from_secs(2)).await;
        settle().await;
        assert!(request.is_finished(), "the request gave up at the bound");
        let answer = request.await.expect("the call's task ends");
        assert!(
            matches!(answer, Err(RouterError::TimedOut)),
            "got {answer:?}"
        );

        drop(held);
        settle().await;

        assert_eq!(fake.all_calls(), Vec::<GraphCall>::new());
    }

    // Criterion (guard, a closed reply is never run — the caller left): a
    // background call whose task was cancelled while the actor was held
    // sends nothing to the graph once the actor is released. It carries no
    // start deadline, so nothing but its closed reply keeps it from
    // running: the near miss is the same call left waiting, which rebuilds
    // (the next test).
    #[tokio::test(start_paused = true)]
    async fn test_a_background_call_whose_caller_left_reaches_no_graph_once_the_actor_is_released()
    {
        let fake = two_speakers();
        let handle = RouterHandle::over_fake(&fake);
        let held = handle.hold_actor();

        let pass = tokio::spawn({
            // Cloned: the pass runs on a task of its own.
            let handle = handle.clone();
            async move { handle.repair(&[target(JBL, 0)]).await }
        });
        settle().await;
        assert!(!pass.is_finished(), "the pass waits behind the hold");
        pass.abort();
        settle().await;
        assert!(pass.is_finished(), "the caller left");

        drop(held);
        settle().await;

        assert_eq!(fake.all_calls(), Vec::<GraphCall>::new());
    }

    // Criterion: a background call waits past the request bound and runs
    // once the actor is free — 5 s behind a held actor, then the rebuild.
    #[tokio::test(start_paused = true)]
    async fn test_a_background_call_behind_an_actor_held_5_s_runs_once_it_is_released() {
        let fake = two_speakers();
        let handle = RouterHandle::over_fake(&fake);
        let held = handle.hold_actor();

        let pass = tokio::spawn({
            // Cloned: the pass runs on a task of its own.
            let handle = handle.clone();
            async move { handle.repair(&[target(JBL, 0)]).await }
        });
        settle().await;
        tokio::time::advance(Duration::from_secs(5)).await;
        settle().await;
        assert!(!pass.is_finished(), "the pass is still waiting");
        assert_eq!(fake.all_calls(), Vec::<GraphCall>::new());

        drop(held);
        settle().await;

        assert!(pass.is_finished(), "the pass ran once the actor was free");
        assert!(matches!(
            pass.await.expect("the call's task ends"),
            Ok(RepairOutcome {
                routed: Ok(()),
                changed: true,
                ..
            })
        ));
        assert_eq!(rebuilds(&fake), 1);
    }

    // Criterion (guard, unreadable is not absent): a retune that finds the
    // sink list unreadable answers the graph's failure — it used to answer
    // `Ok(())`, as if there were no combined sink. The near miss is the
    // graph beside it, whose list reads fine and holds no combined sink:
    // that retune is `Ok(())`, with nothing written.
    #[tokio::test(start_paused = true)]
    async fn test_a_retune_over_an_unreadable_sink_list_answers_the_graph_s_failure() {
        let unreadable = two_speakers();
        unreadable.fail(GraphOp::Sinks);
        let answer = RouterHandle::over_fake(&unreadable)
            .retune(COMBINED, &sony_branch(120))
            .await;
        assert!(
            matches!(
                &answer,
                Err(RouterError::Audio(AudioError::PipeWire(m))) if m.contains("Sinks told to fail")
            ),
            "got {answer:?}"
        );
        assert_eq!(unreadable.all_calls(), vec![GraphCall::Sinks]);

        let absent = two_speakers();
        let answer = RouterHandle::over_fake(&absent)
            .retune(COMBINED, &sony_branch(120))
            .await;
        assert!(matches!(answer, Ok(())), "got {answer:?}");
        assert_eq!(absent.all_calls(), vec![GraphCall::Sinks]);
    }

    // Criterion (guard, the latest selection, once): a selection stamped
    // before `request_routing()` moved the generation is not applied — the
    // Sony's branch is never loaded — and answers `Outdated`; stamped with
    // the generation read after it, the same call runs.
    #[tokio::test(start_paused = true)]
    async fn test_a_selection_stamped_before_a_routing_request_is_outdated_and_reaches_no_graph() {
        let fake = two_speakers();
        let handle = RouterHandle::over_fake(&fake);
        let both = [target(JBL, 0), target(SONY, 0)];
        let stamped = handle.routing_generation();
        handle.request_routing();

        let stale = handle.apply_selection(&both, stamped).await;

        assert!(matches!(stale, Err(RouterError::Outdated)), "got {stale:?}");
        assert_eq!(fake.all_calls(), Vec::<GraphCall>::new());

        let current = handle
            .apply_selection(&both, handle.routing_generation())
            .await;
        assert!(matches!(current, Ok(())), "got {current:?}");
        assert_eq!(rebuilds(&fake), 1);
    }

    // Criterion: the handle's `confirmation_due()` follows what the actor
    // publishes after each message, with no message sent to learn it: `None`
    // before anything ran, the instant the first load's reload falls due —
    // 5 s on the router's clock — once a route built the graph.
    #[tokio::test(start_paused = true)]
    async fn test_confirmation_due_is_published_by_the_actor_after_a_route_armed_a_reload() {
        let fake = two_speakers();
        let handle = RouterHandle::over_fake_with_clock(&fake, Arc::new(now));
        let due = handle.confirmation_due();
        assert_eq!(*due.borrow(), None);
        tokio::time::advance(Duration::from_secs(3)).await;
        let routed_at = now();

        assert!(matches!(handle.route(&[target(JBL, 0)]).await, Ok(())));

        assert_eq!(*due.borrow(), Some(routed_at + Duration::from_secs(5)));
    }
}
