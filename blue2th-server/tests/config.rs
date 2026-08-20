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

/// `GET /config` against `app`, returning the decoded payload.
///
/// Fallible rather than asserting: `clippy`'s `allow-expect-in-tests` does not
/// reach a free helper in an integration-test binary, and the `#[tokio::test]`
/// functions are the right place to assert anyway — a decode failure then says
/// which step broke.
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

/// `POST /config` with a raw JSON body, returning the status and body text.
async fn post_config(app: axum::Router, body: &str) -> Result<(StatusCode, String), String> {
    let request = authorized(Request::builder())
        .method("POST")
        .uri("/config")
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

// Criterion: `GET /config` returns the stored name; a fresh server returns the
// default `blue2th-PC`.
#[tokio::test]
async fn test_get_config_on_a_fresh_router_returns_the_default_name() {
    let config = get_config(build_app()).await.expect("GET /config");
    assert_eq!(config.name, DEFAULT_BACKEND_NAME);
}

// Criterion: `POST /config` rejects a blank name with 400 (the server
// re-validates rather than trusting the client).
#[tokio::test]
async fn test_post_config_rejects_a_blank_name_with_bad_request() {
    let (status, _) = post_config(build_app(), r#"{"name":"   "}"#)
        .await
        .expect("POST /config");
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
        let (status, message) = post_config(build_app(), &body).await.expect("POST /config");
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
    let too_long: String = "a".repeat(MAX_BACKEND_NAME_LEN + 1);
    let body = serde_json::json!({ "name": too_long }).to_string();
    let (status, _) = post_config(build_app(), &body).await.expect("POST /config");
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// Criterion: `POST /config` with a valid name returns 200 and stores a *trimmed*
// value; `GET /config` reflects it.
#[tokio::test]
async fn test_post_config_stores_a_trimmed_valid_name_and_get_reflects_it() {
    // Cloned so both requests hit the same router state (oneshot consumes it).
    let app = build_app();
    let (status, _) = post_config(app.clone(), r#"{"name":"  Salon  "}"#)
        .await
        .expect("POST /config");
    assert_eq!(status, StatusCode::OK, "a valid name must be accepted");

    let config = get_config(app).await.expect("GET /config");
    assert_eq!(config.name, "Salon");
}

// Criterion: the configured name becomes the Spotify Connect device name — the
// `/spotify/status` payload follows `POST /config`, which is what the Web API
// device lookup has to match on.
#[tokio::test]
async fn test_configured_name_becomes_the_spotify_connect_device_name() {
    let app = build_app();
    let (status, _) = post_config(app.clone(), r#"{"name":"Salon"}"#)
        .await
        .expect("POST /config");
    assert_eq!(status, StatusCode::OK);

    let request = authorized(Request::builder())
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
    let (status, _) = post_config(build_app(), "{ not valid json }")
        .await
        .expect("POST /config");
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "expected 400 for a malformed body, got {status}"
    );
}

// ---- phase 6.3: the restore-during-playback setting ----

// Criterion: `GET /config` reports the flag, with its default — a fresh server
// restores a returning speaker even mid-playback until told otherwise.
#[tokio::test]
async fn test_get_config_on_a_fresh_router_reports_restoration_enabled() {
    let config = get_config(build_app()).await.expect("GET /config");
    assert!(config.restore_during_playback, "the setting defaults to on");
}

// Criterion: `POST /config` stores the flag and `GET /config` reflects it, in
// both directions.
#[tokio::test]
async fn test_post_config_stores_the_restore_flag_and_get_reflects_it() {
    // Cloned so both requests hit the same router state (oneshot consumes it).
    let app = build_app();
    let (status, body) = post_config(
        app.clone(),
        r#"{"name":"Salon","restore_during_playback":false}"#,
    )
    .await
    .expect("POST /config");
    assert_eq!(status, StatusCode::OK, "body was {body}");
    let echoed: ServerConfig = serde_json::from_str(&body).expect("parse the POST response");
    assert!(
        !echoed.restore_during_playback,
        "the response must echo what was stored"
    );

    let config = get_config(app.clone()).await.expect("GET /config");
    assert_eq!(config.name, "Salon");
    assert!(!config.restore_during_playback, "the flag must be stored");

    let (status, _) = post_config(
        app.clone(),
        r#"{"name":"Salon","restore_during_playback":true}"#,
    )
    .await
    .expect("POST /config");
    assert_eq!(status, StatusCode::OK);
    assert!(
        get_config(app)
            .await
            .expect("GET /config")
            .restore_during_playback,
        "turning the setting back on must be stored too"
    );
}

// Criterion (non-nominal: old client, new server): a body carrying only a name
// is still accepted, and the flag falls back to its default rather than
// silently disabling restoration.
#[tokio::test]
async fn test_post_config_with_only_a_name_still_succeeds() {
    let app = build_app();
    let (status, body) = post_config(app.clone(), r#"{"name":"Salon"}"#)
        .await
        .expect("POST /config");
    assert_eq!(
        status,
        StatusCode::OK,
        "a phase 6.2 client must keep working, got {status}: {body}"
    );

    let config = get_config(app).await.expect("GET /config");
    assert_eq!(config.name, "Salon");
    assert!(
        config.restore_during_playback,
        "a name-only push must leave restoration on (the default)"
    );
}

// Criterion: a rejected name changes nothing at all — the flag carried by a
// refused body must not be applied either.
#[tokio::test]
async fn test_rejected_config_body_does_not_apply_the_restore_flag() {
    let app = build_app();
    let (status, _) = post_config(
        app.clone(),
        r#"{"name":"2salon","restore_during_playback":false}"#,
    )
    .await
    .expect("POST /config");
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let config = get_config(app).await.expect("GET /config");
    assert_eq!(config.name, DEFAULT_BACKEND_NAME);
    assert!(
        config.restore_during_playback,
        "a refused body must leave the setting untouched"
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
        let request = authorized(Request::builder())
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

// ---- phase 6.5: the auto-reconnect setting ----

// Criterion: `GET /config` reports `auto_reconnect`, with its default — a fresh
// backend dials a remembered speaker back until told otherwise.
#[tokio::test]
async fn test_get_config_on_a_fresh_router_reports_auto_reconnect_enabled() {
    let config = get_config(build_app()).await.expect("GET /config");
    assert!(config.auto_reconnect, "the setting defaults to on");
}

// Criterion: `POST /config` accepts `auto_reconnect`, stores it, and echoes back
// what the backend actually holds; `GET /config` reports it.
#[tokio::test]
async fn test_post_config_stores_the_auto_reconnect_flag_and_get_reflects_it() {
    // Cloned so both requests hit the same router state (oneshot consumes it).
    let app = build_app();
    let (status, body) = post_config(app.clone(), r#"{"name":"Salon","auto_reconnect":false}"#)
        .await
        .expect("POST /config");
    assert_eq!(status, StatusCode::OK, "body was {body}");
    let echoed: ServerConfig = serde_json::from_str(&body).expect("parse the POST response");
    assert!(
        !echoed.auto_reconnect,
        "the response must echo what was stored"
    );

    let config = get_config(app.clone()).await.expect("GET /config");
    assert_eq!(config.name, "Salon");
    assert!(!config.auto_reconnect, "the flag must be stored");
    assert!(
        config.restore_during_playback,
        "a body without the phase 6.3 flag must leave it on"
    );

    let (status, _) = post_config(app.clone(), r#"{"name":"Salon","auto_reconnect":true}"#)
        .await
        .expect("POST /config");
    assert_eq!(status, StatusCode::OK);
    assert!(
        get_config(app).await.expect("GET /config").auto_reconnect,
        "turning the setting back on must be stored too"
    );
}

// Criterion (non-nominal: a phase 6.2/6.3 client pushes `/config`): a body with
// no `auto_reconnect` field leaves the feature **on**, never silently disabled.
#[tokio::test]
async fn test_post_config_without_auto_reconnect_leaves_it_on() {
    let app = build_app();
    let (status, body) = post_config(
        app.clone(),
        r#"{"name":"Salon","restore_during_playback":false}"#,
    )
    .await
    .expect("POST /config");
    assert_eq!(
        status,
        StatusCode::OK,
        "a phase 6.3 client must keep working, got {status}: {body}"
    );

    let config = get_config(app).await.expect("GET /config");
    assert!(
        config.auto_reconnect,
        "an older client must not disable auto-reconnect by omission"
    );
}

// Criterion: a rejected name changes nothing at all — the auto-reconnect flag
// carried by a refused body must not be applied either.
#[tokio::test]
async fn test_rejected_config_body_does_not_apply_the_auto_reconnect_flag() {
    let app = build_app();
    let (status, _) = post_config(app.clone(), r#"{"name":"2salon","auto_reconnect":false}"#)
        .await
        .expect("POST /config");
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let config = get_config(app).await.expect("GET /config");
    assert_eq!(config.name, DEFAULT_BACKEND_NAME);
    assert!(
        config.auto_reconnect,
        "a refused body must leave the setting untouched"
    );
}
