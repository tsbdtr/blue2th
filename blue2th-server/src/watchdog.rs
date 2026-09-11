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
mod tests {
    use super::*;

    const ZERO: Duration = Duration::from_secs(0);

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    // Criterion: a feed with no reader for longer than the grace period is idle.
    #[test]
    fn test_should_pause_on_idle_after_grace_with_no_reader() {
        assert!(should_pause_on_idle(
            0,
            Some(FOREGROUND_GRACE + Duration::from_secs(1)),
            FOREGROUND_GRACE
        ));
    }

    // Criterion: a reader still connected never triggers a pause, however long the
    // feed has been up — this must not become a second foreground detector.
    #[test]
    fn test_should_pause_on_idle_never_with_a_reader() {
        assert!(!should_pause_on_idle(
            1,
            Some(BACKGROUND_GRACE * 10),
            FOREGROUND_GRACE
        ));
    }

    // Criterion: inside the grace period (a restarting app, a brief network drop)
    // playback is left alone; so is a feed that never had a reader.
    #[test]
    fn test_should_pause_on_idle_waits_out_the_grace_period() {
        assert!(!should_pause_on_idle(
            0,
            Some(FOREGROUND_GRACE - Duration::from_secs(1)),
            FOREGROUND_GRACE
        ));
        assert!(!should_pause_on_idle(0, None, FOREGROUND_GRACE));
    }

    // Criterion: the guard counts a reader while alive and releases it on drop,
    // which is what turns a vanished client into an idle feed. Adapted to the
    // clocked signatures (`release_at`, `Option<Duration>`); the claim is the same.
    #[test]
    fn test_guard_counts_readers_and_releases_on_drop() {
        let t0 = Instant::now();
        let watch = std::sync::Arc::new(SseWatch::default());
        let first = watch.subscribe();
        let second = watch.subscribe();
        assert_eq!(watch.readers(), 2);

        second.release_at(t0);
        assert_eq!(watch.readers(), 1);
        // Still one reader: not idle yet, whatever the grace period.
        assert!(watch.claim_idle_pause(ZERO, t0).is_none());

        first.release_at(t0);
        assert_eq!(watch.readers(), 0);
        assert!(watch.claim_idle_pause(ZERO, t0).is_some());
    }

    // Criterion: `Drop` releases the reader like `release_at` does, with the real
    // clock — production never calls `release_at`, so the drop path must count.
    #[test]
    fn test_guard_drop_releases_the_reader_with_the_real_clock() {
        let watch = std::sync::Arc::new(SseWatch::default());
        let guard = watch.subscribe();
        assert_eq!(watch.readers(), 1);
        drop(guard);
        assert_eq!(watch.readers(), 0);
        // The stamp was `Instant::now()` at the drop: a claim just after it, with
        // a zero grace, sees a (tiny) idle time.
        assert!(watch.claim_idle_pause(ZERO, Instant::now()).is_some());
    }

    // Criterion: `release_at` consumes the guard and releases exactly once — the
    // drop that follows must not decrement the count a second time.
    #[test]
    fn test_release_at_releases_the_reader_exactly_once() {
        let t0 = Instant::now();
        let watch = std::sync::Arc::new(SseWatch::default());
        let held = watch.subscribe();
        let released = watch.subscribe();
        released.release_at(t0);
        assert_eq!(
            watch.readers(),
            1,
            "release_at must release once, not twice"
        );
        drop(held);
        assert_eq!(watch.readers(), 0);
    }

    // Criterion: a backgrounded app gets a far longer grace than one on screen —
    // Android freezes it within seconds, so the dropped feed says nothing about
    // the user having left.
    #[test]
    fn test_grace_follows_the_reported_presence() {
        assert_eq!(grace_for(ClientPresence::Foreground), FOREGROUND_GRACE);
        assert_eq!(grace_for(ClientPresence::Background), BACKGROUND_GRACE);
        assert!(BACKGROUND_GRACE > FOREGROUND_GRACE);
    }

    // Criterion: presence defaults to foreground until the app reports, so a
    // client that never posts keeps the tighter grace.
    #[test]
    fn test_presence_defaults_to_foreground_and_is_recorded() {
        let watch = SseWatch::default();
        assert_eq!(watch.presence(), ClientPresence::Foreground);
        watch.set_presence(ClientPresence::Background, Instant::now());
        assert_eq!(watch.presence(), ClientPresence::Background);
    }

    // Criterion: the pause is claimed once per departure, so the watchdog does not
    // re-pause on every tick while the app stays away. Adapted to the clocked
    // signatures; the claim is the same.
    #[test]
    fn test_idle_pause_is_claimed_once_per_departure() {
        let t0 = Instant::now();
        let watch = std::sync::Arc::new(SseWatch::default());
        watch.subscribe().release_at(t0);
        assert!(watch.claim_idle_pause(ZERO, t0).is_some());
        assert!(watch.claim_idle_pause(ZERO, t0).is_none());

        // A reader coming back re-arms it for the next departure.
        watch.subscribe().release_at(t0);
        assert!(watch.claim_idle_pause(ZERO, t0).is_some());
    }

    // Criterion: `claim_idle_pause` returns the idle time it measured, as
    // `now.saturating_duration_since(empty_since)`, so the log can name it.
    #[test]
    fn test_claim_idle_pause_returns_the_measured_idle_time() {
        let t0 = Instant::now();
        let watch = std::sync::Arc::new(SseWatch::default());
        watch.subscribe().release_at(t0);
        assert_eq!(
            watch.claim_idle_pause(FOREGROUND_GRACE, t0 + secs(615)),
            Some(secs(615))
        );
    }

    // Criterion (the ticket's trace): subscribe; release at `t0` under
    // `Background`; `set_presence(Foreground, t0 + 760 s)` restarts the idle clock;
    // the claim under the foreground grace at `t0 + 761 s` is `None` (idle for
    // 1 s, not 761 s), and at `t0 + 760 s + 600 s` it is `Some(600 s)`.
    #[test]
    fn test_foreground_report_after_a_long_background_idle_does_not_pause_at_the_next_tick() {
        let t0 = Instant::now();
        let watch = std::sync::Arc::new(SseWatch::default());
        // The app is on screen with its feed open; the user locks the phone.
        let feed = watch.subscribe();
        watch.set_presence(ClientPresence::Background, t0);
        feed.release_at(t0);

        // Twelve minutes later the phone is unlocked: `Foreground` arrives before
        // the feed is re-opened.
        let report = t0 + secs(760);
        watch.set_presence(ClientPresence::Foreground, report);
        let grace = grace_for(watch.presence());
        assert_eq!(grace, FOREGROUND_GRACE);

        // The next tick, one second later: idle for 1 s under a 600 s grace.
        assert_eq!(
            watch.claim_idle_pause(grace, report + secs(1)),
            None,
            "a Foreground report must restart the idle clock, not apply the shorter grace retroactively"
        );
        // Had the feed never come back, the foreground grace runs from the report.
        assert_eq!(
            watch.claim_idle_pause(grace, report + FOREGROUND_GRACE),
            Some(FOREGROUND_GRACE)
        );
    }

    // Criterion: a `Background` report restarts the thirty-minute backstop from the
    // report: release at `t0`; `set_presence(Background, t0 + 1000 s)`; the claim
    // at `t0 + 1000 s + 1799 s` is `None` and at `t0 + 1000 s + 1800 s` is
    // `Some(1800 s)`.
    #[test]
    fn test_background_report_restarts_the_backstop_from_the_report() {
        let t0 = Instant::now();
        let watch = std::sync::Arc::new(SseWatch::default());
        watch.subscribe().release_at(t0);

        let report = t0 + secs(1000);
        watch.set_presence(ClientPresence::Background, report);
        let grace = grace_for(watch.presence());
        assert_eq!(grace, BACKGROUND_GRACE);

        assert_eq!(
            watch.claim_idle_pause(grace, report + secs(1799)),
            None,
            "the backstop counts from the report, not from the reader's departure"
        );
        assert_eq!(
            watch.claim_idle_pause(grace, report + secs(1800)),
            Some(BACKGROUND_GRACE)
        );
    }

    // Criterion: `Gone` touches no clock — the handler pauses at once, and the idle
    // clock keeps running from the reader's departure at `t0`.
    #[test]
    fn test_gone_report_touches_no_clock() {
        let t0 = Instant::now();
        let watch = std::sync::Arc::new(SseWatch::default());
        watch.subscribe().release_at(t0);

        watch.set_presence(ClientPresence::Gone, t0 + secs(100));
        assert_eq!(watch.presence(), ClientPresence::Gone);

        assert_eq!(
            watch.claim_idle_pause(BACKGROUND_GRACE, t0 + secs(1800)),
            Some(secs(1800)),
            "a Gone report must leave the idle clock counting from the departure at t0"
        );
    }

    // Criterion: a report while a reader is connected leaves `empty_since` as
    // `None`; the clock only starts at the later departure, so the claim is due
    // at `t2 + grace`, not earlier.
    #[test]
    fn test_report_while_a_reader_is_connected_leaves_the_clock_alone() {
        let t0 = Instant::now();
        let watch = std::sync::Arc::new(SseWatch::default());
        let feed = watch.subscribe();

        let t1 = t0 + secs(100);
        watch.set_presence(ClientPresence::Foreground, t1);
        // Still connected: nothing to claim, however far the clock is read.
        assert_eq!(watch.claim_idle_pause(ZERO, t1 + BACKGROUND_GRACE), None);

        let t2 = t1 + secs(300);
        feed.release_at(t2);
        assert_eq!(
            watch.claim_idle_pause(FOREGROUND_GRACE, t2 + FOREGROUND_GRACE - secs(1)),
            None,
            "the clock must start at the departure, not at the earlier report"
        );
        assert_eq!(
            watch.claim_idle_pause(FOREGROUND_GRACE, t2 + FOREGROUND_GRACE),
            Some(FOREGROUND_GRACE)
        );
    }

    // Criterion: the pause claim survives a presence report (an app that thaws for
    // a moment, posts `Background`, freezes again must not be paused twice) and is
    // cleared by `subscribe`.
    #[test]
    fn test_pause_claim_survives_a_presence_report_and_clears_on_subscribe() {
        let t0 = Instant::now();
        let watch = std::sync::Arc::new(SseWatch::default());
        watch.subscribe().release_at(t0);
        assert!(watch.claim_idle_pause(ZERO, t0).is_some());

        let later = t0 + secs(50);
        watch.set_presence(ClientPresence::Background, later);
        assert_eq!(
            watch.claim_idle_pause(BACKGROUND_GRACE, later + BACKGROUND_GRACE),
            None,
            "a presence report must not re-arm a pause already claimed for this idle period"
        );

        // A reader coming back — and leaving again — is what re-arms it.
        let back = later + secs(10);
        watch.subscribe().release_at(back);
        assert_eq!(
            watch.claim_idle_pause(BACKGROUND_GRACE, back + BACKGROUND_GRACE),
            Some(BACKGROUND_GRACE)
        );
    }

    // Criterion: an `empty_since` in the future reads as zero idle time
    // (`saturating_duration_since`) — impossible with a monotonic clock, but the
    // arithmetic must not panic.
    #[test]
    fn test_claim_idle_pause_reads_a_future_stamp_as_zero_idle() {
        let earlier = Instant::now();
        let t0 = earlier + secs(1);
        let watch = std::sync::Arc::new(SseWatch::default());
        watch.subscribe().release_at(t0);

        assert_eq!(watch.claim_idle_pause(ZERO, earlier), Some(ZERO));
    }
}
