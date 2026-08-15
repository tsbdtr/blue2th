//! Integration tests for phase 5.1 (Spotify source backend): route wiring for
//! `/spotify/start` and `/spotify/status`.
//!
//! Route tests exercise the router in-process via `app().oneshot(...)`, mirroring
//! `tests/transport.rs`. They are expected to FAIL until the routes are wired and
//! the backend implemented (red phase). The real spawn/kill of `librespot`, its
//! appearance in the Spotify app and audio on two speakers are a manual seam and
//! are not covered here.

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use blue2th_proto::{SpotifyState, SpotifyStatus};
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

/// The router under test: store-free, with a known API token.
fn build_app() -> axum::Router {
    blue2th_server::app_with_auth_store(
        SpotifyAuth::with_config(None, "blue2th://spotify-callback".to_string()),
        AuthStore::with_token(TOKEN),
    )
}

// Criterion: `POST /spotify/start` with no target selected returns 400 and does
// not spawn (mirrors `test_play_without_connected_speaker_returns_client_error`,
// but pins 400 exactly so an unwired 404 route still fails the test).
#[tokio::test]
async fn test_spotify_start_without_target_returns_bad_request() {
    let request = authorized(Request::builder())
        .method("POST")
        .uri("/spotify/start")
        .body(Body::empty())
        .expect("build request");

    let response = build_app().oneshot(request).await.expect("router response");
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "expected 400 when no speaker is selected, got {}",
        response.status()
    );
}

// Criterion: `GET /spotify/status` on a fresh router returns `Stopped`.
#[tokio::test]
async fn test_spotify_status_on_fresh_server_is_stopped() {
    let request = authorized(Request::builder())
        .uri("/spotify/status")
        .body(Body::empty())
        .expect("build request");

    let response = build_app().oneshot(request).await.expect("router response");
    assert_eq!(response.status(), StatusCode::OK);

    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let state: SpotifyState =
        serde_json::from_slice(&bytes).expect("parse SpotifyState from /spotify/status");
    assert_eq!(state.status, SpotifyStatus::Stopped);
}
