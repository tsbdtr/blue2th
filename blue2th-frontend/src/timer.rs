// SPDX-License-Identifier: MIT OR Apache-2.0

//! Runtime-agnostic timers for the code shared by the native and the browser
//! builds (#159).
//!
//! Natively `sleep` is tokio's own, so the Android app keeps the timer it has
//! always run on; tokio's timer does not exist in the browser, where
//! `gloo-timers` drives `setTimeout` instead. `timeout` is written once, on top
//! of `sleep`, so both targets share its semantics.

use std::future::Future;
use std::time::Duration;

use futures::future::Either;

#[cfg(target_arch = "wasm32")]
pub use gloo_timers::future::sleep;
#[cfg(not(target_arch = "wasm32"))]
pub use tokio::time::sleep;

/// The error [`timeout`] returns when the deadline passes before the future
/// completes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Elapsed;

/// Runs `future` until it completes or `duration` passes, whichever comes first.
///
/// The future is polled first, so one that is already ready wins over the
/// deadline. A timed-out future is dropped before this returns.
pub async fn timeout<F: Future>(duration: Duration, future: F) -> Result<F::Output, Elapsed> {
    let future = std::pin::pin!(future);
    let deadline = std::pin::pin!(sleep(duration));
    match futures::future::select(future, deadline).await {
        Either::Left((output, _)) => Ok(output),
        Either::Right(((), _)) => Err(Elapsed),
    }
}
