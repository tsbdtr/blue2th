// SPDX-License-Identifier: MIT OR Apache-2.0

//! The one way into the audio router (#145).
//!
//! A request waits for the router at most [`ROUTER_WAIT`]: while the PipeWire
//! daemon does not answer, whoever holds the router holds it for as long as
//! the daemon stays frozen, and a request queued behind it used to wait just
//! as long, then succeed late with nothing telling the user anything was
//! wrong. Background tasks keep an unbounded wait — a repair delayed is better
//! than a repair dropped — and a selection change hands its routing to the
//! single background applier through [`RouterHandle::request_routing`], so it
//! is never lost to the bound.

use std::{fmt, sync::Arc, time::Duration};

use blue2th_proto::{SpeakerTarget, SpotifyState};
use tokio::sync::{watch, Mutex, MutexGuard};

use crate::audio::{AudioError, AudioRouter, CombineBranch};
use crate::spotify::{SpotifyBackend, SpotifyError};

/// How long a request waits for the router before giving up.
///
/// A healthy holder keeps the router a few milliseconds, even for a full
/// rebuild of the combined sink, so a wait this long only ever expires behind
/// a graph that is not answering.
pub const ROUTER_WAIT: Duration = Duration::from_secs(2);

/// Why a request-path router operation failed.
#[derive(Debug)]
pub enum RouterError {
    /// The router was not obtained within [`ROUTER_WAIT`]: nothing was sent
    /// to the graph.
    TimedOut,
    /// The router was obtained and the graph call failed.
    Audio(AudioError),
}

impl fmt::Display for RouterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RouterError::TimedOut => write!(f, "the audio router was not obtained in time"),
            RouterError::Audio(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for RouterError {}

impl From<AudioError> for RouterError {
    fn from(err: AudioError) -> Self {
        RouterError::Audio(err)
    }
}

/// A cloneable handle onto the shared [`AudioRouter`].
#[derive(Clone)]
pub struct RouterHandle {
    router: Arc<Mutex<AudioRouter>>,
    /// Wakes the background routing applier. A `watch` rather than a queue:
    /// every request made before the applier marks it seen folds into one
    /// pass, which reads the selection current at that moment.
    routing_requests: Arc<watch::Sender<()>>,
}

impl RouterHandle {
    /// A handle owning `router`.
    pub fn new(router: AudioRouter) -> Self {
        let (routing_requests, _) = watch::channel(());
        Self {
            router: Arc::new(Mutex::new(router)),
            routing_requests: Arc::new(routing_requests),
        }
    }

    /// Take the router with no bound on the wait: for background tasks, where
    /// a repair delayed is better than a repair dropped.
    pub async fn lock_unbounded(&self) -> MutexGuard<'_, AudioRouter> {
        self.router.lock().await
    }

    /// Take the router, giving up after [`ROUTER_WAIT`].
    async fn lock_bounded(&self) -> Result<MutexGuard<'_, AudioRouter>, RouterError> {
        tokio::time::timeout(ROUTER_WAIT, self.router.lock())
            .await
            .map_err(|_| RouterError::TimedOut)
    }

    /// Ask the background applier to route the graph to the current selection.
    /// Never waits: the applier reads the selection once it holds the router.
    pub fn request_routing(&self) {
        self.routing_requests.send_replace(());
    }

    /// A receiver of [`Self::request_routing`] wakes, for the applier. Only
    /// the requests made after this call wake it.
    pub fn routing_requests(&self) -> watch::Receiver<()> {
        self.routing_requests.subscribe()
    }

    /// Route the graph to `speakers`.
    pub async fn route(&self, speakers: &[SpeakerTarget]) -> Result<(), RouterError> {
        Ok(self.lock_bounded().await?.route_for_targets(speakers)?)
    }

    /// The live volume of each speaker in `macs`, over one read of the sink
    /// list (see [`AudioRouter::sink_volumes`]).
    pub async fn sink_volumes(&self, macs: &[String]) -> Result<Vec<Option<f32>>, RouterError> {
        Ok(self.lock_bounded().await?.sink_volumes(macs)?)
    }

    /// Set every speaker in `macs` to `level`, stopping at the first failure.
    pub async fn set_sink_volumes(&self, macs: &[String], level: f32) -> Result<(), RouterError> {
        let mut router = self.lock_bounded().await?;
        for mac in macs {
            router.set_sink_volume(mac, level)?;
        }
        Ok(())
    }

    /// Retune `branch` in place inside the combined sink `sink_name`; nothing
    /// to do while that sink is not loaded.
    pub async fn retune(&self, sink_name: &str, branch: &CombineBranch) -> Result<(), RouterError> {
        let mut router = self.lock_bounded().await?;
        if router.combined_sink_exists(sink_name) {
            router.retune_branch(sink_name, branch)?;
        }
        Ok(())
    }

    /// Start `spotify` towards `speakers`. The outer `Result` is whether the
    /// router was obtained; the inner one is the start itself.
    pub async fn start_spotify(
        &self,
        spotify: &mut SpotifyBackend,
        speakers: &[SpeakerTarget],
    ) -> Result<Result<SpotifyState, SpotifyError>, RouterError> {
        let mut router = self.lock_bounded().await?;
        Ok(spotify.start(&mut router, speakers))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Criterion (#145): a request-path wait for the router expires after
    // `ROUTER_WAIT` = 2 s. The value lives here, in the test's name, rather
    // than in the constant's doc comment.
    #[test]
    fn test_router_wait_is_two_seconds() {
        assert_eq!(ROUTER_WAIT, Duration::from_secs(2));
    }
}
