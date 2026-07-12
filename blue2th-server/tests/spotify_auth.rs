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
use blue2th_proto::{AuthUrlResponse, SpotifyAuthState, SpotifyAuthStatus};
use tower::ServiceExt; // for `oneshot`

fn build_app() -> axum::Router {
    blue2th_server::app()
}

// Criterion: `GET /spotify/auth/url` returns 200 with a URL and a non-empty CSRF
// `state` (the app opens the URL and echoes the state back on callback).
#[tokio::test]
async fn test_spotify_auth_url_returns_url_and_state() {
    let request = Request::builder()
        .uri("/spotify/auth/url")
        .body(Body::empty())
        .expect("build request");

    let response = build_app().oneshot(request).await.expect("router response");
    assert_eq!(response.status(), StatusCode::OK);

    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let parsed: AuthUrlResponse =
        serde_json::from_slice(&bytes).expect("parse AuthUrlResponse from /spotify/auth/url");

    assert!(
        parsed.url.contains("accounts.spotify.com/authorize"),
        "auth url must target the Spotify authorize endpoint, got {}",
        parsed.url
    );
    assert!(
        parsed.url.contains("response_type=code"),
        "auth url must request an authorization code, got {}",
        parsed.url
    );
    assert!(
        !parsed.state.is_empty(),
        "auth url response must carry a CSRF state"
    );
}

// Criterion: `GET /spotify/auth/status` on a fresh router returns Disconnected
// (no tokens are held until the user logs in).
#[tokio::test]
async fn test_spotify_auth_status_on_fresh_server_is_disconnected() {
    let request = Request::builder()
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
    let request = Request::builder()
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
    let request = Request::builder()
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
        let request = Request::builder()
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
