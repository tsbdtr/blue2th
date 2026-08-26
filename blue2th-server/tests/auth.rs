// SPDX-License-Identifier: MIT OR Apache-2.0

//! Integration tests for phase 6.4 — the authenticated LAN API.
//!
//! The guard is asserted **route by route, driven from `blue2th_server::ROUTES`**
//! rather than on a hand-picked sample: a route added later without the guard
//! must fail a test instead of quietly shipping an open door. A second test pins
//! today's routes into that table, so the table cannot be emptied to make the
//! first one pass vacuously.
//!
//! The router is always built store-free with an explicit token: after this
//! phase no test may call `app()`, which reloads — and, on a malformed store,
//! rotates — the operator's real API token, unpairing their phone.

use std::time::Duration;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use blue2th_proto::HealthStatus;
use blue2th_server::{auth::AuthStore, spotify_auth::SpotifyAuth, RouteSpec, ROUTES};
use tower::ServiceExt; // for `oneshot`

/// The API token these tests pair with (in memory only).
const TOKEN: &str = "test-api-token";

/// A sample address to fill the `{addr}` path parameter with.
const ADDR: &str = "AA:BB:CC:DD:EE:FF";

/// How long a single route may take before the test gives up on it. The
/// hardware routes (`/devices`, `/scan`) reach BlueZ once the guard lets them
/// through, and CI has none.
const ROUTE_TIMEOUT: Duration = Duration::from_secs(5);

/// A store-free router with a known API token and an unconfigured Spotify auth
/// driver: no hardware, no filesystem state shared with the operator's box.
fn build_app() -> axum::Router {
    blue2th_server::app_with_auth_store(
        SpotifyAuth::with_config(None, "blue2th://spotify-callback".to_string()),
        AuthStore::with_token(TOKEN),
    )
}

/// The concrete path a route spec is exercised on.
fn concrete_path(spec: &RouteSpec) -> String {
    spec.path.replace("{addr}", ADDR)
}

/// Send one request through a fresh router. `None` means the route never
/// answered within [`ROUTE_TIMEOUT`] — which is certainly not a 401, and is how
/// the assertions below read it.
async fn status_of(spec: &RouteSpec, bearer: Option<&str>) -> Option<StatusCode> {
    let mut builder = Request::builder()
        .method(spec.method)
        .uri(concrete_path(spec));
    if let Some(token) = bearer {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let request = builder.body(Body::empty()).ok()?;
    tokio::time::timeout(ROUTE_TIMEOUT, build_app().oneshot(request))
        .await
        .ok()?
        .ok()
        .map(|response| response.status())
}

// Criterion: every route except `/health` and `POST /pair` answers 401 without a
// valid bearer — asserted route by route from the table the router is built
// from, not on a sample.
#[tokio::test]
async fn test_every_guarded_route_rejects_a_missing_bearer() {
    assert!(
        !ROUTES.is_empty(),
        "an empty route table would pass every guard test without checking a thing"
    );
    for spec in ROUTES.iter().filter(|s| !s.public) {
        assert_eq!(
            status_of(spec, None).await,
            Some(StatusCode::UNAUTHORIZED),
            "{} {} must be refused without a bearer",
            spec.method,
            spec.path
        );
    }
}

// Criterion (non-nominal): a wrong token reads exactly like no token at all —
// 401 on every guarded route, never a partial success.
#[tokio::test]
async fn test_every_guarded_route_rejects_a_wrong_bearer() {
    assert!(
        !ROUTES.is_empty(),
        "an empty route table would pass every guard test without checking a thing"
    );
    for spec in ROUTES.iter().filter(|s| !s.public) {
        assert_eq!(
            status_of(spec, Some("not-the-token")).await,
            Some(StatusCode::UNAUTHORIZED),
            "{} {} must be refused with a wrong bearer",
            spec.method,
            spec.path
        );
    }
}

// Criterion: with a valid bearer every route answers its normal status — the
// guard must not have broken the API, and a table entry that 404s would mean the
// table and the router have drifted apart.
#[tokio::test]
async fn test_every_guarded_route_accepts_a_valid_bearer() {
    assert!(
        !ROUTES.is_empty(),
        "an empty route table would pass every guard test without checking a thing"
    );
    for spec in ROUTES.iter().filter(|s| !s.public) {
        let status = status_of(spec, Some(TOKEN)).await;
        assert_ne!(
            status,
            Some(StatusCode::UNAUTHORIZED),
            "{} {} must be allowed with the right bearer",
            spec.method,
            spec.path
        );
        assert_ne!(
            status,
            Some(StatusCode::NOT_FOUND),
            "{} {} is in ROUTES but the router does not serve it",
            spec.method,
            spec.path
        );
        assert_ne!(
            status,
            Some(StatusCode::METHOD_NOT_ALLOWED),
            "{} {} is in ROUTES with a method the router does not accept",
            spec.method,
            spec.path
        );
    }
}

// Criterion: the two public routes answer without a bearer — `/health` so the
// app can tell "not paired" from "unreachable", `POST /pair` because it is the
// one door that must open without a token.
#[tokio::test]
async fn test_public_routes_answer_without_a_bearer() {
    assert!(
        !ROUTES.is_empty(),
        "an empty route table would pass every guard test without checking a thing"
    );
    for spec in ROUTES.iter().filter(|s| s.public) {
        let status = status_of(spec, None).await;
        assert_ne!(
            status,
            Some(StatusCode::UNAUTHORIZED),
            "{} {} is public and must answer without a bearer",
            spec.method,
            spec.path
        );
        assert_ne!(
            status,
            Some(StatusCode::NOT_FOUND),
            "{} {} is in ROUTES but the router does not serve it",
            spec.method,
            spec.path
        );
    }
}

// Criterion: only `/health` and `POST /pair` are public. Any other public entry
// is an open door, whoever added it and for whatever reason.
#[test]
fn test_only_health_and_pair_are_public() {
    let public: Vec<(&str, &str)> = ROUTES
        .iter()
        .filter(|s| s.public)
        .map(|s| (s.method, s.path))
        .collect();
    assert_eq!(public, vec![("GET", "/health"), ("POST", "/pair")]);
}

// Criterion: the route table is the single source of truth — it must name every
// route the backend serves today, or the guard tests above would pass while
// leaving real routes unchecked (an empty table passes them vacuously).
#[test]
fn test_route_table_lists_every_route_the_backend_serves() {
    let expected: &[(&str, &str, bool)] = &[
        ("GET", "/health", true),
        ("POST", "/pair", true),
        ("GET", "/adapters", false),
        ("GET", "/devices", false),
        ("POST", "/devices/{addr}/connect", false),
        ("POST", "/devices/{addr}/disconnect", false),
        ("POST", "/devices/{addr}/select", false),
        ("POST", "/devices/{addr}/deselect", false),
        ("POST", "/devices/{addr}/offset", false),
        ("GET", "/targets", false),
        ("GET", "/scan", false),
        ("POST", "/play", false),
        ("POST", "/pause", false),
        ("POST", "/stop", false),
        ("POST", "/volume", false),
        ("GET", "/playback", false),
        ("POST", "/spotify/start", false),
        ("POST", "/spotify/stop", false),
        ("GET", "/spotify/status", false),
        ("GET", "/spotify/auth/url", false),
        ("POST", "/spotify/auth/callback", false),
        ("GET", "/spotify/auth/status", false),
        ("POST", "/spotify/play", false),
        ("POST", "/spotify/pause", false),
        ("POST", "/spotify/next", false),
        ("POST", "/spotify/previous", false),
        ("GET", "/spotify/now-playing", false),
        ("POST", "/client/presence", false),
        ("GET", "/config", false),
        ("POST", "/config", false),
    ];

    for (method, path, public) in expected {
        let listed = ROUTES
            .iter()
            .find(|s| s.method == *method && s.path == *path);
        assert_eq!(
            listed.map(|s| s.public),
            Some(*public),
            "{method} {path} must be listed in ROUTES with public={public}"
        );
    }
    assert_eq!(
        ROUTES.len(),
        expected.len(),
        "ROUTES holds an entry this test does not know about: {ROUTES:?}"
    );
}

// Criterion: `/health` answers 200 without a bearer and reports
// `auth_required` — this is exactly why it stays open: the app can then say
// "not paired" instead of "offline".
#[tokio::test]
async fn test_health_without_a_bearer_reports_auth_required() {
    let request = Request::builder()
        .uri("/health")
        .body(Body::empty())
        .expect("build request");
    let response = build_app().oneshot(request).await.expect("router response");
    assert_eq!(response.status(), StatusCode::OK);

    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let parsed: HealthStatus = serde_json::from_slice(&bytes).expect("parse HealthStatus");
    assert!(
        parsed.auth_required,
        "an authenticated backend must say so on its open probe"
    );
}

// Criterion (non-nominal): `/health` answers even with a stale or wrong token,
// so a revoked app reads "not paired", never "unreachable".
#[tokio::test]
async fn test_health_answers_with_a_wrong_bearer() {
    let request = Request::builder()
        .uri("/health")
        .header("authorization", "Bearer stale-token")
        .body(Body::empty())
        .expect("build request");
    let response = build_app().oneshot(request).await.expect("router response");
    assert_eq!(response.status(), StatusCode::OK);
}

// Criterion: `/health` must not leak anything beyond the version and
// `auth_required` — it is the only payload an unauthenticated caller can read.
#[tokio::test]
async fn test_health_payload_leaks_nothing_beyond_status_version_and_auth() {
    let request = Request::builder()
        .uri("/health")
        .body(Body::empty())
        .expect("build request");
    let response = build_app().oneshot(request).await.expect("router response");
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let payload: serde_json::Value = serde_json::from_slice(&bytes).expect("parse the payload");
    let object = payload.as_object().expect("the payload must be an object");
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, vec!["auth_required", "status", "version"]);
}

// Criterion: `CorsLayer::permissive()` is gone — it answered the preflight for
// any web page the user opened, which was the most realistic attack vector of
// the whole feature. The needle is assembled at compile time so this test is not
// itself an occurrence.
#[test]
fn test_permissive_cors_is_gone_from_the_server_sources() {
    let needle = concat!("CorsLayer", "::permissive");
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let entries = std::fs::read_dir(&src)
        .map_err(|e| format!("read {src:?}: {e}"))
        .expect("the server sources must be readable");

    let mut offenders = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        if std::fs::read_to_string(&path)
            .unwrap_or_default()
            .contains(needle)
        {
            offenders.push(path);
        }
    }
    assert!(
        offenders.is_empty(),
        "{needle} must not appear in the server, found in {offenders:?}"
    );
}

// Criterion: no permissive CORS in behaviour either — a cross-origin request
// must not come back blessed with `access-control-allow-origin`, or a hostile
// page could drive the backend from the user's own browser.
#[tokio::test]
async fn test_no_cors_header_is_granted_to_a_foreign_origin() {
    let request = Request::builder()
        .uri("/health")
        .header("origin", "https://evil.example")
        .body(Body::empty())
        .expect("build request");
    let response = build_app().oneshot(request).await.expect("router response");
    assert!(
        response
            .headers()
            .get("access-control-allow-origin")
            .is_none(),
        "a foreign origin must not be allowed, got {:?}",
        response.headers()
    );
}

// Criterion: the preflight a hostile page sends before a cross-origin POST is
// not answered permissively either.
#[tokio::test]
async fn test_cors_preflight_is_not_answered_permissively() {
    let request = Request::builder()
        .method("OPTIONS")
        .uri("/play")
        .header("origin", "https://evil.example")
        .header("access-control-request-method", "POST")
        .body(Body::empty())
        .expect("build request");
    let response = build_app().oneshot(request).await.expect("router response");
    assert!(
        response
            .headers()
            .get("access-control-allow-origin")
            .is_none(),
        "the preflight must not bless the origin, got {:?}",
        response.headers()
    );
}
