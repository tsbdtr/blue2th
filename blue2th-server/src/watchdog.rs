// SPDX-License-Identifier: MIT OR Apache-2.0

//! Idle watchdog for the now-playing SSE feed (phase 5.2).
//!
//! Playback deliberately keeps going while the app sits in the background, so the
//! backend needs some other way to notice that nobody is there any more: a
//! swipe-away, a crash, an OOM kill or a dropped network would otherwise leave the
//! PC streaming to nobody.
//!
//! The app already holds the `/spotify/now-playing` SSE stream open for as long as
//! it runs, so that connection *is* a heartbeat — no extra route, no extra traffic,
//! no timer on the phone. This module counts the readers and, once the last one has
//! been gone long enough, the router pauses playback.
//!
//! Losing the feed is ambiguous on its own: Android freezes a backgrounded app,
//! which drops the connection even though the user is deliberately listening on.
//! So the app reports what it is doing (`POST /client/presence`) and the grace
//! period follows: short-ish in the foreground (only a crash can cut the feed
//! there), long in the background (the freeze is expected), and a `Gone` report
//! pauses at once without waiting for any of it.
//!
//! A presence report is positive evidence of liveness, so on any report other
//! than `Gone` the idle clock restarts while no reader is connected. Without
//! that, a `Foreground` report arriving after a long background idle would apply
//! the shorter foreground grace to time already spent under the longer one, and
//! the very next tick would pause the playback the user just came back to.
//! Accounting the elapsed time per presence was considered and rejected: more
//! bookkeeping for the same outcome, and a second clock to keep honest.
//!
//! The clock is a parameter (`now: Instant`) rather than `Instant::now()` read
//! inside, so the arithmetic — minutes of idle time against a grace period — is a
//! unit test instead of a sleep.

use std::{
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use blue2th_proto::ClientPresence;

/// Grace period while the app says it is on screen: only a crash or a kill can
/// cut the feed there, so this needs no slack for a frozen process.
pub const FOREGROUND_GRACE: Duration = Duration::from_secs(10 * 60);

/// Grace period while the app says it is backgrounded. Android freezes the
/// process — and with it the SSE connection — within seconds, so this is not a
/// liveness measure at all: it is the backstop for an app that was killed while
/// backgrounded and will never report `Gone`. Hence the deliberately long value:
/// it must never cut a listening session short.
pub const BACKGROUND_GRACE: Duration = Duration::from_secs(30 * 60);

/// How often the watchdog re-checks. Well under either grace period, so the pause
/// lands close to the deadline without polling tightly.
pub const WATCHDOG_TICK: Duration = Duration::from_secs(10);

/// How long a silent feed is tolerated for a given presence. Pure.
pub fn grace_for(presence: ClientPresence) -> Duration {
    match presence {
        ClientPresence::Foreground => FOREGROUND_GRACE,
        // `Gone` is handled by pausing immediately; should one still be pending
        // here, treat it like the background backstop rather than never firing.
        ClientPresence::Background | ClientPresence::Gone => BACKGROUND_GRACE,
    }
}

/// Whether an idle feed should trigger a pause: no reader left, and the last one
/// gone for at least `grace`. Pure — the caller supplies the elapsed time.
pub fn should_pause_on_idle(readers: usize, empty_for: Option<Duration>, grace: Duration) -> bool {
    readers == 0 && empty_for.is_some_and(|elapsed| elapsed >= grace)
}

/// Reader count for the now-playing SSE feed, when it last fell to zero, and the
/// app's last reported presence.
#[derive(Debug, Default)]
pub struct SseWatch {
    readers: AtomicUsize,
    /// The app's last report. `None` (never reported) is treated as foreground:
    /// an older client that does not post its presence keeps the tighter grace.
    presence: std::sync::Mutex<Option<ClientPresence>>,
    /// When the count last reached zero, as a monotonic instant. `None` while at
    /// least one reader is connected.
    empty_since: std::sync::Mutex<Option<Instant>>,
    /// Set once the watchdog has paused for the current idle period, so it fires
    /// once per departure instead of every tick.
    paused: AtomicBool,
}

impl SseWatch {
    /// Register a reader. The returned guard must live as long as the stream: it
    /// is what detects the client leaving, however the stream ends.
    pub fn subscribe(self: &std::sync::Arc<Self>) -> SseGuard {
        self.readers.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut empty_since) = self.empty_since.lock() {
            *empty_since = None;
        }
        // A returning reader re-arms the watchdog for the next departure.
        self.paused.store(false, Ordering::SeqCst);
        SseGuard {
            watch: Some(std::sync::Arc::clone(self)),
        }
    }

    /// Whether playback should be paused at `now`, claiming the right to do it so
    /// the following ticks stay quiet until a reader comes back. `Some(idle)`
    /// carries how long the feed has been empty, for the log line.
    pub fn claim_idle_pause(&self, grace: Duration, now: Instant) -> Option<Duration> {
        if self.paused.load(Ordering::SeqCst) {
            return None;
        }
        let empty_for = self
            .empty_since
            .lock()
            .ok()
            .and_then(|since| since.map(|instant| now.saturating_duration_since(instant)));
        if !should_pause_on_idle(self.readers.load(Ordering::SeqCst), empty_for, grace) {
            return None;
        }
        self.paused.store(true, Ordering::SeqCst);
        empty_for
    }

    /// Drop a reader, starting the idle clock at `now` when it was the last one.
    fn release(&self, now: Instant) {
        // `fetch_sub` returns the previous value: 1 means we just removed the last.
        if self.readers.fetch_sub(1, Ordering::SeqCst) == 1 {
            if let Ok(mut empty_since) = self.empty_since.lock() {
                *empty_since = Some(now);
            }
        }
    }

    /// Record what the app says it is doing, as reported at `now`. Any report
    /// other than `Gone` is proof the app is alive at `now`, so while no reader is
    /// connected the idle clock restarts there. `Gone` leaves it alone: the handler
    /// pauses at once, and the clock keeps counting from the reader's departure.
    /// The pause claim is untouched either way — only a returning reader
    /// (`subscribe`) re-arms it, so an app that thaws for a moment, reports and
    /// freezes again is not paused twice for the same idle period.
    pub fn set_presence(&self, presence: ClientPresence, now: Instant) {
        if let Ok(mut slot) = self.presence.lock() {
            *slot = Some(presence);
        }
        if presence == ClientPresence::Gone || self.readers.load(Ordering::SeqCst) != 0 {
            return;
        }
        if let Ok(mut empty_since) = self.empty_since.lock() {
            *empty_since = Some(now);
        }
    }

    /// The app's last reported presence, defaulting to foreground.
    pub fn presence(&self) -> ClientPresence {
        self.presence
            .lock()
            .ok()
            .and_then(|slot| *slot)
            .unwrap_or(ClientPresence::Foreground)
    }

    /// Current reader count (tests and diagnostics).
    pub fn readers(&self) -> usize {
        self.readers.load(Ordering::SeqCst)
    }

    /// The raw idle-clock stamp, so a test can pin the invariant the field
    /// documents (`None` while a reader is connected). Nothing outside the tests
    /// reads it: `claim_idle_pause` is the production view of this clock.
    #[cfg(test)]
    fn empty_since(&self) -> Option<Instant> {
        self.empty_since.lock().ok().and_then(|since| *since)
    }
}

/// Keeps a reader counted for as long as it is held. Dropping it — the stream
/// ending, the client vanishing, the task being cancelled — releases the reader.
pub struct SseGuard {
    /// Taken by `release_at`, so the drop that follows has nothing left to release.
    watch: Option<std::sync::Arc<SseWatch>>,
}

impl SseGuard {
    /// Release the reader as of `now`. Production lets the drop do this with the
    /// real clock; tests use this to place the departure on a chosen instant.
    pub fn release_at(mut self, now: Instant) {
        if let Some(watch) = self.watch.take() {
            watch.release(now);
        }
    }
}

impl Drop for SseGuard {
    fn drop(&mut self) {
        if let Some(watch) = self.watch.take() {
            watch.release(Instant::now());
        }
    }
}

#[cfg(test)]
mod tests;
