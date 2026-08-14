//! Integration tests for phase 6.2 (`GET`/`POST /config`): the backend's own
//! name, pushed by the app and re-validated server-side.
//!
//! Route tests exercise the router in-process via `oneshot`, mirroring
//! `tests/spotify_auth.rs`. The router is built with an explicit, off-disk auth
//! driver so the tests can neither read nor clobber the real user's state — and
//! so the name store stays store-free too.
//!
//! The visible half of the rename (the Connect device reappearing under the new
//! name in a Spotify client, `librespot` actually respawning) is a manual seam.

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use blue2th_proto::{ServerConfig, DEFAULT_BACKEND_NAME, MAX_BACKEND_NAME_LEN};
use blue2th_server::spotify_auth::SpotifyAuth;
use tower::ServiceExt; // for `oneshot`

/// A router with an off-disk auth driver and a store-free server name, so no
/// test can read or write the real `~/.local/state/blue2th/`.
fn build_app() -> axum::Router {
    blue2th_server::app_with_auth(SpotifyAuth::with_config(
        None,
        "blue2th://spotify-callback".to_string(),
    ))
}

/// `GET /config` against `app`, returning the decoded payload.
async fn get_config(app: axum::Router) -> ServerConfig {
    let request = Request::builder()
        .uri("/config")
        .body(Body::empty())
        .expect("build request");
    let response = app.oneshot(request).await.expect("router response");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    serde_json::from_slice(&bytes).expect("parse ServerConfig from /config")
}

/// `POST /config` with a raw JSON body, returning the status and body text.
async fn post_config(app: axum::Router, body: &str) -> (StatusCode, String) {
    let request = Request::builder()
        .method("POST")
        .uri("/config")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("build request");
    let response = app.oneshot(request).await.expect("router response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    (status, String::from_utf8_lossy(&bytes).to_string())
}

// Criterion: `GET /config` returns the stored name; a fresh server returns the
// default `blue2th-PC`.
#[tokio::test]
async fn test_get_config_on_a_fresh_router_returns_the_default_name() {
    let config = get_config(build_app()).await;
    assert_eq!(config.name, DEFAULT_BACKEND_NAME);
}

// Criterion: `POST /config` rejects a blank name with 400 (the server
// re-validates rather than trusting the client).
#[tokio::test]
async fn test_post_config_rejects_a_blank_name_with_bad_request() {
    let (status, _) = post_config(build_app(), r#"{"name":"   "}"#).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a blank name must be refused, got {status}"
    );
}

// Criterion: `POST /config` rejects an invalid name with 400 and a message
// naming the rule — `POST /config` is unauthenticated on the LAN, so the server
// cannot assume a well-behaved client.
#[tokio::test]
async fn test_post_config_rejects_an_invalid_name_with_a_message_naming_the_rule() {
    for name in ["2salon", "-salon", "salon tv", "s\u{e9}jour", "salon!"] {
        let body = serde_json::json!({ "name": name }).to_string();
        let (status, message) = post_config(build_app(), &body).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{name} must be refused, got {status}"
        );
        assert!(
            !message.trim().is_empty(),
            "{name} must be refused with a message naming the rule"
        );
    }
}

// Criterion: the length cap is enforced server-side too.
#[tokio::test]
async fn test_post_config_rejects_a_name_longer_than_the_cap() {
    let too_long: String = std::iter::repeat('a')
        .take(MAX_BACKEND_NAME_LEN + 1)
        .collect();
    let body = serde_json::json!({ "name": too_long }).to_string();
    let (status, _) = post_config(build_app(), &body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// Criterion: `POST /config` with a valid name returns 200 and stores a *trimmed*
// value; `GET /config` reflects it.
#[tokio::test]
async fn test_post_config_stores_a_trimmed_valid_name_and_get_reflects_it() {
    // Cloned so both requests hit the same router state (oneshot consumes it).
    let app = build_app();
    let (status, _) = post_config(app.clone(), r#"{"name":"  Salon  "}"#).await;
    assert_eq!(status, StatusCode::OK, "a valid name must be accepted");

    let config = get_config(app).await;
    assert_eq!(config.name, "Salon");
}

// Criterion: the configured name becomes the Spotify Connect device name — the
// `/spotify/status` payload follows `POST /config`, which is what the Web API
// device lookup has to match on.
#[tokio::test]
async fn test_configured_name_becomes_the_spotify_connect_device_name() {
    let app = build_app();
    let (status, _) = post_config(app.clone(), r#"{"name":"Salon"}"#).await;
    assert_eq!(status, StatusCode::OK);

    let request = Request::builder()
        .uri("/spotify/status")
        .body(Body::empty())
        .expect("build request");
    let response = app.oneshot(request).await.expect("router response");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let state: blue2th_proto::SpotifyState =
        serde_json::from_slice(&bytes).expect("parse SpotifyState");
    assert_eq!(
        state.device_name, "Salon",
        "librespot must advertise the configured name, not the constant"
    );
}

// Criterion: `POST /config` with a malformed body is a 400, never a panic. The
// status is pinned exactly so an unwired route (404) still fails this test.
#[tokio::test]
async fn test_post_config_rejects_a_malformed_body() {
    let (status, _) = post_config(build_app(), "{ not valid json }").await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "expected 400 for a malformed body, got {status}"
    );
}

// Criterion: no regression from threading the name through `transport()` — a
// transport call while Disconnected still returns 409 (and never reaches the
// network).
#[tokio::test]
async fn test_transport_while_disconnected_still_returns_conflict() {
    for path in [
        "/spotify/play",
        "/spotify/pause",
        "/spotify/next",
        "/spotify/previous",
    ] {
        let request = Request::builder()
            .method("POST")
            .uri(path)
            .body(Body::empty())
            .expect("build request");
        let response = build_app().oneshot(request).await.expect("router response");
        assert_eq!(
            response.status(),
            StatusCode::CONFLICT,
            "{path} must still answer 409 while Disconnected"
        );
    }
}
