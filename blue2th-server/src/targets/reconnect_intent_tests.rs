// SPDX-License-Identifier: MIT OR Apache-2.0

use super::*;

const A: &str = "AA:BB:CC:DD:EE:FF";
const B: &str = "11:22:33:44:55:66";

fn connected(addrs: &[&str]) -> Vec<String> {
    addrs.iter().map(|s| s.to_string()).collect()
}

/// A private, per-test store path under the system temp dir. Never the real
/// user state directory.
fn store_path(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("blue2th-intent-test-{name}"));
    std::fs::create_dir_all(&dir).expect("create the test store dir");
    dir.join("offsets.json")
}

// Criterion (phase 6.5): `intended()` reports the persisted intent, in the
// order it was picked — that is the candidate list the pass reads.
#[test]
fn test_intended_reports_the_selection_order() {
    let mut targets = SpeakerTargets::new();
    targets.select(B, &connected(&[A, B])).expect("select B");
    targets.select(A, &connected(&[A, B])).expect("select A");
    assert_eq!(targets.intended(), vec![B.to_string(), A.to_string()]);
}

// Criterion (non-nominal: nothing remembered): a fresh selection has an empty
// intent, so the pass finds no candidate and does no I/O at all.
#[test]
fn test_intended_is_empty_for_a_fresh_selection() {
    assert!(SpeakerTargets::new().intended().is_empty());
    assert!(SpeakerTargets::with_store(None).intended().is_empty());
}

// Criterion (phase 6.5): losing the radio keeps the intent — a speaker that
// went flat is still a reconnect candidate.
#[test]
fn test_intended_survives_losing_the_connection() {
    let mut targets = SpeakerTargets::new();
    targets.select(A, &connected(&[A])).expect("select A");
    targets.retain_connected(&[]);
    assert!(targets.speakers().is_empty(), "the selection is pruned");
    assert_eq!(
        targets.intended(),
        vec![A.to_string()],
        "the intent is what auto-reconnect dials back"
    );
}

// Criterion (non-nominal: the user deselects a speaker): `deselect` clears the
// intent, so the address stops being a candidate at all.
#[test]
fn test_deselect_drops_the_address_from_the_intent() {
    let mut targets = SpeakerTargets::new();
    targets.select(A, &connected(&[A, B])).expect("select A");
    targets.select(B, &connected(&[A, B])).expect("select B");
    targets.deselect(A);
    assert_eq!(targets.intended(), vec![B.to_string()]);
}

// Criterion (phase 6.5, nominal): the intent is read back from
// `offsets.json` after a restart — the whole point is that a rebooted PC
// knows which speaker to dial.
#[test]
fn test_intended_is_reloaded_from_the_store_after_a_restart() {
    let path = store_path("reload");
    {
        let mut targets = SpeakerTargets::with_store(Some(path.clone()));
        targets.select(A, &connected(&[A, B])).expect("select A");
        targets.select(B, &connected(&[A, B])).expect("select B");
    }

    let reloaded = SpeakerTargets::with_store(Some(path.clone()));
    assert!(
        reloaded.speakers().is_empty(),
        "nothing is connected at startup"
    );
    assert_eq!(
        reloaded.intended(),
        vec![A.to_string(), B.to_string()],
        "the persisted intent is the reconnect candidate list"
    );

    let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
}
