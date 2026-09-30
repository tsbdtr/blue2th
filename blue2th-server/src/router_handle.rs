// SPDX-License-Identifier: MIT OR Apache-2.0

//! The one way into the audio router (#145).
//!
//! RED-phase skeleton: the types the tests name, with a wait that is not
//! bounded yet. The request-path methods, the background routing applier and
//! the bound itself are what the tests in `lib.rs` pin.

use std::{fmt, sync::Arc, time::Duration};

use tokio::sync::{Mutex, MutexGuard};

use crate::audio::{AudioError, AudioRouter};

/// How long a request waits for the router before giving up.
///
/// RED-phase skeleton value: `test_router_wait_is_two_seconds` pins the real one.
pub const ROUTER_WAIT: Duration = Duration::from_secs(3600);

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
}

impl RouterHandle {
    /// A handle owning `router`.
    pub fn new(router: AudioRouter) -> Self {
        Self {
            router: Arc::new(Mutex::new(router)),
        }
    }

    /// Take the router with no bound on the wait: for background tasks, where
    /// a repair delayed is better than a repair dropped.
    pub async fn lock_unbounded(&self) -> MutexGuard<'_, AudioRouter> {
        self.router.lock().await
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
