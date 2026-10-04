// SPDX-License-Identifier: MIT OR Apache-2.0

//! Runtime-agnostic timers for the code shared by the native and the browser
//! builds (#159).
//!
//! RED-phase stub: the bodies below are deliberately wrong so that
//! `tests/timer.rs` fails until the real helper lands.

use std::future::Future;
use std::time::Duration;

/// The error [`timeout`] returns when the deadline passes before the future
/// completes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Elapsed;

/// Waits for `duration`.
pub async fn sleep(duration: Duration) {
    let _ = duration;
}

/// Runs `future` until it completes or `duration` passes, whichever comes first.
pub async fn timeout<F: Future>(duration: Duration, future: F) -> Result<F::Output, Elapsed> {
    let _ = (duration, future);
    Err(Elapsed)
}
