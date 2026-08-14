//! Integration tests for phase 6.3 — restoring the playback selection when a
//! speaker comes back.
//!
//! What is covered here is the **decision**: whether restoring a returning
//! speaker moves `librespot`'s target sink, which is the only reason to respawn
//! it (`--device` is fixed at spawn). The respawn itself, the PipeWire re-route
//! and BlueZ's own reconnection are hardware seams, validated by hand.

use blue2th_server::spotify::spotify_target_sink;
use blue2th_server::targets::SpeakerTargets;

const A: &str = "AA:BB:CC:DD:EE:FF";
const B: &str = "11:22:33:44:55:66";

/// The connected-address list the route layer hands to the selection.
fn connected(addrs: &[&str]) -> Vec<String> {
    addrs.iter().map(|s| s.to_string()).collect()
}

// Criterion: `librespot` is respawned when the restoration moves the target sink
// — a lone speaker feeds its `bluez_output.*` directly, two go through the
// combined sink, so a speaker coming back changes where the stream must go.
#[test]
fn test_restoring_a_second_speaker_moves_the_librespot_target_sink() {
    // Store-free: no test may read or write the real ~/.local/state/blue2th/.
    let mut targets = SpeakerTargets::new();
    targets.select(A, &connected(&[A, B])).expect("select A");
    targets.select(B, &connected(&[A, B])).expect("select B");
    // B goes flat: the live selection falls back to the single-sink route.
    targets.retain_connected(&connected(&[A]));
    let before = spotify_target_sink(&targets.speakers());

    assert!(targets.restore(&connected(&[A, B])), "B came back");
    let after = spotify_target_sink(&targets.speakers());

    assert_ne!(
        before, after,
        "the target sink moved, so librespot must be respawned"
    );
}

// Criterion: ...and **only** then — a restoration that leaves the target sink
// where it was must not respawn `librespot`. A lone speaker with a non-zero
// offset already runs through the combined sink, so the second one joining it
// changes nothing for `--device`.
#[test]
fn test_restoring_into_an_existing_combined_sink_keeps_the_target_sink() {
    let mut targets = SpeakerTargets::new();
    targets.select(A, &connected(&[A, B])).expect("select A");
    targets.select(B, &connected(&[A, B])).expect("select B");
    targets.set_offset(A, 300);
    targets.retain_connected(&connected(&[A]));
    let before = spotify_target_sink(&targets.speakers());

    assert!(targets.restore(&connected(&[A, B])), "B came back");
    let after = spotify_target_sink(&targets.speakers());

    assert_eq!(
        before, after,
        "the sink did not move: librespot must be left alone"
    );
}

// Criterion (the hot-path trap): a `/devices` poll that restores nothing leaves
// both the selection and the target sink exactly as they were, so nothing is
// torn down and rebuilt every couple of seconds.
#[test]
fn test_a_poll_that_restores_nothing_leaves_the_target_sink_alone() {
    let mut targets = SpeakerTargets::new();
    targets.select(A, &connected(&[A, B])).expect("select A");
    targets.select(B, &connected(&[A, B])).expect("select B");
    targets.retain_connected(&connected(&[A]));
    assert!(
        targets.restore(&connected(&[A, B])),
        "first poll restores B"
    );
    let settled = spotify_target_sink(&targets.speakers());

    for tick in 0..5 {
        assert!(
            !targets.restore(&connected(&[A, B])),
            "poll {tick} must report no change"
        );
        assert_eq!(
            spotify_target_sink(&targets.speakers()),
            settled,
            "poll {tick} must leave the target sink where it is"
        );
    }
}
