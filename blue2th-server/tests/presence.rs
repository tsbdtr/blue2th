// SPDX-License-Identifier: MIT OR Apache-2.0

//! Route pin for `POST /client/presence` (#71): the report is accepted with 204
//! whatever the clock does behind it. The watchdog tick and its log line run on a
//! spawned sleep loop and are not test-runnable here; the idle-clock arithmetic
//! lives in the unit tests of `blue2th_server::watchdog`.

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use blue2th_server::{auth::AuthStore, spotify_auth::SpotifyAuth};
use tower::ServiceExt; // for `oneshot`

/// The API token these tests pair with, held in memory only (see `tests/spotify.rs`).
const TOKEN: &str = "test-api-token";

fn authorized(builder: axum::http::request::Builder) -> axum::http::request::Builder {
    builder.header("authorization", format!("Bearer {TOKEN}"))
}

fn build_app() -> axum::Router {
    blue2th_server::app_with_auth_store(
        SpotifyAuth::with_config(None, "blue2th://spotify-callback".to_string()),
        AuthStore::with_token(TOKEN),
    )
}

// Criterion: `POST /client/presence` with `{"presence":"foreground"}` answers 204,
// unchanged — the handler now stamps the report with `Instant::now()`, which the
// wire contract does not see.
#[tokio::test]
async fn test_post_client_presence_foreground_returns_no_content() {
    let request = authorized(Request::builder())
        .method("POST")
        .uri("/client/presence")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"presence":"foreground"}"#))
        .expect("build request");

    let response = build_app().oneshot(request).await.expect("router response");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

// Criterion: a `background` report is accepted the same way — the report that
// starts the thirty-minute backstop must never be refused.
#[tokio::test]
async fn test_post_client_presence_background_returns_no_content() {
    let request = authorized(Request::builder())
        .method("POST")
        .uri("/client/presence")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"presence":"background"}"#))
        .expect("build request");

    let response = build_app().oneshot(request).await.expect("router response");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}
