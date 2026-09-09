// SPDX-License-Identifier: MIT OR Apache-2.0

//! Integration tests for #58: `POST /spotify/volume` and the
//! `spotify_volume_lock` setting on `/config`.
//!
//! Route tests exercise the router in-process via `oneshot`, mirroring
//! `tests/config.rs`. The router is built with an off-disk, unconfigured auth
//! driver, so the Web API is never reached: what these tests pin is the
//! validation and the lock, which must answer **before** any outbound call —
//! 400 for a level above 100 and 409 for a locked backend, whatever the auth
//! state. The actual `PUT /me/player/volume` and the restore after a
//! `librespot` respawn are a manual network seam.

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use blue2th_proto::{ServerConfig, DEFAULT_BACKEND_NAME};
use blue2th_server::{auth::AuthStore, spotify_auth::SpotifyAuth};
use tower::ServiceExt; // for `oneshot`

/// The API token these tests pair with (phase 6.4). Held in memory only, so a
/// test run can neither read nor rotate the operator's real one.
const TOKEN: &str = "test-api-token";

/// Add the bearer every guarded route requires (phase 6.4).
fn authorized(builder: axum::http::request::Builder) -> axum::http::request::Builder {
    builder.header("authorization", format!("Bearer {TOKEN}"))
}

/// A router with an off-disk auth driver and a store-free server name, so no
/// test can read or write the real `~/.local/state/blue2th/`.
fn build_app() -> axum::Router {
    blue2th_server::app_with_auth_store(
        SpotifyAuth::with_config(None, "blue2th://spotify-callback".to_string()),
        AuthStore::with_token(TOKEN),
    )
}

/// `POST` a raw JSON body to `path`, returning the status and body text.
///
/// Fallible rather than asserting: `clippy`'s `allow-expect-in-tests` does not
/// reach a free helper in an integration-test binary.
async fn post_json(
    app: axum::Router,
    path: &str,
    body: &str,
) -> Result<(StatusCode, String), String> {
    let request = authorized(Request::builder())
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .map_err(|e| format!("build request: {e}"))?;
    let response = app
        .oneshot(request)
        .await
        .map_err(|e| format!("router response: {e}"))?;
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .map_err(|e| format!("read body: {e}"))?;
    Ok((status, String::from_utf8_lossy(&bytes).to_string()))
}

/// `GET /config` against `app`, returning the decoded payload.
async fn get_config(app: axum::Router) -> Result<ServerConfig, String> {
    let request = authorized(Request::builder())
        .uri("/config")
        .body(Body::empty())
        .map_err(|e| format!("build request: {e}"))?;
    let response = app
        .oneshot(request)
        .await
        .map_err(|e| format!("router response: {e}"))?;
    if response.status() != StatusCode::OK {
        return Err(format!("GET /config answered {}", response.status()));
    }
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .map_err(|e| format!("read body: {e}"))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("parse ServerConfig: {e}"))
}

// Criterion: `POST /spotify/volume` above 100 → 400. Checked before the auth
// state: a Disconnected backend must still say *why* the body is wrong.
#[tokio::test]
async fn test_post_spotify_volume_above_100_returns_bad_request() {
    let (status, body) = post_json(build_app(), "/spotify/volume", r#"{"percent":101}"#)
        .await
        .expect("POST /spotify/volume");
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "101 % must be refused, got {status}: {body}"
    );
}

// Criterion (non-nominal): a malformed body is a 400, never a panic and never
// a 422 — the route parses leniently like `/config` does.
#[tokio::test]
async fn test_post_spotify_volume_rejects_a_malformed_body() {
    for body in [
        "{ not valid json }",
        "{}",
        r#"{"percent":"loud"}"#,
        r#"{"percent":-1}"#,
    ] {
        let (status, _) = post_json(build_app(), "/spotify/volume", body)
            .await
            .expect("POST /spotify/volume");
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{body} must be refused with 400, got {status}"
        );
    }
}

// Criterion (non-nominal): not authorised → the transport routes' error, a 409
// exactly like `POST /spotify/play` while Disconnected.
#[tokio::test]
async fn test_post_spotify_volume_while_disconnected_returns_conflict() {
    let (status, body) = post_json(build_app(), "/spotify/volume", r#"{"percent":60}"#)
        .await
        .expect("POST /spotify/volume");
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a Disconnected backend must answer like the transport routes, got {status}: {body}"
    );
}

// Criterion (nominal step 5): with the lock on, `POST /spotify/volume` answers
// 409, and the message names the lock so the app can tell it from "not
// connected" — which is a 409 too.
#[tokio::test]
async fn test_post_spotify_volume_with_the_lock_on_returns_conflict_naming_the_lock() {
    // Cloned so both requests hit the same router state (oneshot consumes it).
    let app = build_app();
    let (status, body) = post_json(
        app.clone(),
        "/config",
        r#"{"name":"Salon","spotify_volume_lock":true}"#,
    )
    .await
    .expect("POST /config");
    assert_eq!(status, StatusCode::OK, "body was {body}");

    let (status, body) = post_json(app, "/spotify/volume", r#"{"percent":60}"#)
        .await
        .expect("POST /spotify/volume");
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a locked backend must refuse the level, got {status}: {body}"
    );
    assert!(
        body.to_lowercase().contains("lock"),
        "the refusal must name the lock, got: {body}"
    );
}

// Criterion: `GET /config` reports `spotify_volume_lock`, off by default — a
// fresh backend leaves the Connect level to the user.
#[tokio::test]
async fn test_get_config_on_a_fresh_router_reports_the_spotify_volume_lock_off() {
    let config = get_config(build_app()).await.expect("GET /config");
    assert!(!config.spotify_volume_lock, "the lock defaults to off");
}

// Criterion: `POST /config` applies the lock, echoes what the backend holds,
// and `GET /config` reports it — in both directions.
#[tokio::test]
async fn test_post_config_stores_the_spotify_volume_lock_and_get_reflects_it() {
    // Cloned so both requests hit the same router state (oneshot consumes it).
    let app = build_app();
    let (status, body) = post_json(
        app.clone(),
        "/config",
        r#"{"name":"Salon","spotify_volume_lock":true}"#,
    )
    .await
    .expect("POST /config");
    assert_eq!(status, StatusCode::OK, "body was {body}");
    let echoed: ServerConfig = serde_json::from_str(&body).expect("parse the POST response");
    assert!(
        echoed.spotify_volume_lock,
        "the response must echo what was stored"
    );

    let config = get_config(app.clone()).await.expect("GET /config");
    assert_eq!(config.name, "Salon");
    assert!(config.spotify_volume_lock, "the lock must be stored");
    assert!(
        config.auto_reconnect && config.restore_during_playback,
        "a body without the other flags must leave them on"
    );

    let (status, _) = post_json(
        app.clone(),
        "/config",
        r#"{"name":"Salon","spotify_volume_lock":false}"#,
    )
    .await
    .expect("POST /config");
    assert_eq!(status, StatusCode::OK);
    assert!(
        !get_config(app)
            .await
            .expect("GET /config")
            .spotify_volume_lock,
        "turning the lock back off must be stored too"
    );
}

// Criterion (non-nominal: old app): a `POST /config` body without the field
// leaves the lock **off** — an older client must not pin the level by omission,
// and must not turn a stored lock on either.
#[tokio::test]
async fn test_post_config_without_the_lock_leaves_it_unchanged() {
    // The app re-pushes its whole config on every activation; a client that
    // does not know the field must not switch the guard off each time.
    let app = build_app();
    let (status, _) = post_json(
        app.clone(),
        "/config",
        r#"{"name":"Salon","spotify_volume_lock":true}"#,
    )
    .await
    .expect("POST /config with the lock");
    assert_eq!(status, StatusCode::OK);

    let (status, body) = post_json(app.clone(), "/config", r#"{"name":"Salon"}"#)
        .await
        .expect("POST /config");
    assert_eq!(
        status,
        StatusCode::OK,
        "an older client must keep working, got {status}: {body}"
    );

    let config = get_config(app).await.expect("GET /config");
    assert!(
        config.spotify_volume_lock,
        "a push without the field must leave the lock as it was"
    );
}

// Criterion: a rejected name changes nothing at all — the lock carried by a
// refused body must not be applied either.
#[tokio::test]
async fn test_rejected_config_body_does_not_apply_the_spotify_volume_lock() {
    let app = build_app();
    let (status, _) = post_json(
        app.clone(),
        "/config",
        r#"{"name":"2salon","spotify_volume_lock":true}"#,
    )
    .await
    .expect("POST /config");
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let config = get_config(app).await.expect("GET /config");
    assert_eq!(config.name, DEFAULT_BACKEND_NAME);
    assert!(
        !config.spotify_volume_lock,
        "a refused body must leave the lock untouched"
    );
}
