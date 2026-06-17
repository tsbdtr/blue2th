//! blue2th PC backend.
//!
//! Phase 0: an Axum service exposing `GET /health`. Later phases add Bluetooth
//! (`bluer`) and audio (PipeWire) routes — see `docs/ROADMAP.md`.

use std::convert::Infallible;
use std::time::Duration;

use axum::{
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::get,
    Json, Router,
};
use blue2th_proto::{AdapterInfo, DeviceInfo, HealthStatus};
use futures::{Stream, StreamExt};
use tower_http::cors::CorsLayer;
use tracing_subscriber::EnvFilter;

mod bluetooth;

/// Hard cap on a single scan so a forgotten client cannot keep discovery running.
const SCAN_DURATION: Duration = Duration::from_secs(20);

/// Default bind address. `0.0.0.0` so the phone can reach the backend over the LAN.
const DEFAULT_BIND: &str = "0.0.0.0:4000";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();

    let addr = std::env::var("BLUE2TH_BIND").unwrap_or_else(|_| DEFAULT_BIND.to_string());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("blue2th-server listening on http://{addr}");

    axum::serve(listener, app()).await?;
    Ok(())
}

/// Build the application router. Kept separate from `main` so tests can exercise
/// it in-process without binding a socket.
fn app() -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/adapters", get(adapters))
        .route("/devices", get(devices))
        .route("/scan", get(scan))
        // Permissive CORS for LAN development; tightened in a later phase.
        .layer(CorsLayer::permissive())
}

/// `GET /health` — liveness probe carrying the backend version.
async fn health() -> Json<HealthStatus> {
    Json(HealthStatus::ok(env!("CARGO_PKG_VERSION")))
}

/// `GET /adapters` — Bluetooth adapters present on the host.
async fn adapters() -> Result<Json<Vec<AdapterInfo>>, AppError> {
    Ok(Json(bluetooth::list_adapters().await?))
}

/// `GET /devices` — paired devices on the default adapter.
async fn devices() -> Result<Json<Vec<DeviceInfo>>, AppError> {
    Ok(Json(bluetooth::list_paired_devices().await?))
}

/// `GET /scan` — Server-Sent Events stream of devices discovered by an active
/// scan. Emits a `device` event per discovery and an `error` event on failure;
/// the scan stops after `SCAN_DURATION` or when the client disconnects.
async fn scan() -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let deadline = tokio::time::sleep(SCAN_DURATION);
    let stream = bluetooth::scan_events()
        .map(|result| {
            let event = match result {
                Ok(device) => Event::default()
                    .event("device")
                    .json_data(device)
                    .unwrap_or_else(|_| Event::default().comment("serialization failed")),
                Err(e) => Event::default().event("error").data(e.to_string()),
            };
            Ok(event)
        })
        .take_until(deadline);

    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// Error type for handlers: renders a 500 with the message. BlueZ failures
/// (no adapter, bluetoothd down) convert into it via `From<bluer::Error>`.
struct AppError(String);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        tracing::warn!("request failed: {}", self.0);
        (StatusCode::INTERNAL_SERVER_ERROR, self.0).into_response()
    }
}

impl From<bluer::Error> for AppError {
    fn from(err: bluer::Error) -> Self {
        AppError(err.to_string())
    }
}

/// Initialise tracing from `RUST_LOG`, defaulting to `info`.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt; // for `oneshot`

    #[tokio::test]
    async fn test_health_endpoint_returns_ok_status_and_version() {
        let request = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .expect("build request");

        let response = app().oneshot(request).await.expect("router response");
        assert_eq!(response.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let parsed: HealthStatus = serde_json::from_slice(&bytes).expect("parse HealthStatus");

        assert_eq!(parsed.status, "ok");
        assert_eq!(parsed.version, env!("CARGO_PKG_VERSION"));
    }
}
