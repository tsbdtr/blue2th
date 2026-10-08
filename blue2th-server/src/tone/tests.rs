// SPDX-License-Identifier: MIT OR Apache-2.0

use super::*;

const COMBINED: &str = "blue2th_combined";

/// The whole tone, asked for with room to spare: nothing past its end
/// comes back.
fn whole_tone() -> Vec<f32> {
    tone_frames(0, 200_000)
}

/// The left channel of interleaved stereo samples.
fn left(samples: &[f32]) -> Vec<f32> {
    samples.iter().step_by(2).copied().collect()
}

fn peak(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0_f32, |max, s| max.max(s.abs()))
}

// ─── tone_frames ─────────────────────────────────────────────────────────

// Criterion: the tone is 48 kHz and 96 000 frames (2 s) in total, over any
// chunking — including chunk sizes that do not divide it, where an
// off-by-one would add or drop a frame — and nothing comes after its end.
#[test]
fn test_tone_frames_lasts_two_seconds_at_48_khz() {
    assert_eq!(TONE_RATE, 48_000);
    assert_eq!(TONE_FRAMES, 96_000);
    assert_eq!(TONE_FRAMES, 2 * TONE_RATE as usize);
    assert_eq!(whole_tone().len(), 2 * 96_000, "stereo, interleaved");

    for chunk in [1_024, 441, 1_000, 96_000] {
        let mut position = 0;
        let mut sizes = Vec::new();
        // Bounded: a generator that never runs dry must fail, not hang.
        for _ in 0..1_000 {
            let samples = tone_frames(position, chunk);
            if samples.is_empty() {
                break;
            }
            assert_eq!(samples.len() % 2, 0, "whole frames only");
            sizes.push(samples.len() / 2);
            position += samples.len() / 2;
        }
        assert_eq!(position, 96_000, "chunks of {chunk}: {sizes:?}");
        let (last, full) = sizes.split_last().unwrap();
        assert!(
            full.iter().all(|size| *size == chunk),
            "every chunk but the last is whole, chunks of {chunk}: {sizes:?}"
        );
        assert_eq!(*last, 96_000 - chunk * full.len());
    }

    assert_eq!(tone_frames(95_999, 1_024).len(), 2, "one frame is left");
    assert!(tone_frames(96_000, 1_024).is_empty());
    assert!(tone_frames(1_000_000, 10).is_empty());
    assert!(tone_frames(0, 0).is_empty());
}

// Criterion: the tone is a 440 Hz sine — 880 zero crossings a second over
// its steady part, between the fades — and it is not silent. The tone it
// replaces (`assets/test-tone.wav`, `ffprobe`: 2.00 s, 48 000 Hz, 2
// channels, 440 Hz) carried the same claims.
#[test]
fn test_tone_frames_is_a_440_hz_sine() {
    assert_eq!(TONE_FREQUENCY_HZ, 440.0);
    let tone = left(&whole_tone());
    assert_eq!(tone.len(), 96_000);
    let steady = &tone[480..96_000 - 480];

    let crossings = steady
        .windows(2)
        .filter(|pair| (pair[0] < 0.0) != (pair[1] < 0.0))
        .count();
    // 95 040 frames at 48 kHz is 1.98 s; 441 Hz would give 1 746.
    let expected = 880.0 * steady.len() as f64 / 48_000.0;
    assert!(
        (crossings as f64 - expected).abs() <= 2.0,
        "{crossings} zero crossings, expected about {expected}"
    );

    let rms =
        (steady.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / steady.len() as f64).sqrt();
    let expected_rms = 0.25 / 2.0_f64.sqrt();
    assert!(
        (rms - expected_rms).abs() < 0.002,
        "RMS {rms}, expected about {expected_rms}"
    );
}

// Criterion: the tone peaks at `TONE_AMPLITUDE` (0.25), and never above.
#[test]
fn test_tone_frames_peaks_at_the_tone_amplitude() {
    assert_eq!(TONE_AMPLITUDE, 0.25);
    let tone = whole_tone();
    assert!(!tone.is_empty(), "the tone has samples");

    let max = peak(&tone);

    assert!(max <= 0.25 + 1e-6, "peaks at {max}, above 0.25");
    assert!(max >= 0.2498, "peaks at {max}, below 0.25");
}

// Criterion (guard, no click): a 10 ms linear fade-in and fade-out — the
// first and the last frame are exactly 0.0 (a sine with no fade starts at
// 0 but ends mid-cycle: its frame 95 999 is about -0.014), the envelope
// rises over the first 480 frames and falls over the last 480, never above
// the linear ramp, and the tone is at full amplitude right after.
#[test]
fn test_tone_frames_fades_in_and_out_without_a_click() {
    assert_eq!(TONE_FADE_FRAMES, 480);
    let tone = whole_tone();
    assert_eq!(tone.len(), 192_000);
    assert_eq!((tone[0], tone[1]), (0.0, 0.0), "the first frame");
    assert_eq!((tone[191_998], tone[191_999]), (0.0, 0.0), "the last frame");

    let channel = left(&tone);
    for (n, sample) in channel.iter().enumerate().take(480) {
        let ramp = 0.25 * n as f32 / 479.0;
        assert!(
            sample.abs() <= ramp + 1e-6,
            "frame {n} is {sample}, above the fade-in's {ramp}"
        );
    }
    for (n, sample) in channel.iter().enumerate().skip(96_000 - 480) {
        let ramp = 0.25 * (95_999 - n) as f32 / 479.0;
        assert!(
            sample.abs() <= ramp + 1e-6,
            "frame {n} is {sample}, above the fade-out's {ramp}"
        );
    }

    // One window per cycle or so: the envelope rises, then falls.
    let rising: Vec<f32> = [0..110, 110..220, 220..330, 330..440]
        .into_iter()
        .map(|window| peak(&channel[window]))
        .collect();
    assert!(
        rising.windows(2).all(|pair| pair[0] < pair[1]),
        "the fade-in rises: {rising:?}"
    );
    let falling: Vec<f32> = [
        95_560..95_670,
        95_670..95_780,
        95_780..95_890,
        95_890..96_000,
    ]
    .into_iter()
    .map(|window| peak(&channel[window]))
    .collect();
    assert!(
        falling.windows(2).all(|pair| pair[0] > pair[1]),
        "the fade-out falls: {falling:?}"
    );

    // The fades last 10 ms, not longer.
    assert!(peak(&channel[480..600]) >= 0.2495, "full after the fade-in");
    assert!(
        peak(&channel[95_400..95_520]) >= 0.2495,
        "full before the fade-out"
    );
}

// Criterion: the tone is stereo interleaved with L = R, frame by frame.
#[test]
fn test_tone_frames_is_the_same_on_both_channels() {
    let tone = whole_tone();
    assert_eq!(tone.len(), 192_000);

    let (frames, _) = tone.as_chunks::<2>();
    let differing = frames.iter().position(|[left, right]| left != right);

    assert_eq!(differing, None, "a frame with L != R");
    assert!(peak(&tone) > 0.0, "and the channels are not both silent");
}

// Criterion: the generator is a pure function of the position, so a tone
// read in two chunks is the tone read in one — pause and resume keep the
// position, wherever the split falls (in a fade, in the steady part, one
// frame from either end).
#[test]
fn test_tone_frames_continues_where_it_left_off() {
    let whole = whole_tone();
    assert_eq!(whole.len(), 192_000);

    for split in [1, 240, 480, 1_024, 50_000, 95_700, 95_999] {
        let mut joined = tone_frames(0, split);
        joined.extend(tone_frames(split, 96_000));
        assert!(joined == whole, "split at frame {split}");
    }

    let middle = tone_frames(30_000, 777);
    assert!(middle == whole[60_000..61_554], "a chunk from the middle");
}

// ─── write_tone ──────────────────────────────────────────────────────────

/// The F32LE samples a buffer holds.
fn decode(bytes: &[u8]) -> Vec<f32> {
    let (samples, _) = bytes.as_chunks::<4>();
    samples.iter().map(|b| f32::from_le_bytes(*b)).collect()
}

// The buffer the stream hands the daemon holds the generator's samples in
// the format the stream offers — F32 little-endian, interleaved, both
// channels — and the answer is the number of frames written. The chunk is
// taken mid-tone, where a silent or a byte-swapped buffer cannot pass.
#[test]
fn test_write_tone_encodes_the_tone_as_interleaved_little_endian_f32() {
    let mut buffer = vec![0_u8; 1_024 * TONE_STRIDE];

    let written = write_tone(&mut buffer, 30_000);

    assert_eq!(written, 1_024);
    let samples = decode(&buffer);
    assert!(
        samples == tone_frames(30_000, 1_024),
        "not the tone's samples"
    );
    assert!(peak(&samples) > 0.24, "a silent buffer: {}", peak(&samples));
    assert_eq!(samples[0], samples[1], "L = R");
}

// Only whole frames are written: the bytes of a partial frame at the end of
// the buffer are left as they were, and are not counted.
#[test]
fn test_write_tone_writes_whole_frames_only() {
    let mut buffer = vec![0xAA_u8; 10 * TONE_STRIDE + 5];

    let written = write_tone(&mut buffer, 1_000);

    assert_eq!(written, 10);
    assert!(decode(&buffer[..10 * TONE_STRIDE]) == tone_frames(1_000, 10));
    assert_eq!(&buffer[10 * TONE_STRIDE..], &[0xAA; 5]);
}

// At the end of the tone, what is left is written, then nothing: the
// empty answer is what raises `finished`, and the buffer is untouched.
#[test]
fn test_write_tone_at_the_end_writes_what_is_left_then_nothing() {
    let mut buffer = vec![0xAA_u8; 1_024 * TONE_STRIDE];
    assert_eq!(write_tone(&mut buffer, 95_999), 1, "one frame is left");
    assert_eq!(
        decode(&buffer[..TONE_STRIDE]),
        vec![0.0, 0.0],
        "the last frame"
    );

    let mut buffer = vec![0xAA_u8; 1_024 * TONE_STRIDE];
    assert_eq!(write_tone(&mut buffer, 96_000), 0);
    assert!(
        buffer.iter().all(|b| *b == 0xAA),
        "a buffer past the end was written"
    );
}

// ─── tone_stream_props ───────────────────────────────────────────────────

// Criterion (guard, pinned without fallback): the stream is an audio
// playback stream named `blue2th_tone`, `target.object` is exactly the
// target — no card suffix appended, no prefix — and
// `node.dont-reconnect = true` keeps it from being moved to the PC's own
// speakers when the target goes (#67). The second target is the one the
// #110 spike pinned a stream to (2026-09-10).
//
// The pin carries both `node.dont-reconnect` (the target going away mid-tone
// moves it nowhere) and `node.dont-fallback` (a target missing when the
// stream first links sends it nowhere either, never to the default sink —
// the PC's own speakers, #67).
#[test]
fn test_tone_stream_props_pins_the_stream_to_the_target_without_reconnect() {
    for target in [COMBINED, "bluez_output.2C_FD_B4_D3_AC_21.1"] {
        let props = tone_stream_props(target);
        assert!(props.is_ok(), "{target}: {props:?}");

        let mut props = props.unwrap_or_default();
        props.sort();
        let mut expected: Vec<(String, String)> = [
            ("media.type", "Audio"),
            ("media.category", "Playback"),
            ("node.name", "blue2th_tone"),
            ("target.object", target),
            ("node.dont-reconnect", "true"),
            ("node.dont-fallback", "true"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();
        expected.sort();

        assert_eq!(props, expected, "{target}");
    }
}

// Criterion (non-nominal): an empty target is refused — it would leave the
// stream to autoconnect to the default sink, the very fallback the pin
// exists to prevent. A named target, in the same call, is not.
#[test]
fn test_tone_stream_props_refuses_an_empty_target() {
    let refused = tone_stream_props("");

    assert!(
        matches!(refused, Err(AudioError::PipeWire(_))),
        "got {refused:?}"
    );
    assert!(tone_stream_props(COMBINED).is_ok());
}

// ─── PipeWireToneOutput, up to its thread ────────────────────────────────

// Criterion: the output is created lazily — building it, as building the
// app does, starts no thread and so touches no daemon.
#[test]
fn test_tone_output_new_starts_no_thread() {
    let output = PipeWireToneOutput::new(COMBINED);

    assert!(!output.has_thread(), "a thread was started by `new`");
}

// Criterion: only `start` creates the thread — a pause, resume or stop
// before any start has nothing to act on, answers `Ok`, and starts none.
#[test]
fn test_tone_output_commands_before_start_start_no_thread() {
    let mut output = PipeWireToneOutput::new(COMBINED);

    assert!(output.pause().is_ok());
    assert!(output.resume().is_ok());
    assert!(output.stop().is_ok());

    assert!(!output.has_thread(), "a thread was started before `start`");
}

// Criterion (non-nominal): an empty target name is refused before any
// stream is created — `start` errs, and no thread (so no context) exists.
#[test]
fn test_tone_output_with_an_empty_target_refuses_to_start() {
    let mut output = PipeWireToneOutput::new("");

    let started = output.start();

    assert!(
        matches!(started, Err(AudioError::PipeWire(_))),
        "got {started:?}"
    );
    assert!(!output.has_thread(), "a thread was started for no target");
}

// Criterion: `is_finished` is true only once a started tone has run out or
// lost its stream — an output that never started has not finished.
#[test]
fn test_tone_output_is_not_finished_before_it_started() {
    let output = PipeWireToneOutput::new(COMBINED);

    assert!(!output.is_finished());
}
