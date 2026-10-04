// SPDX-License-Identifier: MIT OR Apache-2.0

//! The runtime-agnostic timer helper (#159), from the outside.
//!
//! These run natively, so they exercise the tokio half. The wasm half
//! (`gloo-timers`) is covered by compilation only: there is no browser test
//! runner here.
//!
//! Durations are measured with `std::time::Instant` on real time, never with a
//! paused tokio clock: "returns no earlier than its deadline" is a criterion about
//! time, and only the wall clock can tell an early return from a correct one.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use blue2th_frontend::timer;

/// How long the outer watchdog lets a call run before declaring that it hung.
/// Far above every deadline below, so it only ever fires on a wrong helper.
const WATCHDOG: Duration = Duration::from_secs(2);

// Criterion: `timer::sleep(d)` waits for `d` — natively it is
// `tokio::time::sleep`, so it returns no earlier than its duration.
#[tokio::test]
async fn test_timer_sleep_returns_no_earlier_than_its_duration() {
    let duration = Duration::from_millis(80);
    let start = Instant::now();

    timer::sleep(duration).await;

    let elapsed = start.elapsed();
    assert!(
        elapsed >= duration,
        "sleep({duration:?}) returned after {elapsed:?}"
    );
}

// Criterion: a `timeout()` whose future finishes before the deadline returns its
// value. Guard near-miss: a future that completes at once, well within the
// deadline — an implementation that always elapses fails here.
#[tokio::test]
async fn test_timer_timeout_returns_the_value_of_a_future_ready_at_once() {
    let result: Result<u8, timer::Elapsed> =
        timer::timeout(Duration::from_secs(1), async { 42u8 }).await;

    assert_eq!(result.ok(), Some(42));
}

// Criterion: a `timeout()` whose future finishes before the deadline returns its
// value — also when the future has to wait (it is polled again once woken, not
// only once).
#[tokio::test]
async fn test_timer_timeout_returns_the_value_of_a_future_that_finishes_in_time() {
    let work = async {
        tokio::time::sleep(Duration::from_millis(20)).await;
        7u8
    };

    let result = tokio::time::timeout(WATCHDOG, timer::timeout(Duration::from_secs(1), work)).await;

    assert!(
        result.is_ok(),
        "timeout must return once its future completes"
    );
    assert_eq!(result.ok().and_then(Result::ok), Some(7));
}

// Criterion: a `timeout()` whose future does not finish returns an elapsed
// error — once the deadline has passed, not before.
#[tokio::test]
async fn test_timer_timeout_elapses_on_a_future_that_never_completes() {
    let deadline = Duration::from_millis(60);
    let start = Instant::now();

    let outer = tokio::time::timeout(
        WATCHDOG,
        timer::timeout(deadline, futures::future::pending::<u8>()),
    )
    .await;
    let elapsed = start.elapsed();

    assert!(
        outer.is_ok(),
        "timeout must give up at its deadline rather than await a pending future forever"
    );
    assert!(
        outer.is_ok_and(|inner| inner.is_err()),
        "a future that never completes must yield Elapsed"
    );
    assert!(
        elapsed >= deadline,
        "Elapsed came back after {elapsed:?}, before the {deadline:?} deadline"
    );
}

// Criterion: a future slower than the deadline yields Elapsed at the deadline,
// rather than its own value once it eventually finishes. Near-miss: the future
// *does* complete (after 10 s) — an implementation that simply awaits it would
// return `Ok(9)` and trip the watchdog.
#[tokio::test]
async fn test_timer_timeout_elapses_on_a_future_slower_than_the_deadline() {
    let deadline = Duration::from_millis(50);
    let slow = async {
        tokio::time::sleep(Duration::from_secs(10)).await;
        9u8
    };
    let start = Instant::now();

    let outer = tokio::time::timeout(WATCHDOG, timer::timeout(deadline, slow)).await;
    let elapsed = start.elapsed();

    assert!(outer.is_ok(), "timeout must not wait for the slow future");
    assert!(outer.is_ok_and(|inner| inner.is_err()));
    assert!(
        elapsed >= deadline,
        "Elapsed came back after {elapsed:?}, before the {deadline:?} deadline"
    );
}

/// A future that never completes, recording that it was polled and that it was
/// dropped.
struct Tracked {
    polled: Arc<AtomicBool>,
    dropped: Arc<AtomicBool>,
}

impl Future for Tracked {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        self.polled.store(true, Ordering::SeqCst);
        Poll::Pending
    }
}

impl Drop for Tracked {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

// Criterion: a `timeout()` whose future does not finish returns an elapsed
// error, and the future is dropped. Near-miss: an implementation that runs the
// future elsewhere (spawned, kept in a slot) still returns Elapsed — only the
// drop flag tells it apart. The polled flag pins that the future was actually
// run during the window, not discarded unpolled.
#[tokio::test]
async fn test_timer_timeout_drops_the_timed_out_future() {
    let polled = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));
    let future = Tracked {
        polled: Arc::clone(&polled),
        dropped: Arc::clone(&dropped),
    };

    let outer =
        tokio::time::timeout(WATCHDOG, timer::timeout(Duration::from_millis(30), future)).await;

    assert!(outer.is_ok_and(|inner| inner.is_err()));
    assert!(
        polled.load(Ordering::SeqCst),
        "the future must be polled while the deadline runs"
    );
    assert!(
        dropped.load(Ordering::SeqCst),
        "the timed-out future must be dropped by the time timeout returns"
    );
}
