//! Pure playback-target selection model for phase 4 (fan-out to two speakers).
//!
//! `SpeakerTargets` tracks which connected speakers the user picked as playback
//! targets (capped at two), each speaker's latency offset, and derives the
//! [`RoutingMode`] from the selection count. It performs **no I/O**: validation
//! against the live connection state takes a slice of connected addresses passed
//! by the route layer, and the actual PipeWire routing lives in `audio.rs`.

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
}

impl SpeakerTargets {
    /// A fresh, empty selection (routing mode `Idle`).
    pub fn new() -> Self {
        Self::default()
    }

    /// Select `addr` as a playback target. Rejects an address that is not in
    /// `connected`, rejects a third selection (cap 2), and is idempotent for an
    /// already-selected address. A freshly selected speaker starts at offset `0`.
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
        self.speakers.push(SpeakerTarget {
            address: addr.to_string(),
            offset_ms: 0,
        });
        Ok(())
    }

    /// Remove `addr` from the selection (no-op if it was not selected).
    pub fn deselect(&mut self, addr: &str) {
        self.speakers.retain(|s| s.address != addr);
    }

    /// Set the per-speaker offset (clamped to `0..=MAX_OFFSET_MS`). No-op if the
    /// address is not currently selected.
    pub fn set_offset(&mut self, addr: &str, ms: u32) {
        if let Some(target) = self.speakers.iter_mut().find(|s| s.address == addr) {
            target.offset_ms = clamp_offset(ms);
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
}
