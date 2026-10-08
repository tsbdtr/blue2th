// SPDX-License-Identifier: MIT OR Apache-2.0

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

// Criterion: a report while a reader is connected leaves `empty_since` as
// `None` — the field's invariant, which `claim_idle_pause` cannot observe
// (it gates on the reader count first, and the departure re-stamps the clock
// anyway). Dropping the reader check in `set_presence` survives every other
// test; this one names the value on both sides of the departure.
#[test]
fn test_report_while_a_reader_is_connected_keeps_empty_since_none() {
    let t0 = Instant::now();
    let watch = std::sync::Arc::new(SseWatch::default());
    let feed = watch.subscribe();
    assert_eq!(watch.empty_since(), None);

    let t1 = t0 + secs(100);
    watch.set_presence(ClientPresence::Foreground, t1);
    assert_eq!(
        watch.empty_since(),
        None,
        "a report must not start the idle clock while a reader is connected"
    );
    watch.set_presence(ClientPresence::Background, t1 + secs(1));
    assert_eq!(watch.empty_since(), None);

    let t2 = t1 + secs(300);
    feed.release_at(t2);
    assert_eq!(watch.empty_since(), Some(t2));
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
