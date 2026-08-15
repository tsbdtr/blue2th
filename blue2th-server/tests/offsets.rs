//! Integration tests for phase 6.1 (remember each speaker's sync offset across
//! restarts): route-level non-regression around `POST /devices/{addr}/offset` and
//! `GET /targets` once `SpeakerTargets` gained a remembered-offsets store.
//!
//! The router is always built with `app_with_auth`, the **store-free** entry
//! point: a test run must never read or clobber the real user's
//! `~/.local/state/blue2th/offsets.json` (the lesson from the Spotify token
//! store). The persistence itself is unit-tested in `src/targets.rs` against a
//! temp path; selecting a *connected* speaker needs BlueZ and stays manual.

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use blue2th_proto::{RoutingMode, TargetsState};
use blue2th_server::{auth::AuthStore, spotify_auth::SpotifyAuth};
use tower::ServiceExt; // for `oneshot`

const ADDR: &str = "AA:BB:CC:DD:EE:FF";

/// The API token these tests pair with (phase 6.4). Held in memory only, so a
/// test run can neither read nor rotate the operator's real one.
const TOKEN: &str = "test-api-token";

/// Add the bearer every guarded route requires (phase 6.4).
fn authorized(builder: axum::http::request::Builder) -> axum::http::request::Builder {
    builder.header("authorization", format!("Bearer {TOKEN}"))
}

/// A router with an unconfigured, off-disk auth driver and a store-free
/// selection: no hardware, no filesystem state shared with the developer's box.
fn build_app() -> axum::Router {
    blue2th_server::app_with_auth_store(
        SpotifyAuth::with_config(None, "blue2th://spotify-callback".to_string()),
        AuthStore::with_token(TOKEN),
    )
}

/// Read a `TargetsState` out of a route response body. Errors are propagated
/// rather than asserted here: clippy's `allow-expect-in-tests` covers `#[test]`
/// functions, not a free helper, and the call sites read better anyway.
async fn targets_state(response: axum::response::Response) -> Result<TargetsState, String> {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .map_err(|e| format!("read body: {e}"))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("parse TargetsState: {e}"))
}

// Criterion: the refactor keeps the offset route answering 200 with the current
// `TargetsState` — even when the store cannot be used, a slider drag is never an
// error response.
#[tokio::test]
async fn test_offset_route_still_returns_targets_state() {
    let request = authorized(Request::builder())
        .method("POST")
        .uri(format!("/devices/{ADDR}/offset"))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"offset_ms":750}"#))
        .expect("build request");

    let response = build_app().oneshot(request).await.expect("router response");
    assert_eq!(response.status(), StatusCode::OK);
    let _state = targets_state(response).await.expect("decode TargetsState");
}

// Criterion: remembered offsets for unselected speakers never appear in
// `TargetsState` — setting an offset on a speaker that was never selected (no
// BlueZ in CI, so nothing is connected) leaves the reported selection empty.
#[tokio::test]
async fn test_offset_route_on_unselected_speaker_reports_empty_selection() {
    let app = build_app();

    let offset_request = authorized(Request::builder())
        .method("POST")
        .uri(format!("/devices/{ADDR}/offset"))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"offset_ms":500}"#))
        .expect("build request");
    let response = app
        .clone()
        .oneshot(offset_request)
        .await
        .expect("router response");
    assert_eq!(response.status(), StatusCode::OK);

    let targets_request = authorized(Request::builder())
        .uri("/targets")
        .body(Body::empty())
        .expect("build request");
    let response = app.oneshot(targets_request).await.expect("router response");
    assert_eq!(response.status(), StatusCode::OK);

    let state = targets_state(response).await.expect("decode TargetsState");
    assert!(
        state.speakers.is_empty(),
        "an unselected speaker must not surface through a remembered offset, got {:?}",
        state.speakers
    );
    assert_eq!(state.routing, RoutingMode::Idle);
}
