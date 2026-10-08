// SPDX-License-Identifier: MIT OR Apache-2.0

//! Spotify Connect volume policy (#58).
//!
//! `librespot` applies the Connect volume before PipeWire sees a sample, and
//! every respawn (`--initial-volume 100`) snaps it back to full scale. This
//! module remembers the level the user chose — from any client, since the poll
//! adopts what it observes — and says when to write it back. It is **pure**: it
//! never calls the Web API, never reads the clock, and never spawns anything.
//! The caller feeds it what `GET /me/player` reported and tells it when a write
//! succeeded, exactly as `reconnect::ReconnectTracker` takes its `Instant` from
//! outside — which is the only way the respawn/restore dance is testable.
//!
//! With the lock on, the policy turns into a guard: the level is pinned to 100
//! and re-asserted whenever the poll sees anything else.

/// The level the lock pins the Connect device to.
const LOCKED_LEVEL: u8 = 100;

/// The remembered Connect level and what the next poll should do about it.
#[derive(Debug, Default)]
pub struct Policy {
    /// The level the user last chose, from any client; `None` until one is
    /// seen. It is what a respawn restores, and what the lock overrides.
    desired: Option<u8>,
    /// Whether `librespot` was respawned since the last successful write. The
    /// mark is what tells "librespot reset it" from "someone chose it": with
    /// it armed, a level differing from `desired` is written back; without it,
    /// the same observation is a client change and is adopted.
    respawn_pending: bool,
    /// Whether the level is pinned to [`LOCKED_LEVEL`].
    lock: bool,
}

impl Policy {
    /// A fresh policy: no level known, no respawn pending, lock off.
    pub fn new() -> Self {
        Self::default()
    }

    /// Turn the lock on or off. Only the edge acts: the app re-pushes the whole
    /// config on activation, and re-applying an unchanged lock must not forget
    /// the remembered level or a pending restore.
    pub fn set_lock(&mut self, on: bool) {
        if self.lock == on {
            return;
        }
        self.lock = on;
        // On: the next poll re-asserts 100. Off: the pinned 100 must not linger
        // as a "choice", so the next observed level is adopted without a write.
        self.desired = on.then_some(LOCKED_LEVEL);
    }

    /// The user chose a level over `POST /spotify/volume`: clamp it to
    /// `0..=100`, remember it, and return what was stored. A locked policy
    /// keeps its 100 — the route refuses the request before it gets here, and
    /// the policy stays consistent even if it did not.
    pub fn on_user_set(&mut self, percent: u8) -> u8 {
        let clamped = percent.min(LOCKED_LEVEL);
        if !self.lock {
            self.desired = Some(clamped);
        }
        clamped
    }

    /// `librespot` was respawned, so its level is back at 100 whatever the user
    /// chose: the next observation restores the remembered level.
    pub fn mark_respawned(&mut self) {
        self.respawn_pending = true;
    }

    /// The poll reported a level (`None` when there is no device / a 204).
    /// Returns the level to write, or `None` when nothing is to be done.
    ///
    /// An absent level changes nothing, the mark included: the restore waits
    /// for a device to be back rather than being forgotten.
    pub fn on_observed(&mut self, observed: Option<u8>) -> Option<u8> {
        let observed = observed?;
        if self.lock {
            self.desired = Some(LOCKED_LEVEL);
            if observed == LOCKED_LEVEL {
                self.respawn_pending = false;
                return None;
            }
            return Some(LOCKED_LEVEL);
        }
        match self.desired {
            Some(desired) if self.respawn_pending => {
                if observed == desired {
                    self.respawn_pending = false;
                    None
                } else {
                    // Kept armed until `written()`: a failed write is retried.
                    Some(desired)
                }
            },
            // No respawn pending (or nothing to restore): whatever is observed
            // is the user's choice, made in some client, and is not fought.
            _ => {
                self.desired = Some(observed);
                self.respawn_pending = false;
                None
            },
        }
    }

    /// The write returned by [`Policy::on_observed`] succeeded.
    pub fn written(&mut self) {
        self.respawn_pending = false;
    }

    /// The level the policy wants `librespot` at, once one is known.
    pub fn desired(&self) -> Option<u8> {
        self.desired
    }
}

#[cfg(test)]
mod tests;
