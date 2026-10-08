// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pure playback-target selection model for phase 4 (fan-out to two speakers).
//!
//! `SpeakerTargets` tracks which connected speakers the user picked as playback
//! targets (capped at two), each speaker's latency offset, and derives the
//! [`RoutingMode`] from the selection count. It performs **no I/O**: validation
//! against the live connection state takes a slice of connected addresses passed
//! by the route layer, and the actual PipeWire routing lives in `audio.rs`.

use std::collections::HashMap;

use blue2th_proto::{NowPlayingState, RoutingMode, SpeakerTarget, TargetsState};

/// Maximum number of speakers that can be selected as playback targets at once.
pub const MAX_TARGETS: usize = 2;

/// Inclusive upper bound for a per-speaker latency offset, in milliseconds.
///
/// Offsets are additive-only (a branch can add delay, never take it away),
/// so the lower bound is `0`. Mirrors `clamp_volume`'s clamp-don't-reject policy.
pub const MAX_OFFSET_MS: u32 = 750;

/// Clamp a requested per-speaker latency offset into `0..=MAX_OFFSET_MS` ms.
pub fn clamp_offset(ms: u32) -> u32 {
    ms.min(MAX_OFFSET_MS)
}

/// What the loss of the last selected speaker calls for (#67).
///
/// Three outcomes, because the two paths that empty the selection do not want
/// the same answer: only one of them promises to bring the speaker back, and a
/// boolean has no room for the difference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LastLossAction {
    /// Leave everything alone.
    Nothing,
    /// Pause both sources and touch the routing not at all: the speaker is
    /// coming back, so the pause has to be resumable and nothing may be
    /// destroyed under a still-running stream.
    PauseSources,
    /// Quieten the sources and tear the routing down: nothing will re-select the
    /// speaker, so the graph must stop pointing at it.
    QuietenAndTeardown,
}

/// Which action the loss of the last selected speaker calls for. Pure.
///
/// Pruning the selection leaves the audio graph alone, and PipeWire re-attaches a
/// returning device's sink, so a stream left running would resume on a device
/// that is no longer selected. The setting picks *how* to silence it: it promises
/// to bring the speaker back, so the sources are merely paused and the routing is
/// left for them to come back to; with it off nothing will re-select the speaker,
/// so the graph must stop pointing at it.
pub fn action_on_last_loss(
    lost_last_target: bool,
    restore_during_playback: bool,
) -> LastLossAction {
    if !lost_last_target {
        return LastLossAction::Nothing;
    }
    if restore_during_playback {
        LastLossAction::PauseSources
    } else {
        LastLossAction::QuietenAndTeardown
    }
}

/// Whether a restoration may resume the sources it finds paused (#67).
///
/// Only a pause the backend performed may be undone by the backend: an explicit
/// transport command from the app clears that claim, so a pause the user asked
/// for survives a speaker coming back. Pure.
pub fn should_resume_after_restore(backend_paused_sources: bool) -> bool {
    backend_paused_sources
}

/// Whether the backend may claim the pause it just performed (#67).
///
/// The claim means "the backend silenced a source that was playing", and only a
/// claim licenses a restoration to resume. Clearing it on an explicit transport
/// command is not enough on its own: the loss path runs afterwards and would
/// re-claim a pause it never performed, undoing a pause the user asked for. So
/// each source reports whether it really silenced anything. The engine's pause is
/// a no-op unless it was `Playing`, so its own status answers for it; Spotify's
/// half cannot come from the pause call — see [`spotify_was_playing`] — and comes
/// from the state observed beforehand. Pure.
pub fn may_claim_pause(spotify_silenced: bool, engine_silenced: bool) -> bool {
    spotify_silenced || engine_silenced
}

/// Whether Spotify was playing, from a `now_playing()` snapshot taken **before**
/// the pause (#67).
///
/// The pause call itself cannot answer this: `transport(Pause)` reports success on
/// any 2xx, and Spotify answers 2xx to a pause on a player that is already paused,
/// so a call that went through proves nothing about what it stopped. Only the state
/// observed beforehand does — which is why this takes a snapshot rather than a
/// result. Pure.
pub fn spotify_was_playing(state: NowPlayingState) -> bool {
    matches!(state, NowPlayingState::Playing)
}

/// Whether a returning speaker may be re-selected right now (phase 6.3).
///
/// Restoring mid-playback puts the returning speaker's branch back into the live
/// graph, so it starts playing again under the user's hands: that is opt-in. With
/// playback stopped the restoration is free and always allowed. Pure.
pub fn should_restore(playing: bool, restore_during_playback: bool) -> bool {
    !playing || restore_during_playback
}

/// File holding the remembered offsets, under the app's state directory.
const OFFSETS_STORE_FILE: &str = "offsets.json";

/// Path of the file remembering each speaker's tuned offset:
/// `$XDG_STATE_HOME/blue2th/offsets.json` (or `~/.local/state/blue2th/offsets.json`).
/// `None` when neither variable is set, in which case offsets stay in memory only.
pub fn offsets_store_path() -> Option<std::path::PathBuf> {
    crate::state_store::state_store_path(OFFSETS_STORE_FILE)
}

/// Read the remembered `address → offset_ms` table. A missing, unreadable or
/// malformed file simply means "nothing remembered yet" — never an error.
///
/// Values are clamped on the way in: a hand-edited file must not bypass the bound.
/// Production reads the whole store in one go (see [`SpeakerTargets::with_store`]);
/// this half-view exists for the tests that assert on the offsets alone.
#[cfg(test)]
fn load_offsets(path: Option<&std::path::Path>) -> HashMap<String, u32> {
    load_store(path).offsets
}

/// On-disk shape of the store: the tuned offsets plus the playback intent.
///
/// A phase 6.1 store is a bare `{address: ms}` map and is still read: it is tried
/// **first**, because serde ignores unknown fields, so that shape would otherwise
/// deserialize into an all-default `StoredTargets` and silently lose every tuned
/// offset.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct StoredTargets {
    #[serde(default)]
    offsets: HashMap<String, u32>,
    #[serde(default)]
    intended: Vec<String>,
}

/// Read the whole store. A missing, unreadable or malformed file simply means
/// "nothing remembered yet" — never an error. Offsets are clamped on the way in:
/// a hand-edited file must not bypass the bound.
fn load_store(path: Option<&std::path::Path>) -> StoredTargets {
    let Some(path) = path else {
        return StoredTargets::default();
    };
    let Ok(raw) = std::fs::read_to_string(path) else {
        return StoredTargets::default();
    };
    let mut stored = match serde_json::from_str::<HashMap<String, u32>>(&raw) {
        // Phase 6.1 shape: offsets only, no intent.
        Ok(offsets) => StoredTargets {
            offsets,
            intended: Vec::new(),
        },
        Err(_) => serde_json::from_str::<StoredTargets>(&raw).unwrap_or_default(),
    };
    stored.offsets = stored
        .offsets
        .into_iter()
        .map(|(addr, ms)| (addr, clamp_offset(ms)))
        .collect();
    stored
}

/// Seed or update the offsets alone, preserving the intent already on disk.
///
/// Test-only: production always writes both halves at once through [`save_store`],
/// which is exactly what this delegates to — so the on-disk format a test seeds is
/// the one the server writes. The phase 6.1 shape is pinned separately, by the
/// tests that hand-write a bare `{address: ms}` map.
#[cfg(test)]
fn save_offsets(path: &std::path::Path, offsets: &HashMap<String, u32>) -> std::io::Result<()> {
    // Preserve whatever intent is already on disk: the two halves share one file,
    // so writing offsets alone would drop it.
    let intended = load_store(Some(path)).intended;
    save_store(path, offsets, &intended)
}

/// Persist both halves of the store in one write, so neither can clobber the
/// other.
fn save_store(
    path: &std::path::Path,
    offsets: &HashMap<String, u32>,
    intended: &[String],
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let stored = StoredTargets {
        // Owned copies: the on-disk shape is serialized from its own values.
        offsets: offsets.clone(),
        intended: intended.to_vec(),
    };
    let body = serde_json::to_string(&stored).map_err(std::io::Error::other)?;
    std::fs::write(path, body)
}

/// Why a `select` request was rejected. The route layer maps this to a 4xx.
#[derive(Debug, PartialEq, Eq)]
pub enum SelectError {
    /// The address is not among the currently connected speakers.
    NotConnected,
    /// Two speakers are already selected; the cap was exceeded.
    CapExceeded,
}

/// In-memory selection of playback targets and their offsets. Held behind the
/// router's `Arc<Mutex<_>>`; the route layer keeps it in sync with the live
/// connection state.
#[derive(Debug, Default)]
pub struct SpeakerTargets {
    /// Selected targets in selection order (capped at `MAX_TARGETS`).
    speakers: Vec<SpeakerTarget>,
    /// The **intent**: addresses the user asked to play on, in the order they
    /// were picked. Distinct from `speakers`, which is the live selection: losing
    /// the radio prunes the latter and leaves this one alone, so a speaker that
    /// comes back can be re-selected on its own (phase 6.3). Only an explicit
    /// `deselect` clears an entry.
    intended: Vec<String>,
    /// Last offset tuned for a speaker, by address — kept across deselection and
    /// (through `store`) across restarts. Not part of the API surface.
    remembered: HashMap<String, u32>,
    /// Where the remembered offsets are persisted, or `None` to stay in memory.
    store: Option<std::path::PathBuf>,
}

impl SpeakerTargets {
    /// A fresh, empty selection (routing mode `Idle`), with no store: it performs
    /// no I/O, so a test run can never read or clobber the real user's file.
    pub fn new() -> Self {
        Self::default()
    }

    /// A selection backed by a store, loaded on construction: the tuned offsets
    /// (phase 6.1) and the playback intent (phase 6.3), so a speaker reconnecting
    /// after a restart is re-selected on its own. The live selection itself always
    /// starts empty — nothing is connected yet at startup.
    pub fn with_store(store: Option<std::path::PathBuf>) -> Self {
        // One read for both halves: they share a file, so reading it twice would
        // double the startup I/O and could see two different versions of it.
        let stored = load_store(store.as_deref());
        Self {
            speakers: Vec::new(),
            intended: stored.intended,
            remembered: stored.offsets,
            store,
        }
    }

    /// The remembered addresses that are connected again and not currently
    /// selected, in remembered order, capped so the total selection never exceeds
    /// [`MAX_TARGETS`]. Pure: it never evicts a speaker the user picked by hand.
    pub fn restorable(&self, connected: &[String]) -> Vec<String> {
        let free = MAX_TARGETS.saturating_sub(self.speakers.len());
        self.intended
            .iter()
            .filter(|addr| connected.iter().any(|c| c == *addr))
            .filter(|addr| !self.speakers.iter().any(|s| &&s.address == addr))
            .take(free)
            .cloned()
            .collect()
    }

    /// Re-select the remembered speakers that came back, returning whether the
    /// selection actually changed. The caller re-routes only on `true`:
    /// `sync_connected` runs on every `/devices` poll, so a second call with the
    /// same input must report `false` rather than rebuild the PipeWire graph.
    pub fn restore(&mut self, connected: &[String]) -> bool {
        let mut changed = false;
        for addr in self.restorable(connected) {
            // Already filtered by `restorable`, so this cannot be rejected; a
            // rejection would simply leave that speaker out rather than
            // propagate — and must not claim a change that did not happen, since
            // the caller rebuilds the whole PipeWire graph on `true`.
            changed |= self.select(&addr, connected).is_ok();
        }
        changed
    }

    /// Select `addr` as a playback target. Rejects an address that is not in
    /// `connected`, rejects a third selection (cap 2), and is idempotent for an
    /// already-selected address. A freshly selected speaker starts at its
    /// remembered offset, or `0` when nothing was ever tuned for it.
    pub fn select(&mut self, addr: &str, connected: &[String]) -> Result<(), SelectError> {
        if self.speakers.iter().any(|s| s.address == addr) {
            // Idempotent: already selected.
            return Ok(());
        }
        if !connected.iter().any(|c| c == addr) {
            return Err(SelectError::NotConnected);
        }
        if self.speakers.len() >= MAX_TARGETS {
            return Err(SelectError::CapExceeded);
        }
        // A hand-edited store could hold anything: clamp on restore too.
        let offset_ms = self
            .remembered
            .get(addr)
            .copied()
            .map(clamp_offset)
            .unwrap_or(0);
        self.speakers.push(SpeakerTarget {
            address: addr.to_string(),
            offset_ms,
        });
        // Record the intent: a speaker that later drops off the radio is pruned
        // from the selection but must still be wanted when it comes back.
        if !self.intended.iter().any(|a| a == addr) {
            self.intended.push(addr.to_string());
            self.persist();
        }
        Ok(())
    }

    /// Remove `addr` from the selection (no-op if it was not selected).
    pub fn deselect(&mut self, addr: &str) {
        self.speakers.retain(|s| s.address != addr);
        // The only thing that clears the intent: losing the radio must not, or a
        // speaker going flat would be forgotten rather than restored.
        let before = self.intended.len();
        self.intended.retain(|a| a != addr);
        if self.intended.len() != before {
            self.persist();
        }
    }

    /// Set the per-speaker offset (clamped to `0..=MAX_OFFSET_MS`). No-op if the
    /// address is not currently selected. The clamped value is remembered for that
    /// address and persisted, so it survives a deselection and a restart.
    pub fn set_offset(&mut self, addr: &str, ms: u32) {
        let Some(target) = self.speakers.iter_mut().find(|s| s.address == addr) else {
            return;
        };
        let offset_ms = clamp_offset(ms);
        target.offset_ms = offset_ms;
        self.remembered.insert(addr.to_string(), offset_ms);
        self.persist();
    }

    /// Write the remembered offsets to the store, if this selection has one. A
    /// failure is logged, never propagated: losing the tuning across a restart
    /// must not turn a slider drag into an error response.
    fn persist(&self) {
        let Some(path) = self.store.as_deref() else {
            return;
        };
        if let Err(e) = save_store(path, &self.remembered, &self.intended) {
            tracing::warn!("could not persist the speaker targets: {e}");
        }
    }

    /// Drop any selected target that is no longer in `connected`, then recompute
    /// the routing mode. Returns the (possibly changed) routing mode.
    pub fn retain_connected(&mut self, connected: &[String]) -> RoutingMode {
        self.speakers
            .retain(|s| connected.iter().any(|c| c == &s.address));
        self.routing_mode()
    }

    /// Routing mode derived from the selection count: 0 → Idle, 1 → Single, 2 →
    /// Combined.
    pub fn routing_mode(&self) -> RoutingMode {
        match self.speakers.len() {
            0 => RoutingMode::Idle,
            1 => RoutingMode::Single,
            _ => RoutingMode::Combined,
        }
    }

    /// The selected targets in selection order.
    pub fn speakers(&self) -> Vec<SpeakerTarget> {
        // Clone: callers inspect the snapshot while the selection stays owned here.
        self.speakers.clone()
    }

    /// The persisted playback **intent**, in the order the user picked it:
    /// exactly the addresses phase 6.5's auto-reconnect pass may dial back.
    ///
    /// Read-only — the selection semantics are unchanged, this only exposes what
    /// is already on disk.
    pub fn intended(&self) -> Vec<String> {
        // Clone: the intent stays owned here while the caller filters the snapshot.
        self.intended.clone()
    }

    /// Snapshot for `GET /targets`.
    pub fn state(&self) -> TargetsState {
        TargetsState {
            speakers: self.speakers(),
            routing: self.routing_mode(),
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod reconnect_intent_tests;
