// SPDX-License-Identifier: MIT OR Apache-2.0

//! Integration tests for the phase 3 transport feature: route wiring,
//! embedded-file decoding, and the gated PipeWire hardware test.
//!
//! Route tests exercise the router in-process via `app().oneshot(...)`, mirroring
//! the existing `test_health_endpoint_*` style. They are expected to FAIL until
//! the audio engine is implemented (red phase).

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use blue2th_proto::{PlaybackState, RoutingMode, TargetsState};
use blue2th_server::{auth::AuthStore, spotify_auth::SpotifyAuth};
use tower::ServiceExt; // for `oneshot`

/// The API token these tests pair with. Held in memory only: since phase 6.4 no
/// test may build the router through `app()`, which reloads — and, on a
/// malformed store, rotates — the operator's real API token.
const TOKEN: &str = "test-api-token";

/// Add the bearer every guarded route requires (phase 6.4).
fn authorized(builder: axum::http::request::Builder) -> axum::http::request::Builder {
    builder.header("authorization", format!("Bearer {TOKEN}"))
}

/// The router under test: store-free, with a known API token and an
/// unconfigured Spotify auth driver.
fn build_app() -> axum::Router {
    blue2th_server::app_with_auth_store(
        SpotifyAuth::with_config(None, "blue2th://spotify-callback".to_string()),
        AuthStore::with_token(TOKEN),
    )
}

/// Embedded test tone, read directly from the asset for the decode test.
const TEST_TONE_WAV: &[u8] = include_bytes!("../assets/test-tone.wav");

// Criterion: `GET /playback` returns the current `PlaybackState`.
#[tokio::test]
async fn test_playback_endpoint_returns_state() {
    let request = authorized(Request::builder())
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
    let request = authorized(Request::builder())
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
    let request = authorized(Request::builder())
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
// `AudioEngine::play` (with the real `RodioOutput`) must produce a running
// `Stream/Output/Audio` node in the PipeWire graph. Requires a live PipeWire
// daemon + ALSA backend, so it is ignored in CI / normal runs. Drives the engine
// directly (not the router) to exercise the real output seam without needing a
// connected Bluetooth speaker.
#[test]
#[ignore = "requires a live PipeWire daemon; run manually with --ignored"]
fn test_play_streams_running_output_node_to_pipewire() {
    use std::{process::Command, time::Duration};

    use blue2th_proto::PlaybackStatus;
    use blue2th_server::audio::{AudioEngine, RodioOutput};

    // Remember the current default sink so we can restore it afterwards.
    let previous_default = Command::new("pactl")
        .args(["get-default-sink"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());

    // Load an in-memory null sink and make it the default playback target.
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

    // Drive the real rodio output.
    let mut engine = AudioEngine::with_output(Box::new(RodioOutput::new()));
    let state = engine.play().expect("play starts the output");
    assert_eq!(state.status, PlaybackStatus::Playing);

    // Let PipeWire register and run the output stream node.
    std::thread::sleep(Duration::from_millis(800));
    let dump = Command::new("pw-dump").output().expect("run pw-dump");
    let graph = String::from_utf8_lossy(&dump.stdout);

    // Tear down before asserting so a failed assertion still restores audio.
    let _ = engine.stop();
    let _ = Command::new("pactl")
        .args(["unload-module", &module_id])
        .status();
    if let Some(prev) = previous_default {
        let _ = Command::new("pactl")
            .args(["set-default-sink", &prev])
            .status();
    }

    assert!(
        graph.contains("Stream/Output/Audio"),
        "no Stream/Output/Audio node found in the PipeWire graph"
    );
    assert!(
        graph.contains("\"state\": \"running\""),
        "no running node found in the PipeWire graph"
    );
}

// Criterion: dropping a speaker from the selection empties it and returns to
// Idle routing. Deselecting used to only update the stored selection, leaving
// the PipeWire graph untouched — so the speaker kept playing. The handler now
// pushes the change into the live graph, which must stay panic-free with no
// speaker selected and no PipeWire around (CI).
#[tokio::test]
async fn test_deselect_last_speaker_empties_the_selection() {
    let app = build_app();
    let request = authorized(Request::builder())
        .method("POST")
        .uri("/devices/AA:BB:CC:DD:EE:FF/deselect")
        .body(Body::empty())
        .expect("build request");

    let response = app.oneshot(request).await.expect("router response");
    assert_eq!(response.status(), StatusCode::OK);

    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let targets: TargetsState = serde_json::from_slice(&bytes).expect("parse TargetsState");
    assert!(
        targets.speakers.is_empty(),
        "no speaker must remain selected, got {:?}",
        targets.speakers
    );
    assert_eq!(targets.routing, RoutingMode::Idle);
}
