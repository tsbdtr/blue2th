// SPDX-License-Identifier: MIT OR Apache-2.0

use super::*;
use crate::targets::MAX_OFFSET_MS;
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

// Criterion: `POST /volume` clamps to `0.0..=1.0` — value below 0 saturates
// to 0.0.
#[test]
fn test_clamp_volume_below_range_saturates_to_zero() {
    assert_eq!(clamp_volume(-0.5), 0.0);
    assert_eq!(clamp_volume(-1000.0), 0.0);
}

// Criterion: `POST /volume` clamps to `0.0..=1.0` — value above 1 saturates
// to 1.0.
#[test]
fn test_clamp_volume_above_range_saturates_to_one() {
    assert_eq!(clamp_volume(1.5), 1.0);
    assert_eq!(clamp_volume(1000.0), 1.0);
}

// Criterion: `POST /volume` clamps to `0.0..=1.0` — a NaN level (which
// `f32::clamp` would otherwise propagate, serializing as JSON `null`) is
// treated as silence rather than stored.
#[test]
fn test_clamp_volume_nan_saturates_to_zero() {
    assert_eq!(clamp_volume(f32::NAN), 0.0);
}

// Criterion: `POST /volume` clamps to `0.0..=1.0` — in-range values are kept
// unchanged, including the boundaries.
#[test]
fn test_clamp_volume_in_range_is_unchanged() {
    assert_eq!(clamp_volume(0.0), 0.0);
    assert_eq!(clamp_volume(0.5), 0.5);
    assert_eq!(clamp_volume(1.0), 1.0);
}

// Criterion: `POST /play` starts playback and returns `status: playing` —
// a fresh engine begins stopped.
#[test]
fn test_new_engine_starts_stopped() {
    let engine = AudioEngine::new();
    assert_eq!(engine.playback_state().status, PlaybackStatus::Stopped);
}

// Criterion: pause/resume/stop transitions —
// Stopped -> Playing -> Paused -> Playing -> Stopped.
#[test]
fn test_playback_state_machine_full_cycle() {
    let mut engine = AudioEngine::new();

    let state = engine.play().expect("play succeeds");
    assert_eq!(state.status, PlaybackStatus::Playing);

    let state = engine.pause().expect("pause succeeds");
    assert_eq!(state.status, PlaybackStatus::Paused);

    let state = engine.play().expect("resume succeeds");
    assert_eq!(state.status, PlaybackStatus::Playing);

    let state = engine.stop().expect("stop succeeds");
    assert_eq!(state.status, PlaybackStatus::Stopped);
}

/// An output recording what the engine asked of it, in order, and
/// optionally refusing to start — as the tone stream does with no daemon.
struct RecordingOutput {
    log: Arc<std::sync::Mutex<Vec<&'static str>>>,
    refuse_start: bool,
    /// Raised by the test to play a stream that ended on its own.
    finished: Arc<AtomicBool>,
}

impl AudioOutput for RecordingOutput {
    fn start(&mut self) -> Result<(), AudioError> {
        self.log.lock().unwrap().push("start");
        if self.refuse_start {
            return Err(AudioError::PipeWire("no PipeWire daemon".to_string()));
        }
        Ok(())
    }
    fn resume(&mut self) -> Result<(), AudioError> {
        self.log.lock().unwrap().push("resume");
        Ok(())
    }
    fn pause(&mut self) -> Result<(), AudioError> {
        self.log.lock().unwrap().push("pause");
        Ok(())
    }
    fn stop(&mut self) -> Result<(), AudioError> {
        self.log.lock().unwrap().push("stop");
        Ok(())
    }
    fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Relaxed)
    }
}

// Criterion: `AudioOutput::start` takes no tone, and the state machine is
// otherwise unchanged — each transition is one call of its own on the
// output: a pause and a play resume (keeping the tone's position) rather
// than stop and start it again, and a play after a stop starts afresh.
#[test]
fn test_engine_drives_start_pause_resume_stop_on_the_output() {
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut engine = AudioEngine::with_output(Box::new(RecordingOutput {
        log: Arc::clone(&log),
        refuse_start: false,
        finished: Arc::default(),
    }));

    assert!(engine.play().is_ok());
    assert!(engine.play().is_ok(), "a play while playing is idempotent");
    assert!(engine.pause().is_ok());
    assert!(engine.play().is_ok());
    assert!(engine.stop().is_ok());
    assert!(engine.play().is_ok());

    assert_eq!(
        *log.lock().unwrap(),
        vec!["start", "pause", "resume", "stop", "start"]
    );
}

// Non-nominal: the tone's target vanished while it was paused — the last
// speaker deselected, whose handler pauses the engine and then tears the
// combined sink down, so the stream went `Unconnected`. The engine reads
// that as `Stopped`, and the next play starts the tone afresh: resuming a
// dead stream played nothing until a stop. The near miss, a pause then a
// play on a live stream, resumes it (`test_engine_drives_start_pause_…`).
#[test]
fn test_engine_play_after_the_output_ended_while_paused_starts_afresh() {
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let finished = Arc::new(AtomicBool::new(false));
    let mut engine = AudioEngine::with_output(Box::new(RecordingOutput {
        log: Arc::clone(&log),
        refuse_start: false,
        finished: Arc::clone(&finished),
    }));
    assert!(engine.play().is_ok());
    assert!(engine.pause().is_ok());

    finished.store(true, Ordering::Relaxed);

    assert_eq!(engine.poll_state().status, PlaybackStatus::Stopped);
    finished.store(false, Ordering::Relaxed);
    assert!(engine.play().is_ok());
    assert_eq!(*log.lock().unwrap(), vec!["start", "pause", "start"]);
    assert_eq!(engine.poll_state().status, PlaybackStatus::Playing);
}

// Non-nominal: with no PipeWire daemon the output refuses to start — the
// error comes back from `play` at once, and the engine stays `Stopped`.
#[test]
fn test_engine_play_with_a_refused_start_errs_and_stays_stopped() {
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut engine = AudioEngine::with_output(Box::new(RecordingOutput {
        log: Arc::clone(&log),
        refuse_start: true,
        finished: Arc::default(),
    }));

    let played = engine.play();

    assert!(
        matches!(played, Err(AudioError::PipeWire(_))),
        "got {played:?}"
    );
    assert_eq!(engine.poll_state().status, PlaybackStatus::Stopped);
}

// Criterion: pause/stop while stopped are idempotent (unchanged state).
#[test]
fn test_pause_while_stopped_is_idempotent() {
    let mut engine = AudioEngine::new();
    let state = engine.pause().expect("idempotent pause succeeds");
    assert_eq!(state.status, PlaybackStatus::Stopped);
}

// Criterion: pause/stop while stopped are idempotent (unchanged state).
#[test]
fn test_stop_while_stopped_is_idempotent() {
    let mut engine = AudioEngine::new();
    let state = engine.stop().expect("idempotent stop succeeds");
    assert_eq!(state.status, PlaybackStatus::Stopped);
}

// Criterion: all levels readable and equal -> that value is reported.
#[test]
fn test_reported_volume_agreeing_levels_reports_the_common_value() {
    assert_eq!(
        reported_volume(&[Some(0.4), Some(0.4)], 0.7),
        0.4,
        "two sinks at 40% report 40%, not the commanded level"
    );
}

// Criterion: all levels readable and equal -> that value is reported (a
// single selected speaker agrees with itself, so its live level is reported).
#[test]
fn test_reported_volume_single_readable_level_reports_that_level() {
    assert_eq!(reported_volume(&[Some(0.55)], 0.2), 0.55);
}

// Criterion: levels readable but not all equal -> the commanded level.
#[test]
fn test_reported_volume_differing_levels_reports_commanded() {
    assert_eq!(
        reported_volume(&[Some(0.3), Some(0.8)], 0.55),
        0.55,
        "the slider must not jump to one speaker's value"
    );
}

// Criterion: any level unreadable -> the commanded level, even when the
// readable ones agree.
#[test]
fn test_reported_volume_one_unreadable_level_reports_commanded() {
    assert_eq!(reported_volume(&[Some(0.4), None, Some(0.4)], 0.65), 0.65);
}

// Criterion: any level unreadable -> the commanded level.
#[test]
fn test_reported_volume_all_levels_unreadable_reports_commanded() {
    assert_eq!(reported_volume(&[None, None], 0.25), 0.25);
}

// Criterion: no selected speaker -> the commanded level (unchanged
// behaviour).
#[test]
fn test_reported_volume_empty_selection_reports_commanded() {
    assert_eq!(reported_volume(&[], 0.6), 0.6);
}

// Criterion: agreement is decided at whole-percent resolution — two levels a
// whole percent apart disagree, so the commanded level is reported.
#[test]
fn test_reported_volume_levels_one_percent_apart_reports_commanded() {
    assert_eq!(reported_volume(&[Some(0.40), Some(0.41)], 0.75), 0.75);
}

// Criterion: agreement is decided at whole-percent resolution rather than by
// float equality — two levels differing only below that resolution agree, so
// their common whole-percent level (40%) is reported, not `commanded`.
#[test]
fn test_reported_volume_sub_percent_difference_reports_the_common_value() {
    let reported = reported_volume(&[Some(0.401), Some(0.404)], 0.9);
    assert!(
        (reported - 0.40).abs() < 0.005,
        "expected the shared 40% level, got {reported}"
    );
}

// Criterion: a `NaN` level is never counted as agreeing. `parse_first_percent`
// cannot currently produce one, so this guards a future reader rather than
// pinning a live bug.
#[test]
fn test_reported_volume_nan_level_reports_commanded() {
    assert_eq!(reported_volume(&[Some(0.5), Some(f32::NAN)], 0.35), 0.35);
    assert_eq!(reported_volume(&[Some(f32::NAN)], 0.35), 0.35);
}

// Criterion: a level `PlaybackState.volume` cannot express is never counted
// as agreeing. An over-amplified sink reads as e.g. 1.53 (a 153% volume);
// reporting it would break the DTO's
// documented `0.0..=1.0` range, and clamping it to 100% would name a level
// no speaker is at.
#[test]
fn test_reported_volume_out_of_range_level_reports_commanded() {
    assert_eq!(reported_volume(&[Some(1.53)], 0.35), 0.35);
    assert_eq!(
        reported_volume(&[Some(1.53), Some(1.53)], 0.35),
        0.35,
        "two over-amplified sinks agree on a level the API cannot report"
    );
    assert_eq!(reported_volume(&[Some(0.4), Some(1.2)], 0.35), 0.35);
    assert_eq!(reported_volume(&[Some(-0.2)], 0.35), 0.35);
}

// Criterion: a level that is not a number is never counted as agreeing —
// infinities included. The percentage cast saturates, so an unfiltered
// infinity would be reported as 21474836, and two *distinct* huge levels
// would saturate to the same percentage and manufacture an agreement.
#[test]
fn test_reported_volume_infinite_level_reports_commanded() {
    assert_eq!(reported_volume(&[Some(f32::INFINITY)], 0.35), 0.35);
    assert_eq!(reported_volume(&[Some(f32::NEG_INFINITY)], 0.35), 0.35);
    assert_eq!(reported_volume(&[Some(1e30), Some(1e31)], 0.35), 0.35);
}

// Criterion: any level unreadable -> the commanded level, wherever it sits in
// the selection order — the first entry decides just as the last one does.
#[test]
fn test_reported_volume_unreadable_level_at_either_end_reports_commanded() {
    assert_eq!(reported_volume(&[None, Some(0.4)], 0.65), 0.65);
    assert_eq!(reported_volume(&[Some(0.4), None], 0.65), 0.65);
}

// Criterion: `POST /volume` sets the sink volume and returns the new state;
// out-of-range input is clamped (no error).
#[test]
fn test_set_volume_clamps_and_updates_state() {
    let mut engine = AudioEngine::new();

    let state = engine.set_volume(0.3).expect("set in-range volume");
    assert_eq!(state.volume, 0.3);

    let state = engine.set_volume(2.0).expect("set out-of-range volume");
    assert_eq!(state.volume, 1.0);

    let state = engine.set_volume(-1.0).expect("set negative volume");
    assert_eq!(state.volume, 0.0);
}

// The PipeWire sink node-name prefix is derived from a MAC by upper-casing and
// replacing colons with underscores (matches `bluetooth_sink_for`).
#[test]
fn test_bluez_sink_prefix_maps_mac_to_node_prefix() {
    assert_eq!(
        bluez_sink_prefix("aa:bb:cc:dd:ee:ff"),
        "bluez_output.AA_BB_CC_DD_EE_FF"
    );
}

// Criterion: with two targets, the combined-sink plan lists both speakers'
// `bluez_output.*` sink names and each speaker's offset as its branch's
// delay — offset 250 is a delay of 250 ms, with no base on top.
#[test]
fn test_combine_sink_plan_lists_both_sinks_and_offsets() {
    let targets = vec![
        SpeakerTarget {
            address: "AA:BB:CC:DD:EE:FF".to_string(),
            offset_ms: 0,
        },
        SpeakerTarget {
            address: "11:22:33:44:55:66".to_string(),
            offset_ms: 250,
        },
    ];

    let spec = combine_sink_plan(&targets);
    assert_eq!(spec.branches.len(), 2);

    let first = &spec.branches[0];
    assert!(
        first.sink.starts_with("bluez_output.AA_BB_CC_DD_EE_FF"),
        "first branch must target the first speaker's bluez sink, got {}",
        first.sink
    );
    assert_eq!(first.latency_ms, 0);

    let second = &spec.branches[1];
    assert!(
        second.sink.starts_with("bluez_output.11_22_33_44_55_66"),
        "second branch must target the second speaker's bluez sink, got {}",
        second.sink
    );
    assert_eq!(second.latency_ms, 250);
}

// Criterion: `combine_sink_plan` builds exactly one branch for a lone
// speaker, naming that speaker's sink prefix and its offset as the branch
// delay — including at offset 0, which is one branch at a delay of zero,
// not "no branch".
#[test]
fn test_combine_sink_plan_lone_speaker_at_zero_offset_yields_one_branch() {
    let spec = combine_sink_plan(&[SpeakerTarget {
        address: "AA:BB:CC:DD:EE:FF".to_string(),
        offset_ms: 0,
    }]);

    assert_eq!(spec.branches.len(), 1);
    let only = &spec.branches[0];
    assert_eq!(only.sink, bluez_sink_prefix("AA:BB:CC:DD:EE:FF"));
    assert_eq!(only.latency_ms, 0, "offset 0 is a delay of zero");
}

// Criterion: `combine_sink_plan` keeps a lone speaker's offset as the branch
// delay — the offset only exists as the delay of its branch, so a lone
// speaker going through the combined sink is the only way it is heard.
#[test]
fn test_combine_sink_plan_lone_speaker_carries_its_offset_as_latency() {
    let spec = combine_sink_plan(&[SpeakerTarget {
        address: "11:22:33:44:55:66".to_string(),
        offset_ms: 320,
    }]);

    assert_eq!(spec.branches.len(), 1);
    let only = &spec.branches[0];
    assert_eq!(only.sink, bluez_sink_prefix("11:22:33:44:55:66"));
    assert_eq!(only.latency_ms, 320);
}

// Criterion: the 50 ms base is gone — a branch's delay is exactly the
// speaker's offset, so offset 0 plans a delay of 0 ms, and the largest
// accepted offset plans exactly that many milliseconds.
#[test]
fn test_combine_sink_plan_offset_zero_is_delay_zero() {
    let delays = |offset_ms| {
        combine_sink_plan(&[SpeakerTarget {
            address: "AA:BB:CC:DD:EE:FF".to_string(),
            offset_ms,
        }])
        .branches
        .iter()
        .map(|b| b.latency_ms)
        .collect::<Vec<_>>()
    };

    assert_eq!(delays(0), vec![0]);
    assert_eq!(delays(MAX_OFFSET_MS), vec![MAX_OFFSET_MS]);
}

// Criterion: the offsets stay purely relative — two offsets differing by `n`
// plan delays differing by exactly `n`. It is what keeps a calibration
// made with the 50 ms base valid once the base is gone: every speaker
// plays 50 ms earlier, and the gap between two of them is unchanged.
#[test]
fn test_combine_sink_plan_keeps_the_gap_between_two_offsets() {
    for (lower, higher) in [(0_u32, 70_u32), (40, 250), (250, MAX_OFFSET_MS)] {
        let spec = combine_sink_plan(&[
            SpeakerTarget {
                address: "AA:BB:CC:DD:EE:FF".to_string(),
                offset_ms: lower,
            },
            SpeakerTarget {
                address: "11:22:33:44:55:66".to_string(),
                offset_ms: higher,
            },
        ]);
        let delays: Vec<u32> = spec.branches.iter().map(|b| b.latency_ms).collect();

        assert_eq!(delays.len(), 2);
        assert_eq!(
            delays[1] - delays[0],
            higher - lower,
            "the gap between offsets {lower} and {higher}"
        );
    }
}

// Criterion: `route_for_targets` takes the combined path for every non-empty
// selection. The routing itself needs a PipeWire daemon CI does not have, so the
// pure half is pinned instead: the sink the plan names is the sink the
// Spotify backend is pointed at, for a lone speaker as much as for two.
// If they ever disagree, playback goes somewhere the plan did not build.
#[test]
fn test_combine_sink_plan_names_the_sink_spotify_is_pointed_at() {
    let lone = vec![SpeakerTarget {
        address: "AA:BB:CC:DD:EE:FF".to_string(),
        offset_ms: 0,
    }];
    let two = vec![
        SpeakerTarget {
            address: "AA:BB:CC:DD:EE:FF".to_string(),
            offset_ms: 0,
        },
        SpeakerTarget {
            address: "11:22:33:44:55:66".to_string(),
            offset_ms: 250,
        },
    ];

    for selection in [&lone, &two] {
        assert_eq!(
            combine_sink_plan(selection).sink_name,
            crate::spotify::spotify_target_sink(selection),
            "the plan must build the very sink Spotify is pointed at, for {selection:?}"
        );
    }
}

// Criterion: `route_for_targets` still refuses an empty selection with
// `AudioError::NoSpeakerConnected` — the guard runs before the graph is asked,
// which is what makes this testable without hardware.
#[test]
fn test_route_for_targets_empty_selection_is_refused() {
    let mut router = AudioRouter::new(Box::new(crate::graph::fake::FakeGraph::new()));
    assert!(matches!(
        router.route_for_targets(&[]),
        Err(AudioError::NoSpeakerConnected)
    ));
}

/// A realistic sink table (#78): tab-separated columns, the node name
/// second, one Bluetooth speaker whose live node carries the `.1` card
/// suffix, the PC's own output and the combined null sink.
const SINK_TABLE: &str = concat!(
    "39\talsa_output.pci-0000_00_1f.3.analog-stereo\tPipeWire\ts32le 2ch 48000Hz\tSUSPENDED\n",
    "57\tbluez_output.80_99_E7_63_50_29.1\tPipeWire\ts16le 2ch 48000Hz\tRUNNING\n",
    "61\tblue2th_combined\tPipeWire\tf32le 2ch 48000Hz\tIDLE\n",
);

// Criterion: a pure function resolves a prefix against a sink table (#78),
// mapping `bluez_output.<MAC>` to the line carrying the
// card suffix (`bluez_output.<MAC>.1`). This is the heart of the defect: the
// prefix itself names no live node, so `--device <prefix>` silently falls
// back to the default sink.
#[test]
fn test_sink_matching_prefix_resolves_a_bluez_prefix_to_the_card_suffixed_node() {
    assert_eq!(
        sink_matching_prefix(SINK_TABLE, "bluez_output.80_99_E7_63_50_29"),
        Some("bluez_output.80_99_E7_63_50_29.1".to_string())
    );
}

// Criterion: the matcher returns `None` when no line matches — the speaker
// vanished between the routing call and the spawn, and the caller must fail
// rather than fall back to the default sink.
#[test]
fn test_sink_matching_prefix_without_a_matching_line_is_none() {
    assert_eq!(
        sink_matching_prefix(SINK_TABLE, "bluez_output.AA_BB_CC_DD_EE_FF"),
        None
    );
}

// Criterion: with several sinks present the matcher picks the right
// `bluez_output.*` line — the second speaker's node, not the first one and
// not the PC's own output.
#[test]
fn test_sink_matching_prefix_picks_the_right_line_among_several_sinks() {
    let listing = concat!(
        "39\talsa_output.pci-0000_00_1f.3.analog-stereo\tPipeWire\ts32le 2ch 48000Hz\tSUSPENDED\n",
        "57\tbluez_output.80_99_E7_63_50_29.1\tPipeWire\ts16le 2ch 48000Hz\tRUNNING\n",
        "58\tbluez_output.11_22_33_44_55_66.2\tPipeWire\ts16le 2ch 48000Hz\tIDLE\n",
        "61\tblue2th_combined\tPipeWire\tf32le 2ch 48000Hz\tIDLE\n",
    );
    assert_eq!(
        sink_matching_prefix(listing, "bluez_output.11_22_33_44_55_66"),
        Some("bluez_output.11_22_33_44_55_66.2".to_string())
    );
}

// Criterion: the match is anchored at the start of the node name, so a sink
// that merely *contains* the prefix is not mistaken for the speaker's own
// node. A `contains` implementation would answer the wrong sink here.
#[test]
fn test_sink_matching_prefix_ignores_a_sink_that_only_contains_the_prefix() {
    let listing = concat!(
        "44\tvirtual_bluez_output.80_99_E7_63_50_29.9\tPipeWire\ts16le 2ch 48000Hz\tIDLE\n",
        "57\tbluez_output.80_99_E7_63_50_29.1\tPipeWire\ts16le 2ch 48000Hz\tRUNNING\n",
    );
    assert_eq!(
        sink_matching_prefix(listing, "bluez_output.80_99_E7_63_50_29"),
        Some("bluez_output.80_99_E7_63_50_29.1".to_string())
    );
}

// Criterion (non-nominal, the path that always worked): `blue2th_combined`
// is already an exact node name, so resolving it is a no-op and the combined
// route keeps behaving exactly as it does today.
#[test]
fn test_sink_matching_prefix_leaves_an_exact_node_name_unchanged() {
    assert_eq!(
        sink_matching_prefix(SINK_TABLE, "blue2th_combined"),
        Some("blue2th_combined".to_string())
    );
}

// An empty prefix starts every string, so a bare `starts_with` answers the
// first line of the listing — the PC's own output. `spotify_target_sink(&[])`
// returns exactly that empty string, and only the `speakers.is_empty()` guard
// at the top of `SpotifyBackend::start` stands between it and pointing
// `--device` at the PC. Verified on a live graph (#78): before this guard,
// `resolve_target_sink("")` answered `Ok("alsa_output.…HiFi__Speaker__sink")`.
#[test]
fn test_sink_matching_prefix_without_a_prefix_matches_nothing() {
    assert_eq!(sink_matching_prefix(SINK_TABLE, ""), None);
    // Also against a listing carrying a blank node-name column, which the
    // boundary rule would otherwise accept as *equal* to the empty prefix.
    assert_eq!(sink_matching_prefix("39\t\tPipeWire\tIDLE\n", ""), None);
}

// Criterion: the matcher picks the `bluez_output.*` line rather than an
// unrelated sink that happens to share a prefix. A longer namesake that does
// not continue with the `.` card separator is not the target, whatever its
// sink index — a bare `starts_with` answers it as soon as it sorts first.
#[test]
fn test_sink_matching_prefix_ignores_a_longer_namesake_without_a_card_separator() {
    let listing = "60\tblue2th_combined_old\tPipeWire\tf32le 2ch 48000Hz\tIDLE\n";
    assert_eq!(sink_matching_prefix(listing, "blue2th_combined"), None);
}

// The exact node wins over a longer namesake wherever the two sit: a stale
// `blue2th_combined_old` carrying a lower sink index must not shadow the
// combined sink librespot is about to be pointed at.
#[test]
fn test_sink_matching_prefix_prefers_the_exact_node_over_a_longer_namesake() {
    let listing = concat!(
        "60\tblue2th_combined_old\tPipeWire\tf32le 2ch 48000Hz\tIDLE\n",
        "61\tblue2th_combined\tPipeWire\tf32le 2ch 48000Hz\tIDLE\n",
    );
    assert_eq!(
        sink_matching_prefix(listing, "blue2th_combined"),
        Some("blue2th_combined".to_string())
    );
}

// The tie-break is stated in the doc, so it is pinned here: with two
// `.`-suffixed candidates for one prefix, the first line wins — the order
// the sinks were listed in — rather than whichever the iteration happens to reach.
#[test]
fn test_sink_matching_prefix_takes_the_first_suffixed_candidate() {
    let listing = concat!(
        "57\tbluez_output.80_99_E7_63_50_29.1\tPipeWire\ts16le 2ch 48000Hz\tRUNNING\n",
        "58\tbluez_output.80_99_E7_63_50_29.2\tPipeWire\ts16le 2ch 48000Hz\tIDLE\n",
    );
    assert_eq!(
        sink_matching_prefix(listing, "bluez_output.80_99_E7_63_50_29"),
        Some("bluez_output.80_99_E7_63_50_29.1".to_string())
    );
}

// A sink table must survive whatever a tool prints (#78): a blank line, a line short of the node-name column, and a trailing
// newline all get skipped rather than panicking or answering an empty name.
#[test]
fn test_sink_matching_prefix_skips_lines_without_a_node_name_column() {
    let listing = concat!(
        "\n",
        "39\n",
        "\t\n",
        "57\tbluez_output.80_99_E7_63_50_29.1\tPipeWire\ts16le 2ch 48000Hz\tRUNNING\n",
        "\n",
    );
    assert_eq!(
        sink_matching_prefix(listing, "bluez_output.80_99_E7_63_50_29"),
        Some("bluez_output.80_99_E7_63_50_29.1".to_string())
    );
}

/// The spec the reconciliation tests compare a loaded graph against: two
/// speakers, offsets 0 and 250 ms, branch sinks held as `bluez_output.*`
/// **prefixes** the way [`combine_sink_plan`] builds them.
fn two_speaker_spec() -> CombineSinkSpec {
    combine_sink_plan(&[
        SpeakerTarget {
            address: "80:99:E7:63:50:29".to_string(),
            offset_ms: 0,
        },
        SpeakerTarget {
            address: "11:22:33:44:55:66".to_string(),
            offset_ms: 250,
        },
    ])
}

// Criterion: a speaker added to the selection loads its own branch and
// nothing else — the branch already streaming is neither unloaded nor
// retuned. The #75 rule tore it down to restart it with the newcomer.
#[test]
fn test_reconcile_branches_added_speaker_loads_only_that_branch() {
    let spec = two_speaker_spec();
    let already_streaming = CombineBranch {
        sink: "bluez_output.80_99_E7_63_50_29.1".to_string(),
        latency_ms: 0,
    };

    let plan = reconcile_branches(&[already_streaming], &spec);

    assert_eq!(
        plan,
        BranchPlan {
            to_load: vec![CombineBranch {
                sink: bluez_sink_prefix("11:22:33:44:55:66"),
                latency_ms: 250,
            }],
            to_retune: Vec::new(),
            to_unload: Vec::new(),
        }
    );
}

/// The sinks a graph reports: one Bluetooth speaker whose live node carries
/// the `.1` card suffix, the PC's own output and the combined null sink.
fn sinks_present() -> Vec<String> {
    [
        "alsa_output.pci-0000_00_1f.3.analog-stereo",
        "bluez_output.80_99_E7_63_50_29.1",
        "blue2th_combined",
    ]
    .map(String::from)
    .to_vec()
}

/// [`sinks_present`] without the Bluetooth speaker — the graph while it is off.
fn sinks_without_speaker() -> Vec<String> {
    [
        "alsa_output.pci-0000_00_1f.3.analog-stereo",
        "blue2th_combined",
    ]
    .map(String::from)
    .to_vec()
}

// Criterion: a speaker whose sink is absent — switched off — is not part of
// what the reconciliation compares against. It is what keeps a rebuild from
// being asked for on every tick, since a missing branch rebuilds all.
#[test]
fn test_reachable_branches_drops_a_speaker_whose_sink_is_absent() {
    let spec = two_speaker_spec();

    let reachable = reachable_branches(&spec.branches, &sinks_present());

    assert_eq!(
        reachable,
        vec![CombineBranch {
            sink: bluez_sink_prefix("80:99:E7:63:50:29"),
            latency_ms: 0,
        }],
        "only the speaker with a live node survives, got {reachable:?}"
    );
}

// Criterion: an unreadable listing must not read as "every speaker is gone".
// The caller guards on it; this pins what the pure half answers, so the guard
// cannot be dropped as redundant.
#[test]
fn test_reachable_branches_of_an_empty_listing_keeps_nothing() {
    assert!(
        reachable_branches(&two_speaker_spec().branches, &[]).is_empty(),
        "an empty listing names no node, so it reaches no speaker"
    );
}

// Criterion: the churn that made the whole selection rebuild every five
// seconds while one speaker was switched off. Reconciled against the
// speakers actually present, a selection missing an absent one is a no-op.
#[test]
fn test_reconcile_branches_a_switched_off_speaker_asks_for_no_rebuild() {
    let spec = two_speaker_spec();
    let present = CombineSinkSpec {
        sink_name: spec.sink_name.clone(),
        branches: reachable_branches(&spec.branches, &sinks_present()),
    };
    let loaded = vec![CombineBranch {
        sink: "bluez_output.80_99_E7_63_50_29.1".to_string(),
        latency_ms: 0,
    }];

    let plan = reconcile_branches(&loaded, &present);

    assert_eq!(
        plan,
        BranchPlan::default(),
        "the speaker that is playing keeps its branch while the other is off"
    );
}

// Criterion: `CombineBranch::sink` holds the `bluez_output.<MAC>` prefix while
// a loaded module names the resolved node `bluez_output.<MAC>.1`. Without
// this, reconciliation would believe nothing is loaded and rebuild the whole
// graph on every change — the very defect being fixed.
#[test]
fn test_reconcile_branches_matches_a_prefix_against_the_resolved_node() {
    let spec = combine_sink_plan(&[SpeakerTarget {
        address: "80:99:E7:63:50:29".to_string(),
        offset_ms: 0,
    }]);
    assert_eq!(
        spec.branches[0].sink, "bluez_output.80_99_E7_63_50_29",
        "the plan holds the prefix, not the resolved node"
    );
    let loaded = vec![CombineBranch {
        sink: "bluez_output.80_99_E7_63_50_29.1".to_string(),
        latency_ms: 0,
    }];

    let plan = reconcile_branches(&loaded, &spec);

    assert_eq!(
        plan,
        BranchPlan::default(),
        "the prefix names the loaded node, so this branch is already up"
    );
}

// The prefix names the loaded node only up to the `.` PipeWire puts before the
// card index: a node that merely *opens* with the prefix is a different node,
// the rule `sink_matching_prefix` already resolves by. A plain `starts_with`
// here would call that foreign branch this speaker's and leave it in place.
#[test]
fn test_reconcile_branches_prefix_matches_the_node_only_at_a_dot_boundary() {
    let spec = combine_sink_plan(&[SpeakerTarget {
        address: "80:99:E7:63:50:29".to_string(),
        offset_ms: 0,
    }]);
    // At the planned delay, a prefix match would call it "already up"; at
    // another delay, it would retune a foreign branch in place.
    for latency_ms in [0, 30] {
        let loaded = vec![CombineBranch {
            sink: format!("{}_2.1", spec.branches[0].sink),
            latency_ms,
        }];

        let plan = reconcile_branches(&loaded, &spec);

        assert_eq!(
            plan,
            BranchPlan {
                to_load: spec.branches.clone(),
                to_retune: Vec::new(),
                to_unload: loaded,
            },
            "the node continuing the prefix without a `.` is not this branch"
        );
    }
}

// A branch that names no node matches nothing, not even another nameless one:
// an empty string equals an empty string, which is how a missing target used to
// claim an arbitrary node (see `sink_matching_prefix`). `reconcile_branches` is
// public and its inputs come from a subprocess, so the guard has to live in the
// comparison rather than in its callers.
#[test]
fn test_reconcile_branches_a_branch_naming_no_node_matches_nothing() {
    for (loaded_ms, planned_ms) in [(0, 0), (30, 0)] {
        let loaded = CombineBranch {
            sink: String::new(),
            latency_ms: loaded_ms,
        };
        let planned = CombineBranch {
            sink: String::new(),
            latency_ms: planned_ms,
        };
        let spec = CombineSinkSpec {
            sink_name: "blue2th_combined".to_string(),
            branches: vec![planned.clone()],
        };

        let plan = reconcile_branches(std::slice::from_ref(&loaded), &spec);

        assert_eq!(
            plan,
            BranchPlan {
                to_load: vec![planned],
                to_retune: Vec::new(),
                to_unload: vec![loaded],
            },
            "two empty names are not the same node"
        );
    }
}

// Criterion (same one, the other side): the prefix comparison must not match
// a different speaker's node, or a selection change would leave the wrong
// branch in place and never load the right one.
#[test]
fn test_reconcile_branches_prefix_does_not_match_another_speakers_node() {
    let spec = combine_sink_plan(&[SpeakerTarget {
        address: "80:99:E7:63:50:29".to_string(),
        offset_ms: 0,
    }]);
    for latency_ms in [0, 30] {
        let other = CombineBranch {
            sink: "bluez_output.11_22_33_44_55_66.1".to_string(),
            latency_ms,
        };

        let plan = reconcile_branches(std::slice::from_ref(&other), &spec);

        assert_eq!(
            plan,
            BranchPlan {
                to_load: vec![CombineBranch {
                    sink: "bluez_output.80_99_E7_63_50_29".to_string(),
                    latency_ms: 0,
                }],
                to_retune: Vec::new(),
                to_unload: vec![other],
            },
            "the other speaker's branch is neither this one nor retuned into it"
        );
    }
}

// Criterion: a pure decision says whether the repair pass runs at all — a
// non-empty selection with something playing runs.
#[test]
fn test_should_repair_branches_with_a_selection_and_audio_runs() {
    let selection = vec![SpeakerTarget {
        address: "80:99:E7:63:50:29".to_string(),
        offset_ms: 0,
    }];

    assert!(should_repair_branches(&selection, true));
}

// Criterion: an empty selection does not run — there is nothing to repair, and
// the pass must not build a combined sink on its own.
#[test]
fn test_should_repair_branches_with_an_empty_selection_does_not_run() {
    assert!(!should_repair_branches(&[], true));
}

// Criterion: nothing playing does not run — a dead branch matters only while
// audio flows, and skipping keeps the idle cost at zero.
#[test]
fn test_should_repair_branches_with_nothing_playing_does_not_run() {
    let selection = vec![SpeakerTarget {
        address: "80:99:E7:63:50:29".to_string(),
        offset_ms: 0,
    }];

    assert!(!should_repair_branches(&selection, false));
}

const DEAD: &str = "bluez_output.10_28_74_E7_6A_56";
const LIVE: &str = "bluez_output.80_99_E7_63_50_29";

fn planned_branch(sink: &str, latency_ms: u32) -> CombineBranch {
    CombineBranch {
        sink: sink.to_string(),
        latency_ms,
    }
}

// Criterion: with one branch failing and one succeeding, the succeeding one is
// loaded — the rule the hardware caught, where a repair fixed the previous
// speaker and failed on the current one. Two speakers, the first unresolvable:
// the second must still be attempted and loaded.
#[test]
fn test_load_planned_branches_failing_first_still_loads_the_second() {
    let plan = vec![planned_branch(DEAD, 0), planned_branch(LIVE, 40)];
    let resolved = RefCell::new(Vec::new());
    let loaded = RefCell::new(Vec::new());

    let report = load_planned_branches(
        &plan,
        |branch| {
            resolved.borrow_mut().push(branch.sink.clone());
            if branch.sink == DEAD {
                Err(AudioError::PipeWire(format!(
                    "no PipeWire sink for prefix {}",
                    branch.sink
                )))
            } else {
                Ok(format!("{}.1", branch.sink))
            }
        },
        |_branch, real_sink| {
            loaded.borrow_mut().push(real_sink.to_string());
            Ok(())
        },
    );

    assert_eq!(
        *resolved.borrow(),
        vec![DEAD.to_string(), LIVE.to_string()],
        "an unresolvable branch must not stop the next one being attempted"
    );
    assert_eq!(
        *loaded.borrow(),
        vec![format!("{LIVE}.1")],
        "the speaker whose node exists must be fed"
    );
    assert_eq!(report.loaded, vec![LIVE.to_string()]);
    assert_eq!(
        report.failures.len(),
        1,
        "the failure is still reported, so the caller warns and the next tick retries"
    );
}

// Criterion: every branch in the plan is attempted, and the failures are
// reported only after the whole set has been tried — here the failing branch
// comes last, and the first must already have been loaded.
#[test]
fn test_load_planned_branches_failing_second_still_loads_the_first() {
    let plan = vec![planned_branch(LIVE, 40), planned_branch(DEAD, 0)];
    let resolved = RefCell::new(Vec::new());
    let loaded = RefCell::new(Vec::new());

    let report = load_planned_branches(
        &plan,
        |branch| {
            resolved.borrow_mut().push(branch.sink.clone());
            if branch.sink == DEAD {
                Err(AudioError::PipeWire(format!(
                    "no PipeWire sink for prefix {}",
                    branch.sink
                )))
            } else {
                Ok(format!("{}.1", branch.sink))
            }
        },
        |_branch, real_sink| {
            loaded.borrow_mut().push(real_sink.to_string());
            Ok(())
        },
    );

    assert_eq!(*resolved.borrow(), vec![LIVE.to_string(), DEAD.to_string()]);
    assert_eq!(*loaded.borrow(), vec![format!("{LIVE}.1")]);
    assert_eq!(report.loaded, vec![LIVE.to_string()]);
    assert_eq!(report.failures.len(), 1);
}

// Criterion: every branch in the plan is attempted — when they all resolve,
// they are all loaded and nothing is reported as failed.
#[test]
fn test_load_planned_branches_all_resolvable_loads_every_branch() {
    let plan = vec![planned_branch(LIVE, 40), planned_branch(DEAD, 120)];
    let loaded = RefCell::new(Vec::new());

    let report = load_planned_branches(
        &plan,
        |branch| Ok(format!("{}.1", branch.sink)),
        |branch, real_sink| {
            loaded
                .borrow_mut()
                .push((real_sink.to_string(), branch.latency_ms));
            Ok(())
        },
    );

    assert_eq!(
        *loaded.borrow(),
        vec![(format!("{LIVE}.1"), 40), (format!("{DEAD}.1"), 120)],
        "each branch is loaded onto its resolved node with its own latency"
    );
    assert_eq!(report.loaded, vec![LIVE.to_string(), DEAD.to_string()]);
    assert!(
        report.failures.is_empty(),
        "a fully resolvable plan reports no failure"
    );
}

// Criterion: every branch in the plan is attempted — an empty plan attempts
// nothing and reports no failure, so an idle selection stays a no-op.
#[test]
fn test_load_planned_branches_of_an_empty_plan_attempts_nothing() {
    let attempts = RefCell::new(0_usize);

    let report = load_planned_branches(
        &[],
        |branch| {
            *attempts.borrow_mut() += 1;
            Ok(branch.sink.clone())
        },
        |_branch, _real_sink| {
            *attempts.borrow_mut() += 1;
            Ok(())
        },
    );

    assert_eq!(*attempts.borrow(), 0);
    assert_eq!(report, BranchLoadReport::default());
}

// Criterion: the failures are reported only after the whole set has been
// tried — with every branch unresolvable, every branch is still attempted and
// each failure comes back.
#[test]
fn test_load_planned_branches_reports_every_failure_after_trying_all() {
    let plan = vec![planned_branch(DEAD, 0), planned_branch(LIVE, 40)];
    let resolved = RefCell::new(Vec::new());

    let report = load_planned_branches(
        &plan,
        |branch| {
            resolved.borrow_mut().push(branch.sink.clone());
            Err(AudioError::PipeWire(format!(
                "no PipeWire sink for prefix {}",
                branch.sink
            )))
        },
        |_branch, _real_sink| Ok(()),
    );

    assert_eq!(
        *resolved.borrow(),
        vec![DEAD.to_string(), LIVE.to_string()],
        "the first failure must not end the pass"
    );
    assert!(report.loaded.is_empty());
    assert_eq!(
        report.failures.len(),
        2,
        "one message per branch that could not be loaded"
    );
}

/// The speaker of the third branch in the typed-failure tests below: one
/// that resolves and loads.
const THIRD: &str = "bluez_output.2C_FD_B4_D3_AC_21";

/// A report of a pass that loaded `LIVE` and failed with `failures`.
fn report_failing(failures: Vec<AudioError>) -> BranchLoadReport {
    BranchLoadReport {
        loaded: vec![LIVE.to_string()],
        failures,
    }
}

/// The refusal the fake graph answers a load it was told to fail.
fn refused_load() -> AudioError {
    AudioError::PipeWire(format!("fake graph: LoadBranch told to fail for {DEAD}.1"))
}

// Criterion (#147, 2026-10-03): `BranchLoadReport` keeps its failures as
// `AudioError`s, and `into_result` answers `Ok(())` with none,
// `Unanswered` when one is `Unanswered`, and `PipeWire` with the messages
// joined by "; " otherwise — each refusal's own text, without the
// "PipeWire error: " its `Display` adds, which the joined error's
// `Display` adds once. The whole is compared, so a join of each
// failure's `Display` fails.
#[test]
fn test_branch_load_report_answers_ok_unanswered_or_the_joined_pipewire_text() {
    let none = report_failing(vec![]).into_result();
    let stalled = report_failing(vec![AudioError::Unanswered]).into_result();
    let answered = report_failing(vec![
        refused_load(),
        AudioError::PipeWire(format!("no PipeWire sink for prefix {THIRD}")),
    ])
    .into_result();

    assert_eq!(none, Ok(()));
    assert_eq!(
        stalled,
        Err(AudioError::Unanswered),
        "a pass that stalled is a daemon that did not answer, not a refusal"
    );
    assert_eq!(
        answered,
        Err(AudioError::PipeWire(format!(
            "fake graph: LoadBranch told to fail for {DEAD}.1; \
             no PipeWire sink for prefix {THIRD}"
        ))),
        "answers only: the refusals' own texts, joined"
    );
}

// Guard (#147, 2026-10-03, a branch pass is `Unanswered` only when a
// failure is): the near misses are a report holding one refusal alone,
// which must stay `PipeWire` with its own exact text — a rule keyed on
// "any failure" answers it 503 — and the same refusal worded as the
// deadline exit used to word itself, which a rule matching "did not
// answer" in the message would take for a stall. Beside them, a refusal
// *then* a stall, and a stall *then* a refusal: both `Unanswered`, so a
// rule reading the first failure only, or the last only, fails one.
#[test]
fn test_branch_load_report_is_unanswered_only_when_a_failure_is_whatever_its_position() {
    let alone = report_failing(vec![refused_load()]).into_result();
    let worded = report_failing(vec![AudioError::PipeWire(
        "PipeWire did not answer a sync round trip".to_string(),
    )])
    .into_result();
    let refusal_then_stall =
        report_failing(vec![refused_load(), AudioError::Unanswered]).into_result();
    let stall_then_refusal =
        report_failing(vec![AudioError::Unanswered, refused_load()]).into_result();

    assert_eq!(
        alone,
        Err(AudioError::PipeWire(format!(
            "fake graph: LoadBranch told to fail for {DEAD}.1"
        ))),
        "a refused load is an answer: it stays `PipeWire`, with its own text"
    );
    assert_eq!(
        worded,
        Err(AudioError::PipeWire(
            "PipeWire did not answer a sync round trip".to_string()
        )),
        "the variant decides, never the wording"
    );
    assert_eq!(
        refusal_then_stall,
        Err(AudioError::Unanswered),
        "a reading of the first failure alone misses the stall"
    );
    assert_eq!(
        stall_then_refusal,
        Err(AudioError::Unanswered),
        "a reading of the last failure alone misses the stall"
    );
}

// Criterion (found in #152's manual verification): a branch failure is
// named once in the error a pass answers. `AudioError::PipeWire`'s own
// `Display` prefixes "PipeWire error: ", and joining each failure's
// `Display` inside a new `PipeWire` doubled it: the log read
// "PipeWire error: PipeWire error: no PipeWire sink for prefix …". One
// refusal, two refusals, and a refusal beside another variant, which keeps
// its own readable text inside the joined one.
#[test]
fn test_branch_load_report_names_a_pipewire_failure_once() {
    let absent = || AudioError::PipeWire(format!("no PipeWire sink for prefix {THIRD}"));
    let text = |failures| {
        report_failing(failures)
            .into_result()
            .map_err(|e| e.to_string())
    };

    assert_eq!(
        text(vec![absent()]),
        Err(format!(
            "PipeWire error: no PipeWire sink for prefix {THIRD}"
        ))
    );
    assert_eq!(
        text(vec![refused_load(), absent()]),
        Err(format!(
            "PipeWire error: fake graph: LoadBranch told to fail for {DEAD}.1; \
             no PipeWire sink for prefix {THIRD}"
        ))
    );
    assert_eq!(
        text(vec![refused_load(), AudioError::NoSpeakerConnected]),
        Err(format!(
            "PipeWire error: fake graph: LoadBranch told to fail for {DEAD}.1; \
             no speaker connected"
        ))
    );
}

// Criterion (#147, 2026-10-03): `load_planned_branches` keeps each failure
// as the error it was — an unresolvable branch's `PipeWire`, an
// unanswered load's `Unanswered` — in plan order, still attempts the
// branch after them, and its report answers `Unanswered`.
#[test]
fn test_load_planned_branches_keeps_each_failure_as_its_error_and_answers_unanswered() {
    let plan = vec![
        planned_branch(DEAD, 0),
        planned_branch(LIVE, 40),
        planned_branch(THIRD, 80),
    ];

    let report = load_planned_branches(
        &plan,
        |branch| {
            if branch.sink == DEAD {
                Err(AudioError::PipeWire(format!(
                    "no PipeWire sink for prefix {}",
                    branch.sink
                )))
            } else {
                Ok(format!("{}.1", branch.sink))
            }
        },
        |branch, _real_sink| {
            if branch.sink == LIVE {
                Err(AudioError::Unanswered)
            } else {
                Ok(())
            }
        },
    );

    assert_eq!(report.loaded, vec![THIRD.to_string()]);
    assert_eq!(
        report.failures,
        vec![
            AudioError::PipeWire(format!("no PipeWire sink for prefix {DEAD}")),
            AudioError::Unanswered,
        ]
    );
    assert_eq!(report.into_result(), Err(AudioError::Unanswered));
}

fn names(sinks: &[&str]) -> Vec<String> {
    sinks.iter().map(|s| s.to_string()).collect()
}

// Criterion: a branch is confirmed once, at the first pass at least
// `CONFIRM_GAP` after its load — never in the pass that loaded it. The near
// miss: one millisecond short of the gap, as when the app sends a
// selection then a play a moment later (#75: a reload a few milliseconds
// after the load broke a start that worked).
#[test]
fn test_confirmation_register_owes_a_branch_only_once_the_gap_has_passed() {
    let mut register = ConfirmationRegister::default();
    let loaded = Instant::now();
    register.arm(&names(&["bluez_output.B"]), loaded);

    assert!(
        register.take_due(loaded).is_empty(),
        "not in the loading pass"
    );
    assert!(
        register
            .take_due(loaded + CONFIRM_GAP - Duration::from_millis(1))
            .is_empty(),
        "not a moment short of the gap"
    );
    assert_eq!(
        register.take_due(loaded + CONFIRM_GAP),
        names(&["bluez_output.B"])
    );
    assert!(
        register.take_due(loaded + CONFIRM_GAP * 3).is_empty(),
        "and only once: the confirming reload does not arm itself"
    );
}

// Criterion: every branch a pass loads is armed, and two branches loaded
// together are both owed — neither one's reload is lost to the other's.
#[test]
fn test_confirmation_register_owes_every_branch_loaded_together() {
    let mut register = ConfirmationRegister::default();
    let loaded = Instant::now();
    register.arm(&names(&["bluez_output.A", "bluez_output.B"]), loaded);

    assert_eq!(
        register.take_due(loaded + CONFIRM_GAP),
        names(&["bluez_output.A", "bluez_output.B"])
    );
}

// Criterion: each branch waits for its own gap. A branch loaded later is
// still waiting when an earlier one is owed; and loading a branch again
// restarts its wait rather than keeping the older arming.
#[test]
fn test_confirmation_register_times_each_branch_from_its_own_load() {
    let mut register = ConfirmationRegister::default();
    let first = Instant::now();
    let later = first + Duration::from_secs(3);
    register.arm(&names(&["bluez_output.A", "bluez_output.B"]), first);
    register.arm(&names(&["bluez_output.B"]), later);

    assert_eq!(
        register.take_due(first + CONFIRM_GAP),
        names(&["bluez_output.A"]),
        "B was loaded again three seconds later: it waits"
    );
    assert_eq!(
        register.take_due(later + CONFIRM_GAP),
        names(&["bluez_output.B"])
    );
}

// Criterion: a register nothing was armed in never owes a reload however
// long it runs, and an empty name arms nothing — it would name every sink
// to a prefix match.
#[test]
fn test_confirmation_register_owes_nothing_it_was_not_armed_with() {
    let mut register = ConfirmationRegister::default();
    let start = Instant::now();
    register.arm(&names(&[""]), start);

    for tick in 0..10 {
        assert!(
            register.take_due(start + CONFIRM_GAP * tick).is_empty(),
            "tick {tick} owes nothing"
        );
    }
}

// Criterion: the gap is at least the lower bound measured in #75 — five
// seconds between the load and the reload repaired the speaker.
#[test]
fn test_confirm_gap_is_at_least_the_measured_five_seconds() {
    assert!(CONFIRM_GAP >= Duration::from_secs(5));
}

// Criterion: clearing the register — a graph built from nothing — forgets
// every reload armed before.
#[test]
fn test_confirmation_register_clear_forgets_every_armed_reload() {
    let mut register = ConfirmationRegister::default();
    let loaded = Instant::now();
    register.arm(&names(&["bluez_output.A"]), loaded);

    register.clear();

    assert!(register.take_due(loaded + CONFIRM_GAP).is_empty());
}

// ─── #80: the confirmation timer's due time ──────────────────────────────

// Criterion: `next_due` is `None` while nothing is armed — a fresh
// register, and one armed only with an empty name, which arms nothing.
// The control: a named sink armed makes it `Some`.
#[test]
fn test_confirmation_register_next_due_is_none_when_nothing_is_armed() {
    let mut register = ConfirmationRegister::default();
    let loaded = Instant::now();
    assert_eq!(register.next_due(), None, "a fresh register");

    register.arm(&names(&[""]), loaded);
    assert_eq!(register.next_due(), None, "an empty name arms nothing");

    register.arm(&names(&["bluez_output.A"]), loaded);
    assert!(
        register.next_due().is_some(),
        "control: a named sink is armed"
    );
}

// Criterion: `next_due` is the **earliest** armed time plus `CONFIRM_GAP`,
// not the first armed: here A is armed first but later in time, so an
// answer read from the arming order is off by three seconds. Arming B
// again restarts its wait, and the earliest becomes A's.
#[test]
fn test_confirmation_register_next_due_is_the_earliest_armed_plus_the_gap() {
    let mut register = ConfirmationRegister::default();
    let start = Instant::now();
    register.arm(&names(&["bluez_output.A"]), start + Duration::from_secs(3));
    register.arm(&names(&["bluez_output.B"]), start);

    assert_eq!(register.next_due(), Some(start + CONFIRM_GAP));

    register.arm(&names(&["bluez_output.B"]), start + Duration::from_secs(4));
    assert_eq!(
        register.next_due(),
        Some(start + Duration::from_secs(3) + CONFIRM_GAP),
        "B's wait restarted, so A's reload comes first"
    );
}

// Criterion: once a due sink is taken, `next_due` moves on to the next one
// still armed, and to `None` once every reload was handed out — a
// confirming reload does not arm itself.
#[test]
fn test_confirmation_register_next_due_moves_on_once_a_due_sink_is_taken() {
    let mut register = ConfirmationRegister::default();
    let first = Instant::now();
    let second = first + Duration::from_secs(2);
    register.arm(&names(&["bluez_output.A"]), first);
    register.arm(&names(&["bluez_output.B"]), second);

    assert_eq!(
        register.take_due(first + CONFIRM_GAP),
        names(&["bluez_output.A"])
    );
    assert_eq!(register.next_due(), Some(second + CONFIRM_GAP));

    assert_eq!(
        register.take_due(second + CONFIRM_GAP),
        names(&["bluez_output.B"])
    );
    assert_eq!(register.next_due(), None);
}

// Criterion (guard, the gap does not follow the tick): `CONFIRM_GAP` is
// 5 s while the safety net ticks every 30 s. A derivation left in place
// would make the gap 30 s and delay #81's remedy sixfold.
#[test]
fn test_confirm_gap_stays_five_seconds_whatever_the_tick() {
    assert_eq!(CONFIRM_GAP, Duration::from_secs(5));
    assert_ne!(
        CONFIRM_GAP, SAFETY_NET_TICK,
        "the gap no longer derives from the tick"
    );
}

// Criterion: the safety net ticks every 30 s, longer than the gap: the
// confirmation has a timer of its own and never waits for the net.
#[test]
fn test_safety_net_tick_is_thirty_seconds_and_longer_than_the_gap() {
    assert_eq!(SAFETY_NET_TICK, Duration::from_secs(30));
    assert!(SAFETY_NET_TICK > CONFIRM_GAP);
}

// ─── #80: which events wake a repair pass ────────────────────────────────

/// The JBL Xtreme 3 and the WH-1000XM5, as #81's manual verification and a
/// live `pw-dump` (2026-09-26/27) named them.
const JBL: &str = "2C:FD:B4:D3:AC:21";
const JBL_SINK: &str = "bluez_output.2C_FD_B4_D3_AC_21.1";
const SONY: &str = "80:99:E7:63:50:29";
const SONY_SINK: &str = "bluez_output.80_99_E7_63_50_29.1";

fn selected(macs: &[&str]) -> Vec<SpeakerTarget> {
    macs.iter()
        .map(|mac| SpeakerTarget {
            address: mac.to_string(),
            offset_ms: 0,
        })
        .collect()
}

fn appeared(name: &str, at: Instant) -> GraphEvent {
    GraphEvent::SinkAppeared {
        name: name.to_string(),
        at,
    }
}

fn vanished(name: &str, at: Instant) -> GraphEvent {
    GraphEvent::SinkVanished {
        name: name.to_string(),
        at,
    }
}

// Criterion: a sink event wakes a pass when a selected speaker's
// `bluez_sink_prefix` names that sink — both ways, appearing and
// vanishing. The address is mapped through `bluez_sink_prefix`, so a
// selection holding it in lower case still names the upper-case node.
#[test]
fn test_wake_for_wakes_for_a_selected_speakers_sink() {
    let at = Instant::now();
    let selection = selected(&[SONY, JBL]);

    assert!(wake_for(&appeared(JBL_SINK, at), &selection).is_some());
    assert_eq!(
        wake_for(&vanished(SONY_SINK, at), &selection),
        Some(PassReason::SinkVanished {
            name: SONY_SINK.to_string()
        })
    );
    assert!(
        wake_for(&appeared(JBL_SINK, at), &selected(&["2c:fd:b4:d3:ac:21"])).is_some(),
        "a lower-case address names the same sink"
    );
}

// Criterion (non-nominal): an event for a sink no selected speaker names —
// an unselected speaker, the PC's own output — wakes nothing. The control:
// the selected speaker's own sink, in the same selection, wakes.
#[test]
fn test_wake_for_ignores_a_sink_no_selected_speaker_names() {
    let at = Instant::now();
    let selection = selected(&[SONY]);
    assert!(
        wake_for(&appeared(SONY_SINK, at), &selection).is_some(),
        "control: the selected speaker's sink wakes"
    );

    assert_eq!(wake_for(&appeared(JBL_SINK, at), &selection), None);
    assert_eq!(wake_for(&vanished(JBL_SINK, at), &selection), None);
    assert_eq!(
        wake_for(
            &appeared("alsa_output.pci-0000_00_1f.3.analog-stereo", at),
            &selection
        ),
        None
    );
}

// Criterion (guard, exact sink, never a longer address): a selection
// holding `AA:BB:CC:DD:EE:01` does not wake for a sink whose address only
// starts the same. Both near misses pass a bare `starts_with` of the
// prefix; only #81's `.` boundary rejects them. The controls: the exact
// prefix and the prefix followed by `.` wake.
#[test]
fn test_wake_for_does_not_take_a_longer_address_for_a_selected_one() {
    let at = Instant::now();
    let selection = selected(&["AA:BB:CC:DD:EE:01"]);
    assert!(wake_for(
        &appeared("bluez_output.AA_BB_CC_DD_EE_01.1", at),
        &selection
    )
    .is_some());
    assert!(wake_for(&appeared("bluez_output.AA_BB_CC_DD_EE_01", at), &selection).is_some());

    for longer in [
        "bluez_output.AA_BB_CC_DD_EE_01_02.1",
        "bluez_output.AA_BB_CC_DD_EE_010.1",
    ] {
        assert_eq!(
            wake_for(&appeared(longer, at), &selection),
            None,
            "{longer} is another speaker"
        );
        assert_eq!(wake_for(&vanished(longer, at), &selection), None);
    }
}

// Criterion (guard, the empty value): an empty selection wakes nothing —
// no sink event, and not `Reconnected` either: with nothing selected the
// pass has nothing to repair. The control: the same events with a
// selection wake.
#[test]
fn test_wake_for_of_an_empty_selection_wakes_nothing() {
    let at = Instant::now();
    let events = [
        appeared(SONY_SINK, at),
        vanished(SONY_SINK, at),
        GraphEvent::Reconnected,
    ];

    for event in &events {
        assert!(
            wake_for(event, &selected(&[SONY])).is_some(),
            "control: {event:?} wakes with a selection"
        );
        assert_eq!(
            wake_for(event, &[]),
            None,
            "{event:?} with nothing selected"
        );
    }
}

// Criterion: `Reconnected` always wakes a pass, whichever speakers are
// selected — the registry was re-read, so every branch may be gone.
#[test]
fn test_wake_for_always_wakes_on_reconnected() {
    for selection in [selected(&[SONY]), selected(&[JBL]), selected(&[SONY, JBL])] {
        assert_eq!(
            wake_for(&GraphEvent::Reconnected, &selection),
            Some(PassReason::Reconnected)
        );
    }
}

// Criterion: the reason carries the event's sink name and time, so the
// pass line can say what woke it and how long after. Two speakers are
// selected, so a name taken from the selection instead of the event shows.
#[test]
fn test_wake_for_carries_the_sink_name_and_time() {
    let at = Instant::now();
    let selection = selected(&[SONY, JBL]);

    assert_eq!(
        wake_for(&appeared(JBL_SINK, at), &selection),
        Some(PassReason::SinkAppeared {
            name: JBL_SINK.to_string(),
            at
        })
    );
    assert_eq!(
        wake_for(&vanished(JBL_SINK, at), &selection),
        Some(PassReason::SinkVanished {
            name: JBL_SINK.to_string()
        })
    );
}

// Criterion (non-nominal, `restore_during_playback = false`): a speaker
// that dropped off during playback stays in the intent but is not
// re-selected, so its returning sink wakes nothing and it does not start
// playing under the user's hands. The near miss: the speaker is in the
// intent — a filter read from the intent would wake. The control: the
// speaker still selected wakes.
#[test]
fn test_wake_for_ignores_a_returning_speaker_left_out_of_the_selection() {
    let at = Instant::now();
    let both = vec![SONY.to_string(), JBL.to_string()];
    let mut targets = crate::targets::SpeakerTargets::new();
    targets.select(SONY, &both).unwrap();
    targets.select(JBL, &both).unwrap();
    // The JBL is switched off: pruned from the selection, kept in the intent.
    targets.retain_connected(&[SONY.to_string()]);
    assert!(targets.intended().contains(&JBL.to_string()));

    assert_eq!(wake_for(&appeared(JBL_SINK, at), &targets.speakers()), None);
    assert!(
        wake_for(&appeared(SONY_SINK, at), &targets.speakers()).is_some(),
        "control: the speaker still selected wakes"
    );
}

// Criterion: every pass logs what woke it — a sink appeared or vanished
// with its name, the confirmation came due, the safety net, or a
// reconnection. Five reasons, five distinct renderings, and the sink
// events name their sink.
#[test]
fn test_pass_reason_names_what_woke_the_pass() {
    let reasons = [
        PassReason::SinkAppeared {
            name: JBL_SINK.to_string(),
            at: Instant::now(),
        },
        PassReason::SinkVanished {
            name: SONY_SINK.to_string(),
        },
        PassReason::ConfirmationDue,
        PassReason::SafetyNet,
        PassReason::Reconnected,
    ];
    let lines: Vec<String> = reasons.iter().map(|r| r.to_string()).collect();

    assert!(lines[0].contains(JBL_SINK), "got {:?}", lines[0]);
    assert!(lines[1].contains(SONY_SINK), "got {:?}", lines[1]);
    for (i, line) in lines.iter().enumerate() {
        assert!(!line.is_empty(), "reason {i} renders as nothing");
        for other in &lines[i + 1..] {
            assert_ne!(line, other, "two reasons read the same");
        }
    }
}

// ─── #139: the combined sink removed from outside ────────────────────────

const COMBINED_SINK: &str = "blue2th_combined";

fn combined_vanished(at: Instant) -> GraphEvent {
    GraphEvent::CombinedSinkVanished {
        name: COMBINED_SINK.to_string(),
        at,
    }
}

fn combined_reason() -> PassReason {
    PassReason::CombinedSinkVanished {
        name: COMBINED_SINK.to_string(),
    }
}

// Criterion (#139): `wake_for(CombinedSinkVanished, selection)` is
// `Some(PassReason::CombinedSinkVanished)` carrying the sink's name, for
// any non-empty selection — the combined sink is no speaker's sink, so no
// speaker's prefix has to name it.
#[test]
fn test_wake_for_wakes_for_the_combined_sink_s_removal_with_a_selection() {
    let at = Instant::now();
    for selection in [selected(&[SONY]), selected(&[JBL]), selected(&[SONY, JBL])] {
        assert_eq!(
            wake_for(&combined_vanished(at), &selection),
            Some(combined_reason())
        );
    }
}

// Criterion (#139, guard, the empty value): with nothing selected, the
// combined sink's removal wakes nothing — there is nothing to rebuild, and
// nothing to pause for. The control: one selected speaker, and it wakes.
#[test]
fn test_wake_for_the_combined_sink_s_removal_with_nothing_selected_wakes_nothing() {
    let at = Instant::now();
    assert!(
        wake_for(&combined_vanished(at), &selected(&[JBL])).is_some(),
        "control: a selection wakes"
    );

    assert_eq!(wake_for(&combined_vanished(at), &[]), None);
}

// Criterion (#139, guard, the reason is not lost in a burst): wherever the
// combined sink's removal sits in a drained burst — after a selected
// speaker's sink appearing, which a first-wins fold would keep, before
// it, or in the middle of three others that each wake on their own — the
// one pass carries its reason.
#[test]
fn test_wake_for_burst_names_the_combined_sink_s_removal_wherever_it_sits() {
    let at = Instant::now();
    let selection = selected(&[JBL]);
    let bursts = [
        vec![appeared(JBL_SINK, at), combined_vanished(at)],
        vec![combined_vanished(at), appeared(JBL_SINK, at)],
        vec![
            vanished(JBL_SINK, at),
            GraphEvent::Reconnected,
            combined_vanished(at),
            appeared(JBL_SINK, at),
        ],
        vec![combined_vanished(at)],
    ];

    for burst in &bursts {
        assert_eq!(
            wake_for_burst(burst, &selection),
            Some(combined_reason()),
            "burst {burst:?}"
        );
    }
}

// Criterion (#139): without the combined sink's removal, a burst keeps
// today's rule — the first event that wakes names the pass. The
// unselected speaker's sink, first in the burst, wakes nothing and is
// skipped.
#[test]
fn test_wake_for_burst_without_the_combined_sink_s_removal_keeps_the_first_reason() {
    let at = Instant::now();
    let burst = [
        appeared(SONY_SINK, at),
        appeared(JBL_SINK, at),
        GraphEvent::Reconnected,
    ];

    assert_eq!(
        wake_for_burst(&burst, &selected(&[JBL])),
        Some(PassReason::SinkAppeared {
            name: JBL_SINK.to_string(),
            at
        })
    );
}

// Criterion (#139, guard, the empty value): with nothing selected a burst
// carrying the combined sink's removal wakes nothing, and an empty burst
// wakes nothing whatever is selected.
#[test]
fn test_wake_for_burst_of_an_empty_selection_or_burst_wakes_nothing() {
    let at = Instant::now();
    let burst = [appeared(JBL_SINK, at), combined_vanished(at)];

    assert_eq!(wake_for_burst(&burst, &[]), None);
    assert_eq!(wake_for_burst(&[], &selected(&[JBL])), None);
}

// Criterion (#139): `PassReason::CombinedSinkVanished` renders a line of
// its own naming the combined sink — not the line a speaker's sink
// vanishing reads, even for the same name, and none of the others.
#[test]
fn test_pass_reason_of_the_combined_sink_s_removal_names_the_sink_and_reads_apart() {
    let line = combined_reason().to_string();

    assert!(line.contains(COMBINED_SINK), "got {line:?}");
    let others = [
        PassReason::SinkAppeared {
            name: COMBINED_SINK.to_string(),
            at: Instant::now(),
        },
        PassReason::SinkVanished {
            name: COMBINED_SINK.to_string(),
        },
        PassReason::ConfirmationDue,
        PassReason::SafetyNet,
        PassReason::Reconnected,
    ];
    for other in &others {
        assert_ne!(line, other.to_string(), "reads like {other:?}");
    }
}

/// Every reason but the combined sink's removal.
fn other_reasons() -> Vec<PassReason> {
    vec![
        PassReason::SinkAppeared {
            name: JBL_SINK.to_string(),
            at: Instant::now(),
        },
        PassReason::SinkVanished {
            name: JBL_SINK.to_string(),
        },
        PassReason::ConfirmationDue,
        PassReason::SafetyNet,
        PassReason::Reconnected,
    ]
}

// Criterion (#139): a pass woken by the combined sink's removal whose
// route failed falls back to the pause, whatever the re-targeting said —
// a failed build may never have reached it.
#[test]
fn test_fallback_pause_due_after_the_combined_sink_s_removal_when_the_route_fails() {
    assert!(fallback_pause_due(&combined_reason(), false, true));
    assert!(fallback_pause_due(&combined_reason(), false, false));
}

// Criterion (#139): a pass woken by the combined sink's removal whose
// route succeeded but whose re-targeting failed falls back too: the sink
// is back, the music is not on it.
#[test]
fn test_fallback_pause_due_after_the_combined_sink_s_removal_when_the_retarget_fails() {
    assert!(fallback_pause_due(&combined_reason(), true, false));
}

// Criterion (#139, guard, only on failure): a pass woken by the combined
// sink's removal that rebuilt and re-targeted never pauses. The control:
// the same reason with the route failed does.
#[test]
fn test_fallback_pause_due_never_once_the_rebuild_and_the_retarget_succeeded() {
    assert!(
        fallback_pause_due(&combined_reason(), false, true),
        "control: a failed route falls back"
    );

    assert!(!fallback_pause_due(&combined_reason(), true, true));
}

// Criterion (#139, guard, only this reason pauses): a pass woken by any
// other reason never falls back, whichever of the route and the
// re-targeting failed. The control: the combined sink's removal with the
// same failed route does.
#[test]
fn test_fallback_pause_due_never_for_another_reason() {
    assert!(
        fallback_pause_due(&combined_reason(), false, true),
        "control: the combined sink's removal falls back"
    );

    for reason in &other_reasons() {
        for (routed_ok, retargeted_ok) in
            [(false, true), (false, false), (true, false), (true, true)]
        {
            assert!(
                !fallback_pause_due(reason, routed_ok, retargeted_ok),
                "{reason:?} with routed_ok={routed_ok} retargeted_ok={retargeted_ok}"
            );
        }
    }
}

// Criterion: with every selected speaker switched off, nothing is reachable —
// and the reconciliation asks for no rebuild at all rather than for the whole
// selection. Asking would spawn a load per tick for nodes that do not exist.
#[test]
fn test_reconcile_branches_with_every_speaker_switched_off_loads_nothing() {
    let spec = two_speaker_spec();
    let none_present = CombineSinkSpec {
        sink_name: spec.sink_name.clone(),
        branches: reachable_branches(&spec.branches, &sinks_without_speaker()),
    };
    let loaded = vec![CombineBranch {
        sink: "bluez_output.80_99_E7_63_50_29.1".to_string(),
        latency_ms: 0,
    }];

    assert!(
        none_present.branches.is_empty(),
        "neither speaker has a node in this listing"
    );

    let plan = reconcile_branches(&loaded, &none_present);

    assert_eq!(
        plan,
        BranchPlan {
            to_load: Vec::new(),
            to_retune: Vec::new(),
            to_unload: loaded,
        },
        "nothing is loaded onto a node that is not there; the branch left \
         over from the speaker that is now off is dropped"
    );
}

/// What a graph loaded from [`two_speaker_spec`] reports: one branch per
/// speaker on the **resolved** node, each at the latency the plan asked for.
fn two_speakers_loaded() -> Vec<CombineBranch> {
    vec![
        CombineBranch {
            sink: "bluez_output.80_99_E7_63_50_29.1".to_string(),
            latency_ms: 0,
        },
        CombineBranch {
            sink: "bluez_output.11_22_33_44_55_66.1".to_string(),
            latency_ms: 250,
        },
    ]
}

// Criterion (moved back from the graph module of (#78)): what the graph
// reports loaded and what the plan asks for are the same quantity — the
// offset, with no base. Adding anything at load time would have every
// reconciliation see a mismatch and retune every branch on every tick.
#[test]
fn test_branch_latency_round_trips_from_the_plan_through_the_loaded_branches() {
    let spec = combine_sink_plan(&[
        SpeakerTarget {
            address: "80:99:E7:63:50:29".to_string(),
            offset_ms: 0,
        },
        SpeakerTarget {
            address: "11:22:33:44:55:66".to_string(),
            offset_ms: 70,
        },
    ]);
    // What the graph reports back for a graph loaded from this very plan: one
    // branch per planned one, on the resolved node, at the planned latency.
    let loaded: Vec<CombineBranch> = spec
        .branches
        .iter()
        .map(|branch| CombineBranch {
            sink: format!("{}.1", branch.sink),
            latency_ms: branch.latency_ms,
        })
        .collect();

    assert_eq!(
        loaded.iter().map(|b| b.latency_ms).collect::<Vec<_>>(),
        vec![0, 70],
        "the plan carries the offsets as they are, and so do the loaded branches"
    );
    assert_eq!(
        reconcile_branches(&loaded, &spec),
        BranchPlan::default(),
        "a graph loaded from the plan reconciles against it as a no-op"
    );
}

// Criterion (moved back): a speaker present in both the loaded set and the
// spec with the same latency appears in neither list — an unchanged
// selection touches nothing, which is what keeps the stream alive.
#[test]
fn test_reconcile_branches_unchanged_selection_changes_nothing() {
    let spec = two_speaker_spec();

    let plan = reconcile_branches(&two_speakers_loaded(), &spec);

    assert_eq!(plan, BranchPlan::default());
}

// Criterion (moved back): a speaker dropped from the selection yields exactly
// one unload, naming the resolved node its branch feeds, and never the null
// sink.
#[test]
fn test_reconcile_branches_dropped_speaker_unloads_only_that_branch() {
    let spec = combine_sink_plan(&[SpeakerTarget {
        address: "80:99:E7:63:50:29".to_string(),
        offset_ms: 0,
    }]);

    let plan = reconcile_branches(&two_speakers_loaded(), &spec);

    assert_eq!(
        plan,
        BranchPlan {
            to_load: Vec::new(),
            to_retune: Vec::new(),
            to_unload: vec![CombineBranch {
                sink: "bluez_output.11_22_33_44_55_66.1".to_string(),
                latency_ms: 250,
            }],
        },
        "only the deselected speaker's branch is unloaded"
    );
    assert!(
        !plan
            .to_unload
            .iter()
            .any(|b| b.sink.contains(&spec.sink_name)),
        "the null sink is never unloaded by a selection change: {:?}",
        plan.to_unload
    );
}

// Criterion (guard, retune, never reload): a loaded branch for the same
// speaker at another delay is retuned in place — it goes to `to_retune`,
// carrying the resolved node and the planned delay, never to `to_unload`
// plus `to_load`. The other speaker is untouched.
#[test]
fn test_reconcile_branches_offset_change_retunes_in_place() {
    let spec = combine_sink_plan(&[
        SpeakerTarget {
            address: "80:99:E7:63:50:29".to_string(),
            offset_ms: 0,
        },
        SpeakerTarget {
            address: "11:22:33:44:55:66".to_string(),
            offset_ms: 400,
        },
    ]);

    let plan = reconcile_branches(&two_speakers_loaded(), &spec);

    assert_eq!(
        plan,
        BranchPlan {
            to_load: Vec::new(),
            to_retune: vec![CombineBranch {
                sink: "bluez_output.11_22_33_44_55_66.1".to_string(),
                latency_ms: 400,
            }],
            to_unload: Vec::new(),
        }
    );
}

// Criterion (guard, missing branch loads alone): with speaker A loaded and
// live and speaker B's branch missing — dropped by the router once ruled
// dead — the plan loads B alone. The #75 rule would have unloaded A too.
#[test]
fn test_reconcile_branches_missing_branch_loads_it_alone() {
    let spec = two_speaker_spec();
    let a = CombineBranch {
        sink: "bluez_output.80_99_E7_63_50_29.1".to_string(),
        latency_ms: 0,
    };

    let plan = reconcile_branches(&[a], &spec);

    assert_eq!(
        plan,
        BranchPlan {
            to_load: vec![CombineBranch {
                sink: "bluez_output.11_22_33_44_55_66".to_string(),
                latency_ms: 250,
            }],
            to_retune: Vec::new(),
            to_unload: Vec::new(),
        }
    );
}

// Criterion: the three sets are disjoint — every speaker lands in exactly
// one of them, or in none when its branch already matches: one to load,
// one to retune, one to unload and one left alone, in the same pass.
#[test]
fn test_reconcile_branches_sorts_each_speaker_into_exactly_one_set() {
    let spec = combine_sink_plan(&[
        SpeakerTarget {
            address: "AA:BB:CC:DD:EE:01".to_string(),
            offset_ms: 100,
        },
        SpeakerTarget {
            address: "AA:BB:CC:DD:EE:02".to_string(),
            offset_ms: 30,
        },
        SpeakerTarget {
            address: "AA:BB:CC:DD:EE:03".to_string(),
            offset_ms: 60,
        },
    ]);
    let loaded = vec![
        CombineBranch {
            sink: "bluez_output.AA_BB_CC_DD_EE_01.1".to_string(),
            latency_ms: 0,
        },
        CombineBranch {
            sink: "bluez_output.AA_BB_CC_DD_EE_03.1".to_string(),
            latency_ms: 60,
        },
        CombineBranch {
            sink: "bluez_output.AA_BB_CC_DD_EE_04.1".to_string(),
            latency_ms: 30,
        },
    ];

    let plan = reconcile_branches(&loaded, &spec);

    assert_eq!(
        plan,
        BranchPlan {
            to_load: vec![CombineBranch {
                sink: "bluez_output.AA_BB_CC_DD_EE_02".to_string(),
                latency_ms: 30,
            }],
            to_retune: vec![CombineBranch {
                sink: "bluez_output.AA_BB_CC_DD_EE_01.1".to_string(),
                latency_ms: 100,
            }],
            to_unload: vec![CombineBranch {
                sink: "bluez_output.AA_BB_CC_DD_EE_04.1".to_string(),
                latency_ms: 30,
            }],
        }
    );
}
