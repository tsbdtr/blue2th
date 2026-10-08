// SPDX-License-Identifier: MIT OR Apache-2.0

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

    /// Record that the address is connected again: its retry ladder is cleared,
    /// so a later loss starts from the first backoff step.
    ///
    /// A **dismissal is kept**: only the user takes back a hang-up, through
    /// [`rearm`](Self::rearm). Dropping it here would let a stale `/devices`
    /// listing — one taken just before the disconnect landed — silently re-arm
    /// the address the user has just hung up, and the pass would dial it
    /// straight back.
    pub fn record_success(&mut self, addr: &str) {
        let Some(previous) = self.states.remove(addr) else {
            return;
        };
        if previous.dismissed {
            self.states.insert(
                addr.to_string(),
                AddressState {
                    dismissed: true,
                    ..AddressState::default()
                },
            );
        }
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

    /// The user switched auto-reconnect back on: every address gets a fresh
    /// ladder, so nothing stays given up on or dismissed from before the setting
    /// was turned off. Without it the toggle would look inert on exactly the
    /// speaker the user turned it back on for.
    pub fn rearm_all(&mut self) {
        self.states.clear();
    }

    /// Whether the backend has stopped dialling this address.
    pub fn given_up(&self, addr: &str) -> bool {
        self.states.get(addr).is_some_and(|state| state.given_up)
    }
}

#[cfg(test)]
mod tests;
