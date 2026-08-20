//! Auto-reconnect policy for remembered speakers (phase 6.5).
//!
//! Phase 6.3 restores the *selection* when a remembered speaker reappears; this
//! module is what brings it back in the first place. It is **pure**: it decides
//! which addresses to dial and when, never talks to BlueZ, and never reads the
//! clock — `Instant` comes in as a parameter, exactly as `auth::AuthStore::arm_pairing`
//! and `watchdog::grace_for` do, which is the only way the retry ladder is testable.
//!
//! Auto-reconnect only *connects*: it never selects, never routes and never
//! plays. The `sync_connected` → `restore` path (phase 6.3) takes over on the
//! next `/devices` poll.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// The widening waits between the first retries, in order.
pub const BACKOFF_RAMP: [Duration; 4] = [
    Duration::from_secs(15),
    Duration::from_secs(30),
    Duration::from_secs(60),
    Duration::from_secs(120),
];

/// The interval the ramp settles on once it is exhausted.
pub const BACKOFF_CAP: Duration = Duration::from_secs(300);

/// How many attempts are made at [`BACKOFF_CAP`] before the address is given up
/// on. A speaker that is still away after that is off for the evening, and a
/// backend that keeps dialling it forever is a backend that never idles.
pub const ATTEMPTS_AT_CAP: usize = 3;

/// How often the background pass wakes up. No longer than the first backoff
/// step, or the ladder's first rung would be rounded up to the tick.
pub const RECONNECT_TICK: Duration = Duration::from_secs(15);

/// How long to wait before the next attempt, given how many failures were
/// already recorded **before** the one that just happened (`0` for the first
/// failure). `None` means the ladder is exhausted: the address is given up on.
///
/// Pure and table-driven: the ramp first, then [`ATTEMPTS_AT_CAP`] attempts at
/// [`BACKOFF_CAP`].
pub fn next_delay(failures: usize) -> Option<Duration> {
    match BACKOFF_RAMP.get(failures) {
        Some(delay) => Some(*delay),
        None if failures < BACKOFF_RAMP.len() + ATTEMPTS_AT_CAP => Some(BACKOFF_CAP),
        None => None,
    }
}

/// Whether the reconnect pass may run at all. `false` short-circuits the whole
/// pass, before any D-Bus call: with the setting off the backend dials nothing
/// and keeps no per-address state. Pure.
pub fn should_auto_reconnect(auto_reconnect: bool) -> bool {
    auto_reconnect
}

/// The addresses worth dialling: the persisted playback intent, restricted to
/// what is **paired** and **not connected**, in intent order.
///
/// Auto-reconnect connects, it never pairs: an intent address that is no longer
/// a paired device is simply not a candidate (it stays in the intent, so a
/// re-pairing brings it back into scope). A connected speaker is never a
/// candidate either. Pure.
pub fn reconnect_candidates(
    intended: &[String],
    paired: &[String],
    connected: &[String],
) -> Vec<String> {
    intended
        .iter()
        .filter(|addr| paired.iter().any(|p| p == *addr))
        .filter(|addr| !connected.iter().any(|c| c == *addr))
        .cloned()
        .collect()
}

/// Per-address retry bookkeeping: how many attempts failed, when the next one is
/// due, whether the address was given up on, and whether the user dismissed it.
#[derive(Debug, Default, Clone)]
struct AddressState {
    /// Consecutive failed attempts recorded for this address.
    failures: usize,
    /// The earliest instant the next attempt may be made, or `None` for "now".
    next_attempt: Option<Instant>,
    /// Whether the ladder ran out and the backend stopped dialling this address.
    given_up: bool,
    /// Whether the user disconnected this address from the app: the intent (and
    /// its remembered offset) is kept, but auto-reconnect leaves it alone until
    /// the user re-selects or reconnects it. The backend must not fight the user.
    dismissed: bool,
}

/// Which remembered addresses are due for a reconnect attempt, and what the
/// backoff ladder has done to each of them so far.
///
/// Time is injected on every call: the tracker never reads the clock.
#[derive(Debug, Default)]
pub struct ReconnectTracker {
    /// Per-address state, keyed by Bluetooth address.
    states: HashMap<String, AddressState>,
}

impl ReconnectTracker {
    /// A tracker that has seen nothing yet: every candidate is due at once.
    pub fn new() -> Self {
        Self::default()
    }

    /// The candidates that may be dialled at `now`: those never tried, and those
    /// whose scheduled delay has elapsed. Excludes the given-up and the
    /// dismissed. Reads no clock — `now` is the caller's.
    pub fn due(&self, candidates: &[String], now: Instant) -> Vec<String> {
        candidates
            .iter()
            .filter(|addr| match self.states.get(*addr) {
                None => true,
                Some(state) => {
                    !state.given_up
                        && !state.dismissed
                        && state.next_attempt.is_none_or(|due| due <= now)
                },
            })
            .cloned()
            .collect()
    }

    /// Record a failed attempt: count it and move the next attempt out by the
    /// current backoff step. Once the ladder is exhausted the address is given
    /// up on and stops being due.
    pub fn record_failure(&mut self, addr: &str, now: Instant) {
        let state = self.states.entry(addr.to_string()).or_default();
        match next_delay(state.failures) {
            Some(delay) => state.next_attempt = Some(now + delay),
            // The ladder ran out: stop dialling until the user (or a speaker
            // turning up on its own) re-arms the address.
            None => state.given_up = true,
        }
        state.failures += 1;
    }

    /// Record that the address is connected again: its state is cleared
    /// entirely, so a later loss starts from the first backoff step.
    pub fn record_success(&mut self, addr: &str) {
        self.states.remove(addr);
    }

    /// The user disconnected this address from the app: stop dialling it, while
    /// leaving it in the intent so phase 6.3 still restores it if it comes back
    /// on its own.
    pub fn dismiss(&mut self, addr: &str) {
        self.states.entry(addr.to_string()).or_default().dismissed = true;
    }

    /// The user acted on this address (`/connect` or `/select`), or a fresh
    /// server started: clear the give-up **and** the dismissal, so it is due
    /// again immediately.
    pub fn rearm(&mut self, addr: &str) {
        self.states.remove(addr);
    }

    /// Whether the backend has stopped dialling this address.
    pub fn given_up(&self, addr: &str) -> bool {
        self.states.get(addr).is_some_and(|state| state.given_up)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "AA:BB:CC:DD:EE:FF";
    const B: &str = "11:22:33:44:55:66";
    const C: &str = "99:88:77:66:55:44";

    /// A `Vec<String>` from a slice of literals, for the address-list arguments.
    fn addrs(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// Fail `addr` `times` times in a row, letting each scheduled delay elapse.
    /// Returns the instant just after the last failure.
    fn fail_repeatedly(
        tracker: &mut ReconnectTracker,
        addr: &str,
        times: usize,
        start: Instant,
    ) -> Instant {
        let mut now = start;
        for step in 0..times {
            tracker.record_failure(addr, now);
            now += next_delay(step).unwrap_or(BACKOFF_CAP);
        }
        now
    }

    // ---- the backoff ladder ----

    // Criterion: `next_delay(failures)` yields 15 s, 30 s, 60 s, 120 s — the ramp,
    // table-driven and pure.
    #[test]
    fn test_next_delay_walks_the_backoff_ramp() {
        assert_eq!(next_delay(0), Some(Duration::from_secs(15)));
        assert_eq!(next_delay(1), Some(Duration::from_secs(30)));
        assert_eq!(next_delay(2), Some(Duration::from_secs(60)));
        assert_eq!(next_delay(3), Some(Duration::from_secs(120)));
    }

    // Criterion: after the ramp the interval caps at 300 s, and only three
    // attempts are made at that cap.
    #[test]
    fn test_next_delay_caps_at_five_minutes_for_three_attempts() {
        assert_eq!(next_delay(4), Some(Duration::from_secs(300)));
        assert_eq!(next_delay(5), Some(Duration::from_secs(300)));
        assert_eq!(next_delay(6), Some(Duration::from_secs(300)));
        assert_eq!(BACKOFF_CAP, Duration::from_secs(300));
        assert_eq!(ATTEMPTS_AT_CAP, 3);
    }

    // Criterion (non-nominal: backoff exhausted): past the cap attempts the
    // ladder ends — `None` means the address is given up on.
    #[test]
    fn test_next_delay_gives_up_after_the_cap_attempts() {
        assert_eq!(next_delay(BACKOFF_RAMP.len() + ATTEMPTS_AT_CAP), None);
        assert_eq!(next_delay(99), None, "past the ladder it stays given up");
    }

    // Criterion: the tick must not swallow the ladder's first rung — waking up
    // less often than the shortest delay would stretch every retry.
    #[test]
    fn test_reconnect_tick_is_no_longer_than_the_first_backoff_step() {
        let first = BACKOFF_RAMP.first().copied().unwrap_or(BACKOFF_CAP);
        assert!(
            RECONNECT_TICK <= first,
            "the pass wakes up every {RECONNECT_TICK:?}, which is coarser than {first:?}"
        );
    }

    // ---- candidate selection ----

    // Criterion: the candidates are the intent addresses that are paired and not
    // connected, **in intent order**.
    #[test]
    fn test_reconnect_candidates_keeps_the_paired_and_disconnected_in_intent_order() {
        let candidates = reconnect_candidates(&addrs(&[B, A]), &addrs(&[A, B, C]), &addrs(&[]));
        assert_eq!(
            candidates,
            addrs(&[B, A]),
            "the intent order is what the user picked, not the paired-list order"
        );
    }

    // Criterion (non-nominal: speaker already connected): a connected speaker is
    // never a candidate, so the periodic pass never re-dials it.
    #[test]
    fn test_reconnect_candidates_excludes_a_connected_speaker() {
        let candidates = reconnect_candidates(&addrs(&[A, B]), &addrs(&[A, B]), &addrs(&[A]));
        assert_eq!(candidates, addrs(&[B]), "A is already connected");
    }

    // Criterion (non-nominal: speaker unpaired outside the app): auto-reconnect
    // connects, it never pairs — an unpaired address is not a candidate.
    #[test]
    fn test_reconnect_candidates_excludes_an_unpaired_address() {
        let candidates = reconnect_candidates(&addrs(&[A, B]), &addrs(&[B]), &addrs(&[]));
        assert_eq!(
            candidates,
            addrs(&[B]),
            "A is no longer paired, so it must not be dialled"
        );
    }

    // Criterion: a paired, disconnected device the user never selected is not in
    // the intent, so it is not a candidate.
    #[test]
    fn test_reconnect_candidates_excludes_anything_outside_the_intent() {
        let candidates = reconnect_candidates(&addrs(&[A]), &addrs(&[A, B, C]), &addrs(&[]));
        assert_eq!(candidates, addrs(&[A]));
    }

    // Criterion (non-nominal: nothing remembered): an empty intent yields no
    // candidate at all.
    #[test]
    fn test_reconnect_candidates_with_an_empty_intent_is_empty() {
        assert!(reconnect_candidates(&[], &addrs(&[A, B]), &addrs(&[])).is_empty());
    }

    // Criterion: an empty paired list (no adapter, nothing bonded) yields no
    // candidate either.
    #[test]
    fn test_reconnect_candidates_with_no_paired_device_is_empty() {
        assert!(reconnect_candidates(&addrs(&[A, B]), &[], &addrs(&[])).is_empty());
    }

    // ---- the gate ----

    // Criterion (non-nominal: setting off): the gate short-circuits the whole
    // pass — off means no candidate is ever produced.
    #[test]
    fn test_should_auto_reconnect_off_produces_no_candidate() {
        assert!(!should_auto_reconnect(false), "off must stop the pass");
        assert!(should_auto_reconnect(true), "on must let the pass run");

        let intended = addrs(&[A]);
        let paired = addrs(&[A]);
        let candidates = if should_auto_reconnect(false) {
            reconnect_candidates(&intended, &paired, &[])
        } else {
            Vec::new()
        };
        assert!(
            candidates.is_empty(),
            "with the setting off the pass produces nothing to dial"
        );
    }

    // ---- the tracker ----

    // Criterion: `due` returns an address on its first pass — a fresh tracker
    // (server start) dials every candidate straight away.
    #[test]
    fn test_due_returns_every_candidate_on_the_first_pass() {
        let tracker = ReconnectTracker::new();
        assert_eq!(tracker.due(&addrs(&[A, B]), Instant::now()), addrs(&[A, B]));
    }

    // Criterion: after a failure the address is not due again until its
    // scheduled delay has elapsed; time is injected, never read from the clock.
    #[test]
    fn test_due_holds_an_address_back_until_its_delay_has_elapsed() {
        let now = Instant::now();
        let mut tracker = ReconnectTracker::new();
        tracker.record_failure(A, now);

        assert!(
            tracker.due(&addrs(&[A]), now).is_empty(),
            "a just-failed address must not be dialled again immediately"
        );
        assert!(
            tracker
                .due(&addrs(&[A]), now + Duration::from_secs(14))
                .is_empty(),
            "still inside the first 15 s step"
        );
        assert_eq!(
            tracker.due(&addrs(&[A]), now + Duration::from_secs(15)),
            addrs(&[A]),
            "the first step has elapsed, so it is due again"
        );
    }

    // Criterion: recording a failure moves the next attempt out by the *current*
    // backoff step — the ladder widens.
    #[test]
    fn test_each_failure_widens_the_next_attempt_by_the_current_step() {
        let mut now = Instant::now();
        let mut tracker = ReconnectTracker::new();

        for step in 0..BACKOFF_RAMP.len() {
            tracker.record_failure(A, now);
            let delay = next_delay(step).unwrap_or(BACKOFF_CAP);
            assert!(
                tracker
                    .due(&addrs(&[A]), now + delay - Duration::from_millis(1))
                    .is_empty(),
                "step {step} must hold the address for {delay:?}"
            );
            assert_eq!(
                tracker.due(&addrs(&[A]), now + delay),
                addrs(&[A]),
                "step {step} must release the address after {delay:?}"
            );
            now += delay;
        }
    }

    // Criterion: recording a success clears the address's state entirely, so a
    // later loss starts from the first step again.
    #[test]
    fn test_record_success_clears_the_backoff_so_a_later_loss_starts_over() {
        let now = Instant::now();
        let mut tracker = ReconnectTracker::new();
        let now = fail_repeatedly(&mut tracker, A, 3, now);

        tracker.record_success(A);
        assert_eq!(
            tracker.due(&addrs(&[A]), now),
            addrs(&[A]),
            "a cleared address is due at once"
        );

        tracker.record_failure(A, now);
        assert!(tracker
            .due(&addrs(&[A]), now + Duration::from_secs(14))
            .is_empty());
        assert_eq!(
            tracker.due(&addrs(&[A]), now + Duration::from_secs(15)),
            addrs(&[A]),
            "the ladder restarts at its first step, not where it left off"
        );
    }

    // Criterion (non-nominal: backoff exhausted): after the last cap attempt
    // fails, the address is given up on and `due` stops returning it.
    #[test]
    fn test_due_stops_returning_a_given_up_address() {
        let now = Instant::now();
        let mut tracker = ReconnectTracker::new();
        let attempts = BACKOFF_RAMP.len() + ATTEMPTS_AT_CAP + 1;
        let now = fail_repeatedly(&mut tracker, A, attempts, now);

        assert!(tracker.given_up(A), "the ladder ran out");
        assert!(
            tracker.due(&addrs(&[A]), now).is_empty(),
            "a given-up address is never dialled again"
        );
        assert!(
            tracker
                .due(&addrs(&[A]), now + Duration::from_secs(86_400))
                .is_empty(),
            "not even a day later — only an explicit re-arm brings it back"
        );
    }

    // Criterion (non-nominal: re-arming after giving up): `rearm` clears the
    // given-up state, so the address is due again immediately.
    #[test]
    fn test_rearm_makes_a_given_up_address_due_again() {
        let now = Instant::now();
        let mut tracker = ReconnectTracker::new();
        let attempts = BACKOFF_RAMP.len() + ATTEMPTS_AT_CAP + 1;
        let now = fail_repeatedly(&mut tracker, A, attempts, now);

        tracker.rearm(A);
        assert!(!tracker.given_up(A), "the user acted on it");
        assert_eq!(
            tracker.due(&addrs(&[A]), now),
            addrs(&[A]),
            "a re-armed address is due at once"
        );
    }

    // Criterion (non-nominal: re-arming after giving up): a speaker that turns up
    // connected on its own also clears the give-up, through `record_success`.
    #[test]
    fn test_record_success_clears_a_given_up_address() {
        let now = Instant::now();
        let mut tracker = ReconnectTracker::new();
        let attempts = BACKOFF_RAMP.len() + ATTEMPTS_AT_CAP + 1;
        let now = fail_repeatedly(&mut tracker, A, attempts, now);

        tracker.record_success(A);
        assert!(!tracker.given_up(A));
        assert_eq!(tracker.due(&addrs(&[A]), now), addrs(&[A]));
    }

    // Criterion (non-nominal: the user disconnects a speaker from the app):
    // `dismiss` removes the address from `due` while the caller keeps it in the
    // intent — the backend must not fight the user.
    #[test]
    fn test_dismiss_removes_an_address_from_due() {
        let now = Instant::now();
        let mut tracker = ReconnectTracker::new();
        tracker.dismiss(A);

        assert!(
            tracker.due(&addrs(&[A]), now).is_empty(),
            "a dismissed address must not be dialled"
        );
        assert!(
            tracker
                .due(&addrs(&[A]), now + Duration::from_secs(3_600))
                .is_empty(),
            "and no delay brings it back on its own"
        );
    }

    // Criterion: `rearm` clears the dismissal too — re-selecting or reconnecting
    // from the app brings the address back into the pass.
    #[test]
    fn test_rearm_after_a_dismissal_brings_the_address_back() {
        let now = Instant::now();
        let mut tracker = ReconnectTracker::new();
        tracker.dismiss(A);
        tracker.rearm(A);

        assert_eq!(
            tracker.due(&addrs(&[A]), now),
            addrs(&[A]),
            "the user re-selected it, so the backend may dial it again"
        );
    }

    // Criterion: dismissing one address leaves the others alone.
    #[test]
    fn test_dismiss_only_suppresses_that_address() {
        let now = Instant::now();
        let mut tracker = ReconnectTracker::new();
        tracker.dismiss(A);
        assert_eq!(tracker.due(&addrs(&[A, B]), now), addrs(&[B]));
    }

    // Criterion (non-nominal: two remembered speakers, one back): the backoff
    // state is per address — clearing the returning one leaves the other's
    // schedule untouched.
    #[test]
    fn test_backoff_state_is_kept_per_address() {
        let now = Instant::now();
        let mut tracker = ReconnectTracker::new();
        tracker.record_failure(A, now);
        tracker.record_failure(B, now);

        tracker.record_success(A);
        assert_eq!(
            tracker.due(&addrs(&[A, B]), now),
            addrs(&[A]),
            "A came back; B keeps its own 15 s wait"
        );
        assert_eq!(
            tracker.due(&addrs(&[A, B]), now + Duration::from_secs(15)),
            addrs(&[A, B]),
            "B is due once its own step elapsed"
        );
    }

    // Criterion: a fresh tracker (the next server start) re-arms everything — no
    // give-up survives a restart.
    #[test]
    fn test_a_fresh_tracker_dials_a_previously_given_up_address() {
        let now = Instant::now();
        let mut tracker = ReconnectTracker::new();
        let attempts = BACKOFF_RAMP.len() + ATTEMPTS_AT_CAP + 1;
        let now = fail_repeatedly(&mut tracker, A, attempts, now);
        assert!(tracker.given_up(A));

        let restarted = ReconnectTracker::new();
        assert_eq!(
            restarted.due(&addrs(&[A]), now),
            addrs(&[A]),
            "the tracker is in-memory: a restart is a clean slate"
        );
    }

    // Criterion: `due` only ever reports addresses the caller offered — an
    // address that left the intent (deselected) stops being a candidate with no
    // dismissal bookkeeping.
    #[test]
    fn test_due_never_returns_an_address_outside_the_candidates() {
        let now = Instant::now();
        let mut tracker = ReconnectTracker::new();
        tracker.record_failure(A, now);
        tracker.record_success(A);
        assert!(
            tracker.due(&addrs(&[B]), now).iter().all(|a| a == B),
            "only the offered candidates may come back"
        );
        assert!(tracker.due(&[], now).is_empty(), "no candidate, no work");
    }
}
