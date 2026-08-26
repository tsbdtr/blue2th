// SPDX-License-Identifier: MIT OR Apache-2.0

//! Integration tests for phase 6.4 — `POST /pair`, the one open door.
//!
//! Everything protecting the API rests on this route: the code is armed only
//! briefly, is one-shot and is rate limited. Each of those is asserted here, as
//! is the property that no failure says *why* it failed — telling an expired
//! code from an unknown one would help enumeration.
//!
//! The router is built store-free with an explicit token: a test must never
//! reload or rotate the operator's real one.

use std::time::{Duration, SystemTime};

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use blue2th_proto::{PairRequest, PairResponse};
use blue2th_server::{
    auth::{AuthStore, MAX_PAIRING_ATTEMPTS, PAIRING_TTL},
    spotify_auth::SpotifyAuth,
};
use tower::ServiceExt; // for `oneshot`

/// The API token a successful pairing must hand back.
const TOKEN: &str = "test-api-token";

/// A router around an `AuthStore` the caller prepared (armed code or not).
fn app_with(store: AuthStore) -> axum::Router {
    blue2th_server::app_with_auth_store(
        SpotifyAuth::with_config(None, "blue2th://spotify-callback".to_string()),
        store,
    )
}

/// A router whose store has a code armed at `armed_at`, plus that code.
fn app_with_code_armed_at(armed_at: SystemTime) -> (axum::Router, String) {
    let mut store = AuthStore::with_token(TOKEN);
    let code = store.arm_pairing(armed_at);
    (app_with(store), code)
}

/// `POST /pair` with the given code (and **no** bearer: this is the one route
/// that must open without a token), returning the status and the raw body.
///
/// Fallible rather than asserting: clippy's `allow-expect-in-tests` does not
/// excuse a free helper in an integration-test binary from panicking.
async fn post_pair(app: axum::Router, code: &str) -> Result<(StatusCode, String), String> {
    let body = serde_json::to_string(&PairRequest {
        code: code.to_string(),
    })
    .map_err(|e| format!("serialize PairRequest: {e}"))?;
    post_pair_raw(app, &body).await
}

/// `POST /pair` with a raw body, for the malformed-body case.
async fn post_pair_raw(app: axum::Router, body: &str) -> Result<(StatusCode, String), String> {
    let request = Request::builder()
        .method("POST")
        .uri("/pair")
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
    Ok((status, String::from_utf8_lossy(&bytes).into_owned()))
}

// Criterion: `POST /pair` with a valid armed code returns the token — and needs
// no bearer of its own, since the app has none yet.
#[tokio::test]
async fn test_pair_with_a_valid_code_returns_the_token() {
    let (app, code) = app_with_code_armed_at(SystemTime::now());
    let (status, body) = post_pair(app, &code).await.expect("POST /pair");
    assert_eq!(status, StatusCode::OK, "body: {body}");

    let parsed: PairResponse = serde_json::from_str(&body).expect("parse PairResponse");
    assert_eq!(parsed.token, TOKEN);
}

// Criterion: a wrong code is refused with 401.
#[tokio::test]
async fn test_pair_with_a_wrong_code_is_unauthorised() {
    let (app, _code) = app_with_code_armed_at(SystemTime::now());
    let (status, _) = post_pair(app, "AAAAAA").await.expect("POST /pair");
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// Criterion: an expired code is refused — the pairing window is short by design.
#[tokio::test]
async fn test_pair_with_an_expired_code_is_unauthorised() {
    let armed_at = SystemTime::now() - PAIRING_TTL - Duration::from_secs(1);
    let (app, code) = app_with_code_armed_at(armed_at);
    let (status, _) = post_pair(app, &code).await.expect("POST /pair");
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// Criterion: a code is one-shot — consumed on the first success, refused after.
#[tokio::test]
async fn test_pair_with_an_already_used_code_is_unauthorised() {
    let (app, code) = app_with_code_armed_at(SystemTime::now());
    // Cloned: the router shares its state, so the second call sees the code the
    // first one consumed.
    let (first, _) = post_pair(app.clone(), &code)
        .await
        .expect("first POST /pair");
    assert_eq!(first, StatusCode::OK);

    let (second, _) = post_pair(app, &code).await.expect("second POST /pair");
    assert_eq!(
        second,
        StatusCode::UNAUTHORIZED,
        "a pairing code must work exactly once"
    );
}

// Criterion (non-nominal): pairing while no code is armed is refused; the
// operator's move is to restart the server (or run `--pair`) to mint one.
#[tokio::test]
async fn test_pair_without_an_armed_code_is_unauthorised() {
    let (status, _) = post_pair(app_with(AuthStore::with_token(TOKEN)), "K7M2QX")
        .await
        .expect("POST /pair");
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// Criterion (non-nominal, security): unknown, expired, already-used and
// "no code armed" are **indistinguishable** — same status and same body, or the
// route would help an attacker enumerate.
#[tokio::test]
async fn test_pair_failures_are_indistinguishable() {
    let (armed_app, code) = app_with_code_armed_at(SystemTime::now());
    let (_, unknown) = post_pair(armed_app, "AAAAAA").await.expect("unknown code");

    let (expired_app, expired_code) =
        app_with_code_armed_at(SystemTime::now() - PAIRING_TTL - Duration::from_secs(1));
    let (_, expired) = post_pair(expired_app, &expired_code)
        .await
        .expect("expired code");

    let (used_app, used_code) = app_with_code_armed_at(SystemTime::now());
    let _ = post_pair(used_app.clone(), &used_code)
        .await
        .expect("first use");
    let (_, reused) = post_pair(used_app, &used_code).await.expect("reused code");

    let (_, unarmed) = post_pair(app_with(AuthStore::with_token(TOKEN)), &code)
        .await
        .expect("no code armed");

    assert_eq!(unknown, expired, "expired must read like unknown");
    assert_eq!(unknown, reused, "reused must read like unknown");
    assert_eq!(unknown, unarmed, "unarmed must read like unknown");
}

// Criterion: `POST /pair` is rate limited — `MAX_PAIRING_ATTEMPTS` wrong codes
// invalidate the armed one, so a six-character code cannot be brute forced.
#[tokio::test]
async fn test_pair_is_rate_limited_and_invalidates_the_armed_code() {
    let (app, code) = app_with_code_armed_at(SystemTime::now());
    for attempt in 0..MAX_PAIRING_ATTEMPTS {
        let (status, _) = post_pair(app.clone(), "AAAAAA")
            .await
            .expect("wrong code attempt");
        assert_eq!(status, StatusCode::UNAUTHORIZED, "attempt {attempt}");
    }
    let (status, _) = post_pair(app, &code)
        .await
        .expect("the right code, too late");
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "the armed code must be dead once the attempt cap is reached"
    );
}

// Criterion (non-nominal): a malformed body is a client error, never a 500 and
// never a pairing.
#[tokio::test]
async fn test_pair_with_a_malformed_body_is_a_client_error() {
    for body in ["{ not valid json }", "{}", "\"K7M2QX\""] {
        let (app, _code) = app_with_code_armed_at(SystemTime::now());
        let (status, _) = post_pair_raw(app, body).await.expect("POST /pair");
        assert!(
            status.is_client_error(),
            "{body} must be refused as a client error, got {status}"
        );
    }
}

// Criterion (security): a refused pairing must not leak the API token — that is
// the whole point of the exchange being guarded.
#[tokio::test]
async fn test_a_refused_pairing_never_leaks_the_token() {
    let (app, _code) = app_with_code_armed_at(SystemTime::now());
    let (status, body) = post_pair(app, "AAAAAA").await.expect("POST /pair");
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(
        !body.contains(TOKEN),
        "a refusal must never carry the token, got {body}"
    );
}

// Criterion (usability, phase 6.4 review): the code is upper-case but Android
// capitalises only the first character, so a lower-case tail must still pair —
// otherwise the user spends an attempt with nothing on screen saying why.
#[tokio::test]
async fn test_pair_accepts_a_lower_case_code() {
    let (app, code) = app_with_code_armed_at(SystemTime::now());
    let (status, body) = post_pair(app, &code.to_lowercase())
        .await
        .expect("POST /pair");
    assert_eq!(status, StatusCode::OK, "body: {body}");
}

// …but normalising case must not turn a wrong code into a right one.
#[tokio::test]
async fn test_pair_still_refuses_a_wrong_code_whatever_its_case() {
    let (app, _code) = app_with_code_armed_at(SystemTime::now());
    let (status, _) = post_pair(app, "aaaaaa").await.expect("POST /pair");
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
