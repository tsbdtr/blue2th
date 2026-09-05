// SPDX-License-Identifier: MIT OR Apache-2.0

//! Integration tests for phase 6.3 — restoring the playback selection when a
//! speaker comes back.
//!
//! What is covered here is the **decision**: whether restoring a returning
//! speaker moves `librespot`'s target sink, which is the only reason to respawn
//! it (`--device` is fixed at spawn). Since every non-empty selection goes
//! through the combined sink, it never does. The PipeWire re-route and BlueZ's
//! own reconnection are hardware seams, validated by hand.

use blue2th_server::spotify::spotify_target_sink;
use blue2th_server::targets::SpeakerTargets;

const A: &str = "AA:BB:CC:DD:EE:FF";
const B: &str = "11:22:33:44:55:66";

/// The connected-address list the route layer hands to the selection.
fn connected(addrs: &[&str]) -> Vec<String> {
    addrs.iter().map(|s| s.to_string()).collect()
}

// Criterion: a restoration never moves the target sink, so it never respawns
// `librespot` — even from a lone speaker at offset 0, the selection that used to
// take the direct route and whose crossing back is what #70 reports.
#[test]
fn test_restoring_a_second_speaker_keeps_the_librespot_target_sink() {
    // Store-free: no test may read or write the real ~/.local/state/blue2th/.
    let mut targets = SpeakerTargets::new();
    targets.select(A, &connected(&[A, B])).expect("select A");
    targets.select(B, &connected(&[A, B])).expect("select B");
    // B goes flat: the live selection drops to a lone speaker at offset 0.
    targets.retain_connected(&connected(&[A]));
    let before = spotify_target_sink(&targets.speakers());

    assert!(targets.restore(&connected(&[A, B])), "B came back");
    let after = spotify_target_sink(&targets.speakers());

    assert_eq!(
        before, after,
        "the target sink did not move: librespot must be left alone"
    );
}

// Criterion: the same holds when the lone speaker carries an offset — that
// selection already ran through the combined sink, so the second one joining it
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
