// SPDX-License-Identifier: MIT OR Apache-2.0

//! Integration tests for phase 5.2 (Spotify OAuth + Web API transport & SSE):
//! route wiring for `/spotify/auth/{url,callback,status}` and the transport
//! endpoints `/spotify/{play,pause,next,previous}`.
//!
//! Route tests exercise the router in-process via `app().oneshot(...)`, mirroring
//! `tests/transport.rs` and `tests/spotify.rs`. They are expected to FAIL until
//! the routes are wired and the auth backend implemented (red phase). The real
//! OAuth consent, token exchange/refresh, live now-playing over SSE and transport
//! on `blue2th-PC` are a manual network seam and are not covered here.

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use blue2th_proto::{SpotifyAuthState, SpotifyAuthStatus};
use blue2th_server::{auth::AuthStore, spotify_auth::SpotifyAuth};
use tower::ServiceExt; // for `oneshot`

/// The API token these tests pair with (phase 6.4). Held in memory only, so a
/// test run can neither read nor rotate the operator's real one.
const TOKEN: &str = "test-api-token";

/// Add the bearer every guarded route requires (phase 6.4).
fn authorized(builder: axum::http::request::Builder) -> axum::http::request::Builder {
    builder.header("authorization", format!("Bearer {TOKEN}"))
}

/// A router with an unconfigured, off-disk auth driver: no client id, no tokens,
/// and no access to the real user's persisted refresh token (which `app()` would
/// load and which would silently turn these Disconnected cases into Connected).
fn build_app() -> axum::Router {
    blue2th_server::app_with_auth_store(
        SpotifyAuth::with_config(None, "blue2th://spotify-callback".to_string()),
        AuthStore::with_token(TOKEN),
    )
}

// Criterion: `GET /spotify/auth/url` refuses to mint an authorize URL when no
// client id is configured, instead of falling back to a placeholder that Spotify
// would reject much later with an opaque `invalid_client` on its consent page.
// The test process has no BLUE2TH_SPOTIFY_CLIENT_ID; the configured path is
// covered by the `SpotifyAuth::with_config` unit tests, which need no env var.
#[tokio::test]
async fn test_spotify_auth_url_without_client_id_is_unavailable() {
    let request = authorized(Request::builder())
        .uri("/spotify/auth/url")
        .body(Body::empty())
        .expect("build request");

    let response = build_app().oneshot(request).await.expect("router response");
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let message = String::from_utf8_lossy(&bytes);
    assert!(
        message.contains("BLUE2TH_SPOTIFY_CLIENT_ID"),
        "the error must name the missing env var, got {message}"
    );
}

// Criterion: a configured driver mints a URL to the Spotify authorize endpoint
// carrying that client id and a non-empty CSRF `state` (the app opens the URL and
// echoes the state back on callback).
#[tokio::test]
async fn test_spotify_auth_url_with_client_id_carries_it() {
    let mut auth = SpotifyAuth::with_config(
        Some("test-client-id".to_string()),
        "blue2th://spotify-callback".to_string(),
    );
    let (url, state) = auth.authorize_url().expect("configured driver mints a url");

    assert!(
        url.contains("accounts.spotify.com/authorize"),
        "auth url must target the Spotify authorize endpoint, got {url}"
    );
    assert!(
        url.contains("response_type=code"),
        "auth url must request an authorization code, got {url}"
    );
    assert!(
        url.contains("client_id=test-client-id"),
        "auth url must carry the configured client id, got {url}"
    );
    assert!(!state.is_empty(), "authorize_url must carry a CSRF state");
}

// Criterion: a blank client id is as unusable as an absent one and must not be
// sent to Spotify (an empty `BLUE2TH_SPOTIFY_CLIENT_ID=` in a shell wrapper).
#[test]
fn test_spotify_auth_url_with_blank_client_id_is_rejected() {
    let mut auth = SpotifyAuth::with_config(
        Some("   ".to_string()),
        "blue2th://spotify-callback".to_string(),
    );
    assert!(
        auth.authorize_url().is_err(),
        "a blank client id must be treated as unconfigured"
    );
}

// Criterion: `GET /spotify/auth/status` on a fresh router returns Disconnected
// (no tokens are held until the user logs in).
#[tokio::test]
async fn test_spotify_auth_status_on_fresh_server_is_disconnected() {
    let request = authorized(Request::builder())
        .uri("/spotify/auth/status")
        .body(Body::empty())
        .expect("build request");

    let response = build_app().oneshot(request).await.expect("router response");
    assert_eq!(response.status(), StatusCode::OK);

    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let state: SpotifyAuthState =
        serde_json::from_slice(&bytes).expect("parse SpotifyAuthState from /spotify/auth/status");
    assert_eq!(state.status, SpotifyAuthStatus::Disconnected);
}

// Criterion: `POST /spotify/auth/callback` with a missing `code` returns 400.
#[tokio::test]
async fn test_spotify_auth_callback_missing_code_returns_bad_request() {
    let request = authorized(Request::builder())
        .method("POST")
        .uri("/spotify/auth/callback")
        .header("content-type", "application/json")
        // No `code` field: malformed callback body.
        .body(Body::from(r#"{"state":"csrf-abc"}"#))
        .expect("build request");

    let response = build_app().oneshot(request).await.expect("router response");
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "expected 400 for a callback missing `code`, got {}",
        response.status()
    );
}

// Criterion: `POST /spotify/play` while Disconnected returns 409 (no token held,
// no outbound Web API call is attempted).
#[tokio::test]
async fn test_spotify_play_while_disconnected_returns_conflict() {
    let request = authorized(Request::builder())
        .method("POST")
        .uri("/spotify/play")
        .body(Body::empty())
        .expect("build request");

    let response = build_app().oneshot(request).await.expect("router response");
    assert_eq!(
        response.status(),
        StatusCode::CONFLICT,
        "expected 409 for transport while disconnected, got {}",
        response.status()
    );
}

// Criterion: every transport action rejects with 409 while Disconnected.
#[tokio::test]
async fn test_spotify_transport_actions_while_disconnected_return_conflict() {
    for action in ["pause", "next", "previous"] {
        let request = authorized(Request::builder())
            .method("POST")
            .uri(format!("/spotify/{action}"))
            .body(Body::empty())
            .expect("build request");

        let response = build_app().oneshot(request).await.expect("router response");
        assert_eq!(
            response.status(),
            StatusCode::CONFLICT,
            "expected 409 for /spotify/{action} while disconnected, got {}",
            response.status()
        );
    }
}
