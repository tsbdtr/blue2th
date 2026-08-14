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

/// File holding the remembered offsets, under the app's state directory.
const OFFSETS_STORE_FILE: &str = "offsets.json";

/// Path of the file remembering each speaker's tuned offset:
/// `$XDG_STATE_HOME/blue2th/offsets.json` (or `~/.local/state/blue2th/offsets.json`).
/// `None` when neither variable is set, in which case offsets stay in memory only.
pub fn offsets_store_path() -> Option<std::path::PathBuf> {
    let base = std::env::var("XDG_STATE_HOME")
        .ok()
        .filter(|p| !p.trim().is_empty())
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .filter(|h| !h.trim().is_empty())
                .map(|h| format!("{h}/.local/state"))
        })?;
    Some(
        std::path::PathBuf::from(base)
            .join("blue2th")
            .join(OFFSETS_STORE_FILE),
    )
}

/// Read the remembered `address → offset_ms` table. A missing, unreadable or
/// malformed file simply means "nothing remembered yet" — never an error.
///
/// Values are clamped on the way in: a hand-edited file must not bypass the bound.
fn load_offsets(path: Option<&std::path::Path>) -> HashMap<String, u32> {
    let Some(path) = path else {
        return HashMap::new();
    };
    let Ok(raw) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    let parsed: HashMap<String, u32> = serde_json::from_str(&raw).unwrap_or_default();
    parsed
        .into_iter()
        .map(|(addr, ms)| (addr, clamp_offset(ms)))
        .collect()
}

/// Persist the remembered `address → offset_ms` table. Failures are reported to
/// the caller, which logs them: losing persistence must never break a slider drag.
fn save_offsets(path: &std::path::Path, offsets: &HashMap<String, u32>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string(offsets).map_err(std::io::Error::other)?;
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

    /// A selection backed by a remembered-offsets store, loaded on construction.
    /// Only the offsets are restored: the selection itself always starts empty.
    pub fn with_store(store: Option<std::path::PathBuf>) -> Self {
        Self {
            speakers: Vec::new(),
            remembered: load_offsets(store.as_deref()),
            store,
        }
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
        Ok(())
    }

    /// Remove `addr` from the selection (no-op if it was not selected).
    pub fn deselect(&mut self, addr: &str) {
        self.speakers.retain(|s| s.address != addr);
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
        if let Err(e) = save_offsets(path, &self.remembered) {
            tracing::warn!("could not persist the remembered speaker offsets: {e}");
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

        std::env::set_var("XDG_STATE_HOME", "/tmp/blue2th-xdg-state");
        assert_eq!(
            offsets_store_path(),
            Some(std::path::PathBuf::from(
                "/tmp/blue2th-xdg-state/blue2th/offsets.json"
            )),
            "XDG_STATE_HOME must win and be app-scoped"
        );

        std::env::remove_var("XDG_STATE_HOME");
        std::env::set_var("HOME", "/tmp/blue2th-home");
        assert_eq!(
            offsets_store_path(),
            Some(std::path::PathBuf::from(
                "/tmp/blue2th-home/.local/state/blue2th/offsets.json"
            )),
            "HOME must fall back to ~/.local/state"
        );

        std::env::remove_var("HOME");
        assert_eq!(
            offsets_store_path(),
            None,
            "with neither variable set the backend stays in-memory only"
        );

        match previous_xdg {
            Some(v) => std::env::set_var("XDG_STATE_HOME", v),
            None => std::env::remove_var("XDG_STATE_HOME"),
        }
        match previous_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
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
