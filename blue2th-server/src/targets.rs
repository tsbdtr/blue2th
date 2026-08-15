//! Pure playback-target selection model for phase 4 (fan-out to two speakers).
//!
//! `SpeakerTargets` tracks which connected speakers the user picked as playback
//! targets (capped at two), each speaker's latency offset, and derives the
//! [`RoutingMode`] from the selection count. It performs **no I/O**: validation
//! against the live connection state takes a slice of connected addresses passed
//! by the route layer, and the actual PipeWire routing lives in `audio.rs`.

use std::collections::HashMap;

use blue2th_proto::{RoutingMode, SpeakerTarget, TargetsState};

/// Maximum number of speakers that can be selected as playback targets at once.
pub const MAX_TARGETS: usize = 2;

/// Inclusive upper bound for a per-speaker latency offset, in milliseconds.
///
/// Offsets are additive-only (a loopback branch can be delayed, not advanced),
/// so the lower bound is `0`. Mirrors `clamp_volume`'s clamp-don't-reject policy.
pub const MAX_OFFSET_MS: u32 = 750;

/// Clamp a requested per-speaker latency offset into `0..=MAX_OFFSET_MS` ms.
pub fn clamp_offset(ms: u32) -> u32 {
    ms.min(MAX_OFFSET_MS)
}

/// Whether a returning speaker may be re-selected right now (phase 6.3).
///
/// Restoring mid-playback can move the target sink, which respawns `librespot`
/// and cuts the sound for a moment: that is opt-in. With playback stopped the
/// restoration is free and always allowed. Pure.
/// Whether losing the last selected device should quieten the stream (phase 6.3).
///
/// Pruning the selection leaves the audio graph alone, and PipeWire re-attaches a
/// returning device's sink, so a stream left running would resume on a device
/// that is no longer selected. Quietening is skipped when the setting will bring
/// the device back on its own: pausing then would leave it silent until the user
/// pressed play. Pure.
pub fn should_quieten_on_last_loss(lost_last_target: bool, restore_during_playback: bool) -> bool {
    lost_last_target && !restore_during_playback
}

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

    /// Snapshot for `GET /targets`.
    pub fn state(&self) -> TargetsState {
        TargetsState {
            speakers: self.speakers(),
            routing: self.routing_mode(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connected(addrs: &[&str]) -> Vec<String> {
        addrs.iter().map(|s| s.to_string()).collect()
    }

    const A: &str = "AA:BB:CC:DD:EE:FF";
    const B: &str = "11:22:33:44:55:66";
    const C: &str = "99:88:77:66:55:44";

    // Criterion: a connected speaker can be selected as a playback target.
    #[test]
    fn test_select_connected_speaker_succeeds() {
        let mut targets = SpeakerTargets::new();
        let result = targets.select(A, &connected(&[A]));
        assert_eq!(result, Ok(()));
        assert_eq!(targets.speakers().len(), 1);
        assert_eq!(targets.speakers()[0].address, A);
        // A freshly selected speaker starts at offset 0.
        assert_eq!(targets.speakers()[0].offset_ms, 0);
    }

    // Criterion: selecting a non-connected speaker is rejected; selection unchanged.
    #[test]
    fn test_select_non_connected_speaker_is_rejected() {
        let mut targets = SpeakerTargets::new();
        let result = targets.select(A, &connected(&[B]));
        assert_eq!(result, Err(SelectError::NotConnected));
        assert!(targets.speakers().is_empty());
    }

    // Criterion: at most two speakers can be selected; selecting a third is rejected.
    #[test]
    fn test_select_third_speaker_exceeds_cap() {
        let mut targets = SpeakerTargets::new();
        targets
            .select(A, &connected(&[A, B, C]))
            .expect("select first");
        targets
            .select(B, &connected(&[A, B, C]))
            .expect("select second");
        let result = targets.select(C, &connected(&[A, B, C]));
        assert_eq!(result, Err(SelectError::CapExceeded));
        // Selection unchanged: still exactly the first two.
        assert_eq!(targets.speakers().len(), 2);
    }

    // Criterion: selecting an already-selected speaker is idempotent (no duplicate).
    #[test]
    fn test_select_already_selected_is_idempotent() {
        let mut targets = SpeakerTargets::new();
        targets.select(A, &connected(&[A, B])).expect("select once");
        targets
            .select(A, &connected(&[A, B]))
            .expect("re-select is idempotent");
        assert_eq!(targets.speakers().len(), 1);
    }

    // Criterion: deselecting removes the speaker from the selection.
    #[test]
    fn test_deselect_removes_speaker() {
        let mut targets = SpeakerTargets::new();
        targets.select(A, &connected(&[A, B])).expect("select A");
        targets.select(B, &connected(&[A, B])).expect("select B");
        targets.deselect(A);
        let addrs: Vec<String> = targets.speakers().into_iter().map(|s| s.address).collect();
        assert_eq!(addrs, vec![B.to_string()]);
    }

    // Criterion: a per-speaker latency offset can be set and is reflected.
    #[test]
    fn test_set_offset_updates_selected_speaker() {
        let mut targets = SpeakerTargets::new();
        targets.select(A, &connected(&[A])).expect("select A");
        targets.set_offset(A, 200);
        assert_eq!(targets.speakers()[0].offset_ms, 200);
    }

    // Criterion: a per-speaker latency offset is clamped to `0..=750` ms.
    #[test]
    fn test_set_offset_above_range_is_clamped() {
        let mut targets = SpeakerTargets::new();
        targets.select(A, &connected(&[A])).expect("select A");
        targets.set_offset(A, 5000);
        assert_eq!(targets.speakers()[0].offset_ms, MAX_OFFSET_MS);
    }

    // Criterion: the routing mode is derived from the selection count — 0 → Idle.
    #[test]
    fn test_routing_mode_idle_when_empty() {
        let targets = SpeakerTargets::new();
        assert_eq!(targets.routing_mode(), RoutingMode::Idle);
    }

    // Criterion: the routing mode is derived from the selection count — 1 → Single.
    #[test]
    fn test_routing_mode_single_with_one_target() {
        let mut targets = SpeakerTargets::new();
        targets.select(A, &connected(&[A])).expect("select A");
        assert_eq!(targets.routing_mode(), RoutingMode::Single);
    }

    // Criterion: the routing mode is derived from the selection count — 2 → Combined.
    #[test]
    fn test_routing_mode_combined_with_two_targets() {
        let mut targets = SpeakerTargets::new();
        targets.select(A, &connected(&[A, B])).expect("select A");
        targets.select(B, &connected(&[A, B])).expect("select B");
        assert_eq!(targets.routing_mode(), RoutingMode::Combined);
    }

    // Criterion: when a selected target disconnects, it is dropped and the routing
    // mode is recomputed to Single (one remaining connected target).
    #[test]
    fn test_retain_connected_drops_disconnected_target_to_single() {
        let mut targets = SpeakerTargets::new();
        targets.select(A, &connected(&[A, B])).expect("select A");
        targets.select(B, &connected(&[A, B])).expect("select B");
        // B disconnects: only A remains connected.
        let mode = targets.retain_connected(&connected(&[A]));
        assert_eq!(mode, RoutingMode::Single);
        let addrs: Vec<String> = targets.speakers().into_iter().map(|s| s.address).collect();
        assert_eq!(addrs, vec![A.to_string()]);
    }

    // Criterion: when all selected targets disconnect, routing falls back to Idle.
    #[test]
    fn test_retain_connected_drops_all_to_idle() {
        let mut targets = SpeakerTargets::new();
        targets.select(A, &connected(&[A, B])).expect("select A");
        targets.select(B, &connected(&[A, B])).expect("select B");
        let mode = targets.retain_connected(&connected(&[]));
        assert_eq!(mode, RoutingMode::Idle);
        assert!(targets.speakers().is_empty());
    }

    // Criterion: `GET /targets` returns the current selection, per-speaker offsets
    // and routing mode — `state()` reflects all three.
    #[test]
    fn test_state_reflects_selection_offsets_and_routing() {
        let mut targets = SpeakerTargets::new();
        targets.select(A, &connected(&[A, B])).expect("select A");
        targets.select(B, &connected(&[A, B])).expect("select B");
        targets.set_offset(B, 250);

        let state = targets.state();
        assert_eq!(state.routing, RoutingMode::Combined);
        assert_eq!(state.speakers.len(), 2);
        let b = state
            .speakers
            .iter()
            .find(|s| s.address == B)
            .expect("B is in the selection");
        assert_eq!(b.offset_ms, 250);
    }

    // Edge case: setting an offset on an address that is not selected is a no-op
    // (no panic, no phantom entry inserted).
    #[test]
    fn test_set_offset_on_unselected_address_is_noop() {
        let mut targets = SpeakerTargets::new();
        targets.select(A, &connected(&[A])).expect("select A");
        targets.set_offset(B, 300);
        assert_eq!(targets.speakers().len(), 1);
        assert_eq!(targets.speakers()[0].offset_ms, 0);
    }

    // Edge case: re-selecting an already-selected speaker is idempotent even once
    // it is no longer reported as connected, and it preserves its current offset.
    #[test]
    fn test_reselect_is_idempotent_even_if_no_longer_connected() {
        let mut targets = SpeakerTargets::new();
        targets.select(A, &connected(&[A])).expect("select A");
        targets.set_offset(A, 400);
        // A is now absent from the connected list, but it is already selected.
        let result = targets.select(A, &connected(&[]));
        assert_eq!(result, Ok(()));
        assert_eq!(targets.speakers().len(), 1);
        // The offset set earlier is preserved, not reset to 0.
        assert_eq!(targets.speakers()[0].offset_ms, 400);
    }

    // Edge case: deselecting an address that was never selected is a harmless
    // no-op that leaves the rest of the selection intact.
    #[test]
    fn test_deselect_unselected_address_is_noop() {
        let mut targets = SpeakerTargets::new();
        targets.select(A, &connected(&[A])).expect("select A");
        targets.deselect(B);
        let addrs: Vec<String> = targets.speakers().into_iter().map(|s| s.address).collect();
        assert_eq!(addrs, vec![A.to_string()]);
    }

    // Criterion: a per-speaker latency offset is clamped to `0..=750` ms —
    // clamp_offset: above 750 saturates.
    #[test]
    fn test_clamp_offset_above_range_saturates() {
        assert_eq!(clamp_offset(751), MAX_OFFSET_MS);
        assert_eq!(clamp_offset(10_000), MAX_OFFSET_MS);
    }

    // Criterion: clamp_offset: in-range values (including 0) are unchanged.
    #[test]
    fn test_clamp_offset_in_range_is_unchanged() {
        assert_eq!(clamp_offset(0), 0);
        assert_eq!(clamp_offset(123), 123);
    }

    // Criterion: clamp_offset: the 750 boundary is kept.
    #[test]
    fn test_clamp_offset_boundary_is_kept() {
        assert_eq!(clamp_offset(MAX_OFFSET_MS), MAX_OFFSET_MS);
    }

    // ---- phase 6.1: remembered offsets across restarts ----

    /// A private, per-test store path under the system temp dir. Never the real
    /// user's file: the unit tests own the filesystem seam entirely.
    fn store_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("blue2th-test-offsets-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create the test store dir");
        dir.join("offsets.json")
    }

    /// Build a remembered table from `(address, offset)` pairs.
    fn table(entries: &[(&str, u32)]) -> HashMap<String, u32> {
        entries
            .iter()
            .map(|(addr, ms)| ((*addr).to_string(), *ms))
            .collect()
    }

    // Criterion: `save_offsets` then `load_offsets` on the same path round-trips
    // the address → offset table.
    #[test]
    fn test_save_then_load_offsets_round_trips_the_table() {
        let path = store_path("roundtrip");
        let offsets = table(&[(A, 750), (B, 120)]);

        save_offsets(&path, &offsets).expect("save the offsets");
        assert_eq!(load_offsets(Some(&path)), offsets);

        let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
    }

    // Criterion: `load_offsets` on a missing file (first run) yields an empty
    // table rather than an error, and no path at all reads nothing.
    #[test]
    fn test_load_offsets_missing_file_is_empty() {
        let path = store_path("missing");
        let _ = std::fs::remove_file(&path);
        assert!(load_offsets(Some(&path)).is_empty());
        assert!(load_offsets(None).is_empty());
        let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
    }

    // Criterion: `load_offsets` on an empty, non-JSON or malformed file yields an
    // empty table rather than an error (a hand-edited file must never fail a
    // request or the server's startup).
    #[test]
    fn test_load_offsets_malformed_file_is_empty() {
        let path = store_path("malformed");

        std::fs::write(&path, "").expect("write the empty store");
        assert!(load_offsets(Some(&path)).is_empty(), "empty file");

        std::fs::write(&path, "not json").expect("write the non-json store");
        assert!(load_offsets(Some(&path)).is_empty(), "non-json file");

        std::fs::write(&path, r#"{"AA:BB:CC:DD:EE:FF": "#).expect("write the truncated store");
        assert!(load_offsets(Some(&path)).is_empty(), "truncated file");

        std::fs::write(&path, "{}").expect("write the empty object store");
        assert!(load_offsets(Some(&path)).is_empty(), "empty object");

        let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
    }

    // Criterion: `offsets_store_path()` honours `XDG_STATE_HOME`, falls back to
    // `~/.local/state`, is app-scoped (`blue2th/offsets.json`), and yields None
    // when neither variable is set. Env vars are process-wide, so the three cases
    // share one test rather than racing each other.
    #[test]
    fn test_offsets_store_path_is_app_scoped_and_honours_xdg_state_home() {
        let previous_xdg = std::env::var("XDG_STATE_HOME").ok();
        let previous_home = std::env::var("HOME").ok();

        // Resolve all three cases first and assert only once the process env is
        // restored: a failing assertion here must not leak a bogus HOME into the
        // rest of the test binary and cascade into unrelated failures.
        std::env::set_var("XDG_STATE_HOME", "/tmp/blue2th-xdg-state");
        let from_xdg = offsets_store_path();

        std::env::remove_var("XDG_STATE_HOME");
        std::env::set_var("HOME", "/tmp/blue2th-home");
        let from_home = offsets_store_path();

        // A blank XDG_STATE_HOME is treated as unset, not as the root directory.
        std::env::set_var("XDG_STATE_HOME", "   ");
        let from_blank_xdg = offsets_store_path();

        std::env::remove_var("XDG_STATE_HOME");
        std::env::remove_var("HOME");
        let from_neither = offsets_store_path();

        match previous_xdg {
            Some(v) => std::env::set_var("XDG_STATE_HOME", v),
            None => std::env::remove_var("XDG_STATE_HOME"),
        }
        match previous_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }

        assert_eq!(
            from_xdg,
            Some(std::path::PathBuf::from(
                "/tmp/blue2th-xdg-state/blue2th/offsets.json"
            )),
            "XDG_STATE_HOME must win and be app-scoped"
        );
        assert_eq!(
            from_home,
            Some(std::path::PathBuf::from(
                "/tmp/blue2th-home/.local/state/blue2th/offsets.json"
            )),
            "HOME must fall back to ~/.local/state"
        );
        assert_eq!(
            from_blank_xdg, from_home,
            "a blank XDG_STATE_HOME must fall back to HOME, not to the root"
        );
        assert_eq!(
            from_neither, None,
            "with neither variable set the backend stays in-memory only"
        );
    }

    // Criterion: `SpeakerTargets::new()` performs no I/O and has no store, so a
    // test run can never read or clobber the real user's file.
    #[test]
    fn test_new_has_no_store_and_no_remembered_offsets() {
        let targets = SpeakerTargets::new();
        assert!(targets.store.is_none(), "new() must stay off-disk");
        assert!(targets.remembered.is_empty());
    }

    // Criterion: `with_store(None)` holds no store and reads nothing — the
    // store-free constructor used by the test router.
    #[test]
    fn test_with_store_none_has_no_store() {
        let targets = SpeakerTargets::with_store(None);
        assert!(targets.store.is_none());
        assert!(targets.remembered.is_empty());
        assert!(targets.speakers().is_empty());
    }

    // Criterion: `SpeakerTargets::with_store(path)` loads the remembered offsets
    // on construction.
    #[test]
    fn test_with_store_loads_remembered_offsets() {
        let path = store_path("load-on-construction");
        save_offsets(&path, &table(&[(A, 300)])).expect("seed the store");

        let targets = SpeakerTargets::with_store(Some(path.clone()));
        assert_eq!(targets.remembered.get(A), Some(&300));

        let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
    }

    // Criterion: selecting an address with a remembered offset restores it in the
    // resulting `SpeakerTarget`, instead of the default 0.
    #[test]
    fn test_select_restores_remembered_offset_from_the_store() {
        let path = store_path("restore-on-select");
        save_offsets(&path, &table(&[(A, 750)])).expect("seed the store");

        let mut targets = SpeakerTargets::with_store(Some(path.clone()));
        targets.select(A, &connected(&[A])).expect("select A");
        assert_eq!(targets.speakers()[0].offset_ms, 750);

        let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
    }

    // Criterion: `set_offset` records the clamped value for that address and
    // persists it, so a restart finds it on disk.
    #[test]
    fn test_set_offset_persists_the_clamped_value() {
        let path = store_path("persist-on-set");

        let mut targets = SpeakerTargets::with_store(Some(path.clone()));
        targets.select(A, &connected(&[A])).expect("select A");
        targets.set_offset(A, 250);
        assert_eq!(load_offsets(Some(&path)).get(A), Some(&250));

        // Out of range on the wire: the persisted value is the clamped one.
        targets.set_offset(A, 5000);
        assert_eq!(load_offsets(Some(&path)).get(A), Some(&MAX_OFFSET_MS));

        let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
    }

    // Criterion: deselecting keeps the remembered offset — deselect then
    // re-select restores it (remembered per speaker, indefinitely).
    #[test]
    fn test_deselect_then_reselect_restores_the_remembered_offset() {
        let path = store_path("deselect-reselect");

        let mut targets = SpeakerTargets::with_store(Some(path.clone()));
        targets.select(A, &connected(&[A])).expect("select A");
        targets.set_offset(A, 420);
        targets.deselect(A);
        assert!(targets.speakers().is_empty());

        targets.select(A, &connected(&[A])).expect("re-select A");
        assert_eq!(targets.speakers()[0].offset_ms, 420);

        let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
    }

    // Criterion: a remembered offset that survives a restart is restored — a new
    // `with_store` over the same path re-selects the speaker at its tuned value.
    #[test]
    fn test_offset_survives_a_restart_of_the_selection() {
        let path = store_path("restart");

        let mut first = SpeakerTargets::with_store(Some(path.clone()));
        first.select(A, &connected(&[A])).expect("select A");
        first.set_offset(A, 600);
        drop(first);

        // A fresh backend: nothing selected yet, but the tuning is remembered.
        let mut restarted = SpeakerTargets::with_store(Some(path.clone()));
        assert!(
            restarted.speakers().is_empty(),
            "the selection itself is never restored"
        );
        restarted
            .select(A, &connected(&[A]))
            .expect("select A again");
        assert_eq!(restarted.speakers()[0].offset_ms, 600);

        let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
    }

    // Criterion: selecting a speaker with no remembered offset starts at 0.
    #[test]
    fn test_select_without_remembered_offset_starts_at_zero() {
        let path = store_path("no-entry");
        save_offsets(&path, &table(&[(B, 500)])).expect("seed the store");

        let mut targets = SpeakerTargets::with_store(Some(path.clone()));
        targets.select(A, &connected(&[A])).expect("select A");
        assert_eq!(targets.speakers()[0].offset_ms, 0);

        let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
    }

    // Criterion: a remembered offset out of range (hand-edited to 5000) is
    // clamped to `0..=750` on restore, never trusted as-is.
    #[test]
    fn test_remembered_offset_out_of_range_is_clamped_on_restore() {
        let path = store_path("out-of-range");
        std::fs::write(&path, format!(r#"{{"{A}": 5000}}"#)).expect("hand-edit the store");

        let mut targets = SpeakerTargets::with_store(Some(path.clone()));
        targets.select(A, &connected(&[A])).expect("select A");
        assert_eq!(targets.speakers()[0].offset_ms, MAX_OFFSET_MS);

        let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
    }

    // Criterion: remembered offsets for unselected speakers never appear in
    // `TargetsState` — the API keeps reporting only the current selection.
    #[test]
    fn test_remembered_offsets_of_unselected_speakers_stay_out_of_state() {
        let path = store_path("unselected");
        save_offsets(&path, &table(&[(A, 100), (B, 200), (C, 300)])).expect("seed the store");

        let mut targets = SpeakerTargets::with_store(Some(path.clone()));
        targets.select(A, &connected(&[A, B, C])).expect("select A");

        let state = targets.state();
        assert_eq!(state.routing, RoutingMode::Single);
        let addrs: Vec<String> = state.speakers.into_iter().map(|s| s.address).collect();
        assert_eq!(addrs, vec![A.to_string()]);
        assert_eq!(targets.speakers().len(), 1);

        let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
    }

    // Edge case (first run): the app-scoped state directory does not exist yet —
    // `save_offsets` creates it instead of failing, so the very first slider drag
    // is already remembered.
    #[test]
    fn test_save_offsets_creates_the_missing_store_directory() {
        let dir = std::env::temp_dir().join("blue2th-test-offsets-missing-dir");
        let _ = std::fs::remove_dir_all(&dir);
        // Nested, as `$XDG_STATE_HOME/blue2th/` is under a state home that may
        // itself be missing on a fresh machine.
        let path = dir.join("state").join("blue2th").join(OFFSETS_STORE_FILE);

        save_offsets(&path, &table(&[(A, 120)])).expect("save into a missing directory");
        assert_eq!(load_offsets(Some(&path)), table(&[(A, 120)]));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // Edge case: persisting one speaker's offset must not drop what is remembered
    // for the others — the whole table is rewritten, seeded entries included.
    #[test]
    fn test_set_offset_keeps_the_remembered_offsets_of_other_speakers() {
        let path = store_path("other-speakers");
        save_offsets(&path, &table(&[(B, 500), (C, 100)])).expect("seed the store");

        let mut targets = SpeakerTargets::with_store(Some(path.clone()));
        targets.select(A, &connected(&[A])).expect("select A");
        targets.set_offset(A, 300);

        assert_eq!(
            load_offsets(Some(&path)),
            table(&[(A, 300), (B, 500), (C, 100)]),
            "an unrelated speaker's tuning must survive another one's update"
        );

        let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
    }

    // Criterion (non-nominal): a store that cannot be written (its parent is a
    // regular file here) is logged and swallowed — the offset still applies to
    // the running session and `set_offset` must not panic.
    #[test]
    fn test_set_offset_with_unwritable_store_still_applies_to_the_session() {
        let blocker = std::env::temp_dir().join("blue2th-test-offsets-unwritable");
        let _ = std::fs::remove_dir_all(&blocker);
        std::fs::write(&blocker, "not a directory").expect("write the blocking file");
        let path = blocker.join("offsets.json");

        // Construction over an unusable path yields an empty table, not a panic.
        let mut targets = SpeakerTargets::with_store(Some(path));
        targets.select(A, &connected(&[A])).expect("select A");
        targets.set_offset(A, 330);
        assert_eq!(targets.speakers()[0].offset_ms, 330);

        let _ = std::fs::remove_file(&blocker);
    }

    // ---- phase 6.3: the intent, kept across a speaker going flat ----

    /// The intent as a plain `Vec<&str>`-comparable list, for readable asserts.
    fn intent(targets: &SpeakerTargets) -> Vec<String> {
        // Clone: the assertions own their snapshot while the selection stays put.
        targets.intended.clone()
    }

    /// The selected addresses, in selection order.
    fn selected(targets: &SpeakerTargets) -> Vec<String> {
        targets.speakers().into_iter().map(|s| s.address).collect()
    }

    // Criterion: `select` records the intent.
    #[test]
    fn test_select_records_the_intent() {
        let mut targets = SpeakerTargets::new();
        targets.select(A, &connected(&[A, B])).expect("select A");
        targets.select(B, &connected(&[A, B])).expect("select B");
        assert_eq!(
            intent(&targets),
            vec![A.to_string(), B.to_string()],
            "the intent must follow the selection order"
        );
    }

    // Criterion: `select` is idempotent for the intent too — re-selecting an
    // already-selected speaker must not duplicate its address.
    #[test]
    fn test_reselect_does_not_duplicate_the_intent() {
        let mut targets = SpeakerTargets::new();
        targets.select(A, &connected(&[A])).expect("select A");
        targets.select(A, &connected(&[A])).expect("re-select A");
        assert_eq!(intent(&targets), vec![A.to_string()]);
    }

    // Criterion: a rejected `select` records nothing — a speaker that is not
    // connected, or one over the cap, must not leave an intent behind.
    #[test]
    fn test_rejected_select_records_no_intent() {
        let mut targets = SpeakerTargets::new();
        assert_eq!(
            targets.select(C, &connected(&[A])),
            Err(SelectError::NotConnected)
        );
        targets.select(A, &connected(&[A, B, C])).expect("select A");
        targets.select(B, &connected(&[A, B, C])).expect("select B");
        assert_eq!(
            targets.select(C, &connected(&[A, B, C])),
            Err(SelectError::CapExceeded)
        );
        assert_eq!(intent(&targets), vec![A.to_string(), B.to_string()]);
    }

    // Criterion: `retain_connected` still prunes the selection but leaves the
    // intent intact — a speaker going flat must not be forgotten.
    #[test]
    fn test_retain_connected_prunes_the_selection_but_keeps_the_intent() {
        let mut targets = SpeakerTargets::new();
        targets.select(A, &connected(&[A, B])).expect("select A");
        targets.select(B, &connected(&[A, B])).expect("select B");

        // B goes flat: BlueZ drops it, the live selection loses it.
        let mode = targets.retain_connected(&connected(&[A]));
        assert_eq!(mode, RoutingMode::Single);
        assert_eq!(selected(&targets), vec![A.to_string()]);
        assert_eq!(
            intent(&targets),
            vec![A.to_string(), B.to_string()],
            "a disconnection must not destroy the intent"
        );
    }

    // Criterion: `deselect` clears the intent — the only thing that does.
    #[test]
    fn test_deselect_clears_the_intent() {
        let mut targets = SpeakerTargets::new();
        targets.select(A, &connected(&[A, B])).expect("select A");
        targets.select(B, &connected(&[A, B])).expect("select B");
        targets.deselect(B);
        assert_eq!(intent(&targets), vec![A.to_string()]);
        assert!(
            targets.restorable(&connected(&[A, B])).is_empty(),
            "an explicitly deselected speaker must stay out when it reconnects"
        );
    }

    // Criterion (non-nominal): nothing connected — nothing is restorable.
    #[test]
    fn test_restorable_with_nothing_connected_is_empty() {
        let mut targets = SpeakerTargets::new();
        targets.select(A, &connected(&[A])).expect("select A");
        targets.retain_connected(&connected(&[]));
        assert!(targets.restorable(&connected(&[])).is_empty());
    }

    // Criterion (non-nominal): nothing remembered — a connected speaker nobody
    // ever picked is never restored.
    #[test]
    fn test_restorable_with_nothing_remembered_is_empty() {
        let targets = SpeakerTargets::new();
        assert!(targets.restorable(&connected(&[A, B])).is_empty());
    }

    // Criterion: one remembered speaker comes back — it is restorable.
    #[test]
    fn test_restorable_lists_a_remembered_speaker_that_came_back() {
        let mut targets = SpeakerTargets::new();
        targets.select(A, &connected(&[A, B])).expect("select A");
        targets.select(B, &connected(&[A, B])).expect("select B");
        targets.retain_connected(&connected(&[A]));

        assert_eq!(
            targets.restorable(&connected(&[A, B])),
            vec![B.to_string()],
            "B is remembered, connected again and not selected"
        );
    }

    // Criterion (non-nominal): both speakers come back at once — both are
    // restorable, in the remembered order.
    #[test]
    fn test_restorable_lists_both_speakers_in_remembered_order() {
        let mut targets = SpeakerTargets::new();
        targets.select(B, &connected(&[A, B])).expect("select B");
        targets.select(A, &connected(&[A, B])).expect("select A");
        targets.retain_connected(&connected(&[]));

        assert_eq!(
            targets.restorable(&connected(&[A, B])),
            vec![B.to_string(), A.to_string()],
            "the remembered order (B then A) must be honoured, not the connected one"
        );
    }

    // Criterion: a remembered speaker that is already selected is not listed
    // again (no duplicate).
    #[test]
    fn test_restorable_skips_an_already_selected_speaker() {
        let mut targets = SpeakerTargets::new();
        targets.select(A, &connected(&[A])).expect("select A");
        assert!(
            targets.restorable(&connected(&[A])).is_empty(),
            "A is already selected: there is nothing to restore"
        );
    }

    // Criterion (non-nominal): the speaker never comes back — a remembered
    // address that is not connected is simply not restorable.
    #[test]
    fn test_restorable_skips_a_remembered_speaker_that_is_still_away() {
        let mut targets = SpeakerTargets::new();
        targets.select(A, &connected(&[A, B])).expect("select A");
        targets.select(B, &connected(&[A, B])).expect("select B");
        targets.retain_connected(&connected(&[A]));

        assert!(
            targets.restorable(&connected(&[A])).is_empty(),
            "B is remembered but still off: nothing to restore"
        );
    }

    // Criterion (non-nominal): restoration would exceed the two-speaker cap
    // because the user picked another speaker meanwhile — the manual selection
    // wins, restoration is skipped entirely.
    #[test]
    fn test_restorable_never_evicts_a_manual_selection_at_the_cap() {
        let mut targets = SpeakerTargets::new();
        targets.select(A, &connected(&[A, B])).expect("select A");
        targets.select(B, &connected(&[A, B])).expect("select B");
        // B goes flat, and the user picks C instead: the cap is full again.
        targets.retain_connected(&connected(&[A]));
        targets.select(C, &connected(&[A, C])).expect("select C");

        assert!(
            targets.restorable(&connected(&[A, B, C])).is_empty(),
            "the manual selection wins: restoration must never evict A or C"
        );
        assert_eq!(selected(&targets), vec![A.to_string(), C.to_string()]);
    }

    // Criterion: restoration fills the remaining slots only — with one slot free
    // and two remembered speakers back, only the first remembered one fits.
    #[test]
    fn test_restorable_fills_only_the_remaining_slots() {
        let mut targets = SpeakerTargets::new();
        targets.select(A, &connected(&[A, B])).expect("select A");
        targets.select(B, &connected(&[A, B])).expect("select B");
        // Both go flat, then the user picks C by hand while they are away.
        targets.retain_connected(&connected(&[]));
        targets.select(C, &connected(&[C])).expect("select C");

        assert_eq!(
            targets.restorable(&connected(&[A, B, C])),
            vec![A.to_string()],
            "only one slot is free, so only the first remembered speaker fits"
        );
    }

    // Edge case: the intent outlives the selection, so it can grow past the cap —
    // every speaker ever picked and never explicitly deselected stays in it.
    // Restoration must still honour the cap, filling it in remembered order.
    #[test]
    fn test_restorable_caps_an_intent_longer_than_the_selection() {
        let mut targets = SpeakerTargets::new();
        targets.select(A, &connected(&[A, B])).expect("select A");
        targets.select(B, &connected(&[A, B])).expect("select B");
        // Both go flat, the user picks C meanwhile, then that one goes too:
        // three addresses are wanted while none is selected.
        targets.retain_connected(&connected(&[]));
        targets.select(C, &connected(&[C])).expect("select C");
        targets.retain_connected(&connected(&[]));
        assert_eq!(
            intent(&targets),
            vec![A.to_string(), B.to_string(), C.to_string()]
        );

        assert!(
            targets.restore(&connected(&[A, B, C])),
            "all three are back"
        );
        assert_eq!(
            selected(&targets),
            vec![A.to_string(), B.to_string()],
            "only two may come back, and in remembered order"
        );
        assert!(
            !targets.restore(&connected(&[A, B, C])),
            "the cap is full: the leftover intent must not keep reporting a change"
        );
    }

    // Criterion (the hot-path trap, disk side): `restore` runs on every
    // `/devices` poll, so it must not rewrite the store each time. A sentinel
    // planted behind the selection's back is still there afterwards.
    #[test]
    fn test_restore_does_not_rewrite_the_store() {
        const SENTINEL: &str = r#"{"offsets":{},"intended":["sentinel"]}"#;
        let path = store_path("restore-no-write");

        let mut targets = SpeakerTargets::with_store(Some(path.clone()));
        targets.select(A, &connected(&[A, B])).expect("select A");
        targets.select(B, &connected(&[A, B])).expect("select B");
        targets.retain_connected(&connected(&[A]));

        std::fs::write(&path, SENTINEL).expect("plant the sentinel");
        assert!(targets.restore(&connected(&[A, B])), "B came back");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read the store back"),
            SENTINEL,
            "restoring must not touch the disk on the /devices hot path"
        );

        let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
    }

    // Criterion: `restore` reports that the selection actually changed, and
    // re-selects the speaker that came back.
    #[test]
    fn test_restore_reselects_a_returning_speaker_and_reports_a_change() {
        let mut targets = SpeakerTargets::new();
        targets.select(A, &connected(&[A, B])).expect("select A");
        targets.select(B, &connected(&[A, B])).expect("select B");
        targets.retain_connected(&connected(&[A]));

        assert!(
            targets.restore(&connected(&[A, B])),
            "B came back: the selection changed, so the caller must re-route"
        );
        assert_eq!(selected(&targets), vec![A.to_string(), B.to_string()]);
        assert_eq!(targets.routing_mode(), RoutingMode::Combined);
    }

    // Criterion (the hot-path trap): `sync_connected` runs on every `/devices`
    // poll — a second `restore` with the same input must report `false`, or the
    // combined sink would be torn down and rebuilt every couple of seconds.
    #[test]
    fn test_restore_is_idempotent_and_reports_no_change_the_second_time() {
        let mut targets = SpeakerTargets::new();
        targets.select(A, &connected(&[A, B])).expect("select A");
        targets.select(B, &connected(&[A, B])).expect("select B");
        targets.retain_connected(&connected(&[A]));

        assert!(
            targets.restore(&connected(&[A, B])),
            "first call restores B"
        );
        for tick in 0..5 {
            assert!(
                !targets.restore(&connected(&[A, B])),
                "poll {tick}: nothing changed, so no re-route may be triggered"
            );
        }
        assert_eq!(selected(&targets), vec![A.to_string(), B.to_string()]);
    }

    // Criterion: `restore` reports no change when there is nothing to restore —
    // nothing remembered, nothing connected, or the speaker still away.
    #[test]
    fn test_restore_without_anything_to_restore_reports_no_change() {
        let mut nothing = SpeakerTargets::new();
        assert!(!nothing.restore(&connected(&[A, B])), "nothing remembered");

        let mut away = SpeakerTargets::new();
        away.select(A, &connected(&[A, B])).expect("select A");
        away.select(B, &connected(&[A, B])).expect("select B");
        away.retain_connected(&connected(&[A]));
        assert!(!away.restore(&connected(&[A])), "B is still off");
        assert_eq!(selected(&away), vec![A.to_string()]);
    }

    // Criterion (non-nominal): both speakers come back at once — one `restore`
    // call reports a single change and selects both, so the routing is rebuilt
    // once, not twice.
    #[test]
    fn test_restore_brings_both_speakers_back_in_one_change() {
        let mut targets = SpeakerTargets::new();
        targets.select(A, &connected(&[A, B])).expect("select A");
        targets.select(B, &connected(&[A, B])).expect("select B");
        targets.retain_connected(&connected(&[]));
        assert_eq!(targets.routing_mode(), RoutingMode::Idle);

        assert!(targets.restore(&connected(&[A, B])), "both came back");
        assert_eq!(selected(&targets), vec![A.to_string(), B.to_string()]);
        assert!(
            !targets.restore(&connected(&[A, B])),
            "a single change: the second call must be a no-op"
        );
    }

    // Criterion (non-nominal): restoration never evicts a manual selection —
    // `restore` reports no change when the cap is already full.
    #[test]
    fn test_restore_at_the_cap_changes_nothing() {
        let mut targets = SpeakerTargets::new();
        targets.select(A, &connected(&[A, B])).expect("select A");
        targets.select(B, &connected(&[A, B])).expect("select B");
        targets.retain_connected(&connected(&[A]));
        targets.select(C, &connected(&[A, C])).expect("select C");

        assert!(
            !targets.restore(&connected(&[A, B, C])),
            "the cap is full: nothing may change, so nothing may be re-routed"
        );
        assert_eq!(selected(&targets), vec![A.to_string(), C.to_string()]);
    }

    // Criterion (non-nominal): an explicitly deselected speaker stays out when it
    // reconnects — `deselect` is the only thing that drops the intent.
    #[test]
    fn test_restore_ignores_an_explicitly_deselected_speaker() {
        let mut targets = SpeakerTargets::new();
        targets.select(A, &connected(&[A, B])).expect("select A");
        targets.select(B, &connected(&[A, B])).expect("select B");
        targets.deselect(B);

        assert!(!targets.restore(&connected(&[A, B])));
        assert_eq!(selected(&targets), vec![A.to_string()]);
    }

    // Criterion: a restored speaker gets its remembered offset back (the phase
    // 6.1 path), not a fresh 0.
    #[test]
    fn test_restore_gives_the_returning_speaker_its_remembered_offset_back() {
        let mut targets = SpeakerTargets::new();
        targets.select(A, &connected(&[A, B])).expect("select A");
        targets.select(B, &connected(&[A, B])).expect("select B");
        targets.set_offset(B, 320);
        targets.retain_connected(&connected(&[A]));

        assert!(targets.restore(&connected(&[A, B])));
        let restored = targets
            .speakers()
            .into_iter()
            .find(|s| s.address == B)
            .expect("B is back in the selection");
        assert_eq!(restored.offset_ms, 320);
    }

    // Criterion (phase 6.3): losing the last selected device quietens the stream,
    // because nothing else will — the routing still points at its sink, and
    // PipeWire re-attaches that sink when the device comes back.
    #[test]
    fn test_should_quieten_when_the_last_device_leaves_and_nothing_restores_it() {
        assert!(should_quieten_on_last_loss(true, false));
    }

    // Criterion (phase 6.3): with restoration on, the device is re-selected on its
    // own when it returns, so pausing would leave it silent until the user pressed
    // play — the opposite of what that setting promises.
    #[test]
    fn test_should_not_quieten_when_the_setting_restores_the_device() {
        assert!(!should_quieten_on_last_loss(true, true));
    }

    // Criterion (phase 6.3): a poll that did not empty the selection quietens
    // nothing, whatever the setting says.
    #[test]
    fn test_should_not_quieten_while_a_target_remains() {
        assert!(!should_quieten_on_last_loss(false, false));
        assert!(!should_quieten_on_last_loss(false, true));
    }

    // Criterion: while playback runs, restoration only happens when the flag is
    // on; with playback stopped it always happens.
    #[test]
    fn test_should_restore_follows_the_setting_only_while_playing() {
        assert!(
            should_restore(false, false),
            "playback stopped: restoring cuts nothing, so it is always allowed"
        );
        assert!(should_restore(false, true), "playback stopped, setting on");
        assert!(
            should_restore(true, true),
            "playing with the setting on: the user opted into the brief cut"
        );
        assert!(
            !should_restore(true, false),
            "playing with the setting off: no audio may be cut behind the user's back"
        );
    }

    // Criterion: the intent is persisted alongside the offsets and reloaded on
    // startup — the server restarts and a speaker that reconnects afterwards is
    // selected without the app doing anything.
    #[test]
    fn test_intent_survives_a_restart_and_restores_on_reconnection() {
        let path = store_path("intent-restart");
        {
            let mut first = SpeakerTargets::with_store(Some(path.clone()));
            first.select(A, &connected(&[A, B])).expect("select A");
            first.select(B, &connected(&[A, B])).expect("select B");
            first.set_offset(B, 250);
        }

        let mut restarted = SpeakerTargets::with_store(Some(path.clone()));
        assert!(
            restarted.speakers().is_empty(),
            "the live selection still starts empty; only the intent is reloaded"
        );
        assert!(
            restarted.restore(&connected(&[A, B])),
            "both speakers reconnect after the restart: the intent must be on disk"
        );
        assert_eq!(selected(&restarted), vec![A.to_string(), B.to_string()]);
        let b = restarted
            .speakers()
            .into_iter()
            .find(|s| s.address == B)
            .expect("B is back");
        assert_eq!(b.offset_ms, 250, "the 6.1 offset must come back with it");

        let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
    }

    // Criterion: `deselect` clears the intent *on disk* too — a restart must not
    // resurrect a speaker the user explicitly dropped.
    #[test]
    fn test_deselect_clears_the_persisted_intent() {
        let path = store_path("intent-deselect");
        {
            let mut first = SpeakerTargets::with_store(Some(path.clone()));
            first.select(A, &connected(&[A, B])).expect("select A");
            first.select(B, &connected(&[A, B])).expect("select B");
            first.deselect(B);
        }

        let mut restarted = SpeakerTargets::with_store(Some(path.clone()));
        assert!(restarted.restore(&connected(&[A, B])), "A is remembered");
        assert_eq!(
            selected(&restarted),
            vec![A.to_string()],
            "B was deselected: it must not come back after a restart"
        );

        let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
    }

    // Criterion (non-nominal): a phase 6.1-era store on disk (offsets only, no
    // intent) must load without error and simply carry no intent.
    #[test]
    fn test_a_phase_6_1_store_loads_with_no_intent() {
        let path = store_path("legacy-6-1");
        std::fs::write(&path, format!(r#"{{"{A}": 300, "{B}": 120}}"#))
            .expect("write a phase 6.1-era store");

        let mut targets = SpeakerTargets::with_store(Some(path.clone()));
        assert!(
            targets.restorable(&connected(&[A, B])).is_empty(),
            "an offsets-only store carries no intent"
        );
        assert!(!targets.restore(&connected(&[A, B])));
        // The offsets themselves must still be honoured.
        targets.select(A, &connected(&[A])).expect("select A");
        assert_eq!(targets.speakers()[0].offset_ms, 300);

        let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
    }

    // Criterion (non-nominal): a remembered speaker that was unpaired or
    // forgotten never appears as connected, so the stale entry is harmless.
    #[test]
    fn test_a_forgotten_remembered_speaker_is_harmless() {
        let path = store_path("forgotten");
        {
            let mut first = SpeakerTargets::with_store(Some(path.clone()));
            first.select(A, &connected(&[A, B])).expect("select A");
            first.select(B, &connected(&[A, B])).expect("select B");
        }

        // B was unpaired since: it will never show up as connected again.
        let mut restarted = SpeakerTargets::with_store(Some(path.clone()));
        assert!(restarted.restore(&connected(&[A])));
        assert_eq!(selected(&restarted), vec![A.to_string()]);
        assert!(
            !restarted.restore(&connected(&[A])),
            "the stale entry must not keep reporting a change on every poll"
        );

        let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
    }

    // Criterion (non-nominal): a store-free selection keeps the intent in memory
    // only — no test may read or write the real state directory.
    #[test]
    fn test_store_free_targets_keep_the_intent_in_memory_only() {
        let mut targets = SpeakerTargets::new();
        targets.select(A, &connected(&[A])).expect("select A");
        targets.retain_connected(&connected(&[]));
        assert!(targets.restore(&connected(&[A])), "A is back");
        assert!(targets.store.is_none(), "new() must never gain a store");
    }

    // Criterion (non-nominal): a store-free selection (`new()`) behaves exactly as
    // before — offsets are remembered in memory but nothing is written anywhere.
    #[test]
    fn test_store_free_targets_remember_in_memory_only() {
        let mut targets = SpeakerTargets::new();
        targets.select(A, &connected(&[A])).expect("select A");
        targets.set_offset(A, 200);
        targets.deselect(A);
        targets.select(A, &connected(&[A])).expect("re-select A");
        assert_eq!(targets.speakers()[0].offset_ms, 200);
        assert!(targets.store.is_none(), "new() must never gain a store");
    }
}
