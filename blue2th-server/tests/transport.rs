// SPDX-License-Identifier: MIT OR Apache-2.0

//! Integration tests for the phase 3 transport feature: route wiring and the
//! gated PipeWire hardware test.
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

// Criterion (gated hardware, #66): `AudioEngine::play` with the real
// `PipeWireToneOutput` pinned to a null sink produces a running
// `blue2th_tone` stream node, without the default sink being touched. Requires
// a live PipeWire daemon, so it is ignored in CI / normal runs. Drives the
// engine directly (not the router) to exercise the real output seam without
// needing a connected Bluetooth speaker.
#[test]
#[ignore = "requires a live PipeWire daemon; run manually with --ignored"]
fn test_play_streams_running_output_node_to_pipewire() {
    use std::{process::Command, time::Duration};

    use blue2th_proto::PlaybackStatus;
    use blue2th_server::{audio::AudioEngine, tone::PipeWireToneOutput};

    let default_before = Command::new("pactl")
        .args(["get-default-sink"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());

    // An in-memory null sink the tone is pinned to; it is never made default.
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

    let mut engine =
        AudioEngine::with_output(Box::new(PipeWireToneOutput::new("blue2th_test_sink")));
    let state = engine.play().expect("play starts the output");
    assert_eq!(state.status, PlaybackStatus::Playing);

    // Let PipeWire register and run the output stream node.
    std::thread::sleep(Duration::from_millis(800));
    let dump = Command::new("pw-dump").output().expect("run pw-dump");
    let graph = String::from_utf8_lossy(&dump.stdout);
    let default_after = Command::new("pactl")
        .args(["get-default-sink"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());

    // Tear down before asserting so a failed assertion still cleans up.
    let _ = engine.stop();
    let _ = Command::new("pactl")
        .args(["unload-module", &module_id])
        .status();

    assert!(
        graph.contains("\"node.name\": \"blue2th_tone\""),
        "no blue2th_tone node found in the PipeWire graph"
    );
    assert!(
        graph.contains("\"state\": \"running\""),
        "no running node found in the PipeWire graph"
    );
    assert_eq!(default_after, default_before, "the default sink moved");
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
