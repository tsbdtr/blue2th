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
    /// The routing generation, the confirmation due time and the applier's
    /// wake-up, shared with every actor started for this handle (#147,
    /// #152).
    shared: Shared,
    /// The means to hold the actor, when it is a fake one.
    #[cfg(test)]
    holder: Option<crate::router_actor::testing::ActorHolder>,
}

impl RouterHandle {
    /// A handle sending through `transport`, to actors sharing `shared`
    /// (#147).
    pub(crate) fn over(transport: Box<dyn Transport>, shared: Shared) -> Self {
        Self {
            transport: Arc::new(Mutex::new(transport)),
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
        self.shared.request_routing();
    }

    /// A receiver of [`Self::request_routing`] wakes, for the applier. Only
    /// the requests made after this call wake it.
    pub fn routing_requests(&self) -> watch::Receiver<()> {
        self.shared.routing_requests()
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
mod tests;
