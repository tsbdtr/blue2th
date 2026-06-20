//! Integration tests for the phase 3 transport feature: route wiring,
//! embedded-file decoding, and the gated PipeWire hardware test.
//!
//! Route tests exercise the router in-process via `app().oneshot(...)`, mirroring
//! the existing `test_health_endpoint_*` style. They are expected to FAIL until
//! the audio engine is implemented (red phase).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use blue2th_proto::PlaybackState;
use tower::ServiceExt; // for `oneshot`

/// The binary crate is not a library, so we rebuild the router here through the
/// `bin` target by including the module under test is not possible; instead we
/// hit the running router via the public test harness exposed by `main.rs`'s
/// `app()`. Because `app()` is private, the integration test drives the same
/// surface through a thin re-export added for tests.
///
/// NOTE: `app()` is `pub(crate)` in the binary; integration tests cannot call it
/// directly. The test therefore relies on a small `pub fn test_app()` exported
/// from the binary's library facade. The implementer must expose the router to
/// integration tests (e.g. via a `lib.rs` or `#[cfg(test)]`-free `pub fn`).
fn build_app() -> axum::Router {
    blue2th_server::app()
}

/// Embedded test tone, read directly from the asset for the decode test.
const TEST_TONE_WAV: &[u8] = include_bytes!("../assets/test-tone.wav");

// Criterion: `GET /playback` returns the current `PlaybackState`.
#[tokio::test]
async fn test_playback_endpoint_returns_state() {
    let request = Request::builder()
        .uri("/playback")
        .body(Body::empty())
        .expect("build request");

    let response = build_app().oneshot(request).await.expect("router response");
    assert_eq!(response.status(), StatusCode::OK);

    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let _state: PlaybackState =
        serde_json::from_slice(&bytes).expect("parse PlaybackState from /playback");
}

// Criterion: `POST /volume` with a malformed body returns a 4xx (no panic).
#[tokio::test]
async fn test_volume_endpoint_rejects_malformed_body() {
    let request = Request::builder()
        .method("POST")
        .uri("/volume")
        .header("content-type", "application/json")
        .body(Body::from("{ not valid json }"))
        .expect("build request");

    let response = build_app().oneshot(request).await.expect("router response");
    assert!(
        response.status().is_client_error(),
        "expected 4xx for malformed body, got {}",
        response.status()
    );
}

// Criterion: `/play` with no connected speaker returns a 4xx error (no panic).
#[tokio::test]
async fn test_play_without_connected_speaker_returns_client_error() {
    let request = Request::builder()
        .method("POST")
        .uri("/play")
        .body(Body::empty())
        .expect("build request");

    let response = build_app().oneshot(request).await.expect("router response");
    assert!(
        response.status().is_client_error(),
        "expected 4xx when no speaker connected, got {}",
        response.status()
    );
}

// Criterion: the embedded test file decodes to non-silent PCM (verified without
// audio output). The tone is a 2s 440Hz stereo sine at 48kHz.
#[test]
fn test_embedded_tone_decodes_to_non_silent_stereo_pcm() {
    use rodio::Source;

    let cursor = std::io::Cursor::new(TEST_TONE_WAV.to_vec());
    let decoder = rodio::Decoder::new(cursor).expect("decode embedded test tone");

    let channels = decoder.channels().get();
    let sample_rate = decoder.sample_rate().get();

    assert_eq!(channels, 2, "test tone must be stereo");
    assert_eq!(sample_rate, 48_000, "test tone must be 48kHz");

    // Collect samples (f32) and compute RMS; a 2s stereo 48kHz file holds about
    // 2 * 2 * 48000 = 192000 samples.
    let samples: Vec<f32> = decoder.collect();
    let expected = 2usize * channels as usize * sample_rate as usize;
    let tolerance = expected / 20; // ±5% for codec priming/padding.
    assert!(
        samples.len().abs_diff(expected) <= tolerance,
        "sample count {} far from expected {}",
        samples.len(),
        expected
    );

    let sum_sq: f64 = samples.iter().map(|s| (*s as f64) * (*s as f64)).sum();
    let rms = (sum_sq / samples.len() as f64).sqrt();
    assert!(rms > 0.0, "decoded audio must be non-silent (RMS > 0)");
}

// Criterion (gated hardware): with a PipeWire `module-null-sink` set as default,
// `audio::play` must produce a running `Stream/Output/Audio` node linked to the
// sink. Requires a live PipeWire daemon, so it is ignored in CI / normal runs.
#[tokio::test]
#[ignore = "requires a live PipeWire daemon; run manually with --ignored"]
async fn test_play_links_running_output_node_to_pipewire_sink() {
    use std::process::Command;

    // Load a null sink and set it as the default so playback has a target.
    let load = Command::new("pactl")
        .args([
            "load-module",
            "module-null-sink",
            "sink_name=blue2th_test_sink",
        ])
        .output()
        .expect("load module-null-sink");
    let module_id = String::from_utf8_lossy(&load.stdout).trim().to_string();
    assert!(!module_id.is_empty(), "module-null-sink failed to load");

    let _ = Command::new("pactl")
        .args(["set-default-sink", "blue2th_test_sink"])
        .status();

    // Drive the engine through the public router so the test mirrors real use.
    let request = Request::builder()
        .method("POST")
        .uri("/play")
        .body(Body::empty())
        .expect("build request");
    let response = build_app().oneshot(request).await.expect("router response");
    assert_eq!(response.status(), StatusCode::OK);

    // Inspect the graph: expect a running Stream/Output/Audio node.
    let dump = Command::new("pw-dump").output().expect("run pw-dump");
    let graph = String::from_utf8_lossy(&dump.stdout);
    assert!(
        graph.contains("Stream/Output/Audio"),
        "no Stream/Output/Audio node found in PipeWire graph"
    );
    assert!(
        graph.contains("\"state\": \"running\""),
        "output node is not in the running state"
    );

    // Tear down: unload the null sink module.
    let _ = Command::new("pactl")
        .args(["unload-module", &module_id])
        .status();
}
