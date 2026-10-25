// SPDX-License-Identifier: MIT OR Apache-2.0

use axum::{
    body::Body,
    http::{header, Request, StatusCode},
};
use blue2th_proto::HealthStatus;
use tower::ServiceExt as _; // for `oneshot`

use super::*;

/// One request through a fresh plain router.
async fn plain(request: Request<Body>) -> axum::response::Response {
    plain_router()
        .oneshot(request)
        .await
        .expect("the router is infallible")
}

/// A request with `method`, `uri` and the given headers, and no body.
fn request(method: &str, uri: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(Body::empty()).expect("a valid request")
}

/// The `Location` header of `response`, if any.
fn location(response: &axum::response::Response) -> Option<String> {
    response
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

// ---- Limits ----

// Criterion (listener): the first-byte and handshake timeouts are 10 s in
// production — each a parameter, so a test can use a short one.
#[test]
fn test_limits_in_production_are_ten_seconds_each() {
    assert_eq!(
        Limits::PRODUCTION,
        Limits {
            first_byte: Duration::from_secs(10),
            handshake: Duration::from_secs(10),
        }
    );
}

// ---- plain_http_choice ----

// Criterion (plain mode): `Some("1")` in a debug build enables, with no
// warning.
#[test]
fn test_plain_http_choice_one_in_debug_enables_without_a_warning() {
    assert_eq!(
        plain_http_choice(Some("1"), true),
        PlainChoice {
            enabled: true,
            warning: None,
        }
    );
}

// Criterion (plain mode), guard: `Some("1")` in a release build does not
// enable, and warns that it was ignored. Near-miss: the very value that
// enables in debug; only the build check rejects it.
#[test]
fn test_plain_http_choice_one_in_release_is_ignored_with_a_warning() {
    assert!(
        plain_http_choice(Some("1"), true).enabled,
        "the near-miss enables in debug"
    );
    let choice = plain_http_choice(Some("1"), false);
    assert!(!choice.enabled, "a release build always serves TLS");
    let warning = choice.warning.unwrap_or_default();
    assert!(
        warning.contains(PLAIN_HTTP_ENV) && warning.contains("ignored"),
        "the warning names the variable and says it was ignored, got {warning:?}"
    );
}

// Criterion (plain mode), guard: any other non-empty value does not enable,
// with a warning, in both builds. Near-miss: `true`, in debug.
#[test]
fn test_plain_http_choice_any_other_value_is_ignored_with_a_warning() {
    for value in ["true", "0", "yes", "2", " 1"] {
        for debug in [true, false] {
            let choice = plain_http_choice(Some(value), debug);
            assert!(!choice.enabled, "{value:?} (debug {debug}) must not enable");
            let warning = choice.warning.unwrap_or_default();
            assert!(
                warning.contains(PLAIN_HTTP_ENV),
                "{value:?} (debug {debug}): a warning naming the variable, got {warning:?}"
            );
        }
    }
}

// Criterion (plain mode): unset or empty neither enables nor warns. Near-miss:
// `1` in debug, which does enable.
#[test]
fn test_plain_http_choice_unset_or_empty_neither_enables_nor_warns() {
    assert!(
        plain_http_choice(Some("1"), true).enabled,
        "the near-miss enables"
    );
    for env in [None, Some("")] {
        for debug in [true, false] {
            assert_eq!(
                plain_http_choice(env, debug),
                PlainChoice {
                    enabled: false,
                    warning: None,
                },
                "{env:?} (debug {debug})"
            );
        }
    }
}

// ---- plain_bind ----

// Criterion (plain mode): with no `--bind` and no `BLUE2TH_BIND` (an empty one
// is unset), the plain mode binds `127.0.0.1:4000`.
#[test]
fn test_plain_bind_without_an_address_is_loopback_4000() {
    assert_eq!(plain_bind(None, None), Ok("127.0.0.1:4000".to_string()));
    assert_eq!(plain_bind(None, Some("")), Ok("127.0.0.1:4000".to_string()));
}

// Criterion (plain mode): an explicit IP-literal loopback is accepted, from
// `--bind` or from `BLUE2TH_BIND`, `--bind` first. Distinct ports, so a swap
// of the two fails.
#[test]
fn test_plain_bind_accepts_an_ip_literal_loopback() {
    for addr in ["127.0.0.1:4000", "127.0.0.2:4000", "[::1]:4000"] {
        assert_eq!(
            plain_bind(Some(addr), None),
            Ok(addr.to_string()),
            "--bind {addr}"
        );
        assert_eq!(
            plain_bind(None, Some(addr)),
            Ok(addr.to_string()),
            "BLUE2TH_BIND={addr}"
        );
    }
    assert_eq!(
        plain_bind(Some("127.0.0.1:4001"), Some("127.0.0.1:4002")),
        Ok("127.0.0.1:4001".to_string()),
        "--bind wins over BLUE2TH_BIND"
    );
}

// Criterion (plain mode), guard: a non-loopback address is refused, with a
// message naming it and the loopback rule. Near-misses: `localhost:4000`
// (loopback in practice, but not an IP literal) and `0.0.0.0:4000`
// (unspecified, which reaches loopback too).
#[test]
fn test_plain_bind_refuses_anything_but_an_ip_literal_loopback() {
    for addr in [
        "192.168.1.20:4000",
        "0.0.0.0:4000",
        "localhost:4000",
        "[::]:4000",
    ] {
        for (cli, env) in [(Some(addr), None), (None, Some(addr))] {
            let refused = plain_bind(cli, env);
            assert!(refused.is_err(), "{addr} must be refused, got {refused:?}");
            let message = refused.err().unwrap_or_default();
            assert!(
                message.contains(addr) && message.contains("loopback"),
                "the refusal names {addr} and the loopback rule, got {message:?}"
            );
        }
    }
}

// ---- plain_router ----

// Criterion (plain-HTTP router), guard: `GET /health` answers 200 with the
// reduced payload — status ok, version `0.0.0` (never the release version),
// auth required, protocol 3..=3.
#[tokio::test]
async fn test_plain_router_health_is_the_reduced_payload() {
    let response = plain(request("GET", "/health", &[])).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("read the body");
    let health: Option<HealthStatus> = serde_json::from_slice(&bytes).ok();
    assert_eq!(
        health,
        Some(HealthStatus {
            status: "ok".to_string(),
            version: "0.0.0".to_string(),
            auth_required: true,
            protocol: 3,
            protocol_min: 3,
        }),
        "got {}",
        String::from_utf8_lossy(&bytes)
    );
    assert!(
        !String::from_utf8_lossy(&bytes).contains(env!("CARGO_PKG_VERSION")),
        "the release version never appears in the plain /health"
    );
}

// Criterion (plain-HTTP router): `GET /settings?x=1` with `Host:
// 192.168.1.20:4000` answers 308 to the same path and query over `https://`.
#[tokio::test]
async fn test_plain_router_redirects_a_browser_to_https_with_path_and_query() {
    let response = plain(request(
        "GET",
        "/settings?x=1",
        &[("host", "192.168.1.20:4000")],
    ))
    .await;
    assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
    assert_eq!(
        location(&response).as_deref(),
        Some("https://192.168.1.20:4000/settings?x=1")
    );

    let root = plain(request(
        "GET",
        "/",
        &[("host", "blue2th-ba7816bf8f.local:4000")],
    ))
    .await;
    assert_eq!(root.status(), StatusCode::PERMANENT_REDIRECT);
    assert_eq!(
        location(&root).as_deref(),
        Some("https://blue2th-ba7816bf8f.local:4000/"),
        "no query, no `?`"
    );
}

// Criterion (plain-HTTP router), guard: `GET /` with no `Host`, or an empty
// one, answers 400 and carries no `Location` — never `https:///`. Near-miss:
// the empty `Host`, which a check on the header's presence alone redirects.
#[tokio::test]
async fn test_plain_router_without_a_host_answers_400_and_no_location() {
    let with_host = plain(request("GET", "/", &[("host", "192.168.1.20:4000")])).await;
    assert_eq!(
        with_host.status(),
        StatusCode::PERMANENT_REDIRECT,
        "the near-miss"
    );

    for (case, headers) in [("no Host", vec![]), ("empty Host", vec![("host", "")])] {
        let response = plain(request("GET", "/", &headers)).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{case}");
        assert_eq!(location(&response), None, "{case}");
    }
}

// Criterion (plain-HTTP router), guard: under `/api`, with a bearer token, the
// plain router answers 426: it holds no API handler and never reads the token.
// (`tests/tls_listener.rs` pins the near-miss: the same token is valid over TLS.)
#[tokio::test]
async fn test_plain_router_answers_426_under_api_whatever_the_token() {
    for uri in ["/api/devices", "/api/config", "/api", "/api/"] {
        for headers in [vec![], vec![("authorization", "Bearer test-api-token")]] {
            let mut headers = headers;
            headers.push(("host", "192.168.1.20:4000"));
            let response = plain(request("GET", uri, &headers)).await;
            assert_eq!(response.status(), StatusCode::UPGRADE_REQUIRED, "GET {uri}");
            assert_eq!(location(&response), None, "GET {uri} is not redirected");
        }
    }
}

// Criterion (plain-HTTP router), guard: "under `/api`" is a path segment, not a
// prefix — `/apiary` is a browser path like any other and is redirected.
#[tokio::test]
async fn test_plain_router_redirects_a_path_that_only_starts_like_api() {
    let response = plain(request("GET", "/apiary", &[("host", "192.168.1.20:4000")])).await;
    assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
    assert_eq!(
        location(&response).as_deref(),
        Some("https://192.168.1.20:4000/apiary")
    );
}

// Criterion (plain-HTTP router): any method but `GET` answers 426 —
// `POST /health` and `POST /api/pair` included. Near-miss: `GET /health`
// answers 200.
#[tokio::test]
async fn test_plain_router_answers_426_to_any_method_but_get() {
    let get = plain(request("GET", "/health", &[("host", "192.168.1.20:4000")])).await;
    assert_eq!(get.status(), StatusCode::OK, "the near-miss");

    for (method, uri) in [
        ("POST", "/health"),
        ("POST", "/api/pair"),
        ("PUT", "/settings"),
        ("DELETE", "/"),
    ] {
        let response = plain(request(method, uri, &[("host", "192.168.1.20:4000")])).await;
        assert_eq!(
            response.status(),
            StatusCode::UPGRADE_REQUIRED,
            "{method} {uri}"
        );
        assert_eq!(
            location(&response),
            None,
            "{method} {uri} is not redirected"
        );
    }
}
