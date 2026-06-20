//! blue2th PC backend library facade.
//!
//! Exposes the Axum router (`app()`) and the server entry point (`run()`) so
//! both the binary (`main.rs`) and integration tests can drive the same surface
//! in-process. Phase 0 only exposed `GET /health`; later phases add Bluetooth
//! (`bluer`) and audio (PipeWire) routes — see `docs/ROADMAP.md`.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
    Json, Router,
};
use blue2th_proto::{AdapterInfo, DeviceInfo, HealthStatus, PlaybackState, VolumeRequest};
use futures::{Stream, StreamExt};
use tokio::sync::Mutex;
use tower_http::cors::CorsLayer;
use tracing_subscriber::EnvFilter;

pub mod audio;
mod bluetooth;

use audio::{AudioEngine, AudioError};

/// Shared application state: the single audio engine guarded for concurrent
/// access. The backend was stateless before phase 3; playback needs shared
/// mutable state injected through the Axum router (no globals).
#[derive(Clone)]
struct AppState {
    engine: Arc<Mutex<AudioEngine>>,
}

/// Hard cap on a single scan so a forgotten client cannot keep discovery running.
const SCAN_DURATION: Duration = Duration::from_secs(20);

/// Default bind address. `0.0.0.0` so the phone can reach the backend over the LAN.
const DEFAULT_BIND: &str = "0.0.0.0:4000";

/// Run the backend: initialise tracing, bind the socket and serve the router.
pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();

    let addr = std::env::var("BLUE2TH_BIND").unwrap_or_else(|_| DEFAULT_BIND.to_string());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("blue2th-server listening on http://{addr}");

    axum::serve(listener, app()).await?;
    Ok(())
}

/// Build the application router. Kept separate from `run` so tests can exercise
/// it in-process without binding a socket.
pub fn app() -> Router {
    let state = AppState {
        engine: Arc::new(Mutex::new(AudioEngine::new())),
    };

    Router::new()
        .route("/health", get(health))
        .route("/adapters", get(adapters))
        .route("/devices", get(devices))
        .route("/devices/{addr}/connect", post(connect))
        .route("/devices/{addr}/disconnect", post(disconnect))
        .route("/scan", get(scan))
        .route("/play", post(play))
        .route("/pause", post(pause))
        .route("/stop", post(stop))
        .route("/volume", post(volume))
        .route("/playback", get(playback))
        // Permissive CORS for LAN development; tightened in a later phase.
        .layer(CorsLayer::permissive())
        .with_state(state)
}

/// `POST /play` — start (or resume) playback of the embedded test file.
async fn play(State(state): State<AppState>) -> Result<Json<PlaybackState>, AppError> {
    let mut engine = state.engine.lock().await;
    Ok(Json(engine.play()?))
}

/// `POST /pause` — pause playback (idempotent while stopped).
async fn pause(State(state): State<AppState>) -> Result<Json<PlaybackState>, AppError> {
    let mut engine = state.engine.lock().await;
    Ok(Json(engine.pause()?))
}

/// `POST /stop` — stop playback (idempotent while stopped).
async fn stop(State(state): State<AppState>) -> Result<Json<PlaybackState>, AppError> {
    let mut engine = state.engine.lock().await;
    Ok(Json(engine.stop()?))
}

/// `POST /volume` — set the connected speaker's PipeWire sink volume (clamped).
async fn volume(
    State(state): State<AppState>,
    Json(req): Json<VolumeRequest>,
) -> Result<Json<PlaybackState>, AppError> {
    let mut engine = state.engine.lock().await;
    Ok(Json(engine.set_volume(req.level)?))
}

/// `GET /playback` — current playback state.
async fn playback(State(state): State<AppState>) -> Json<PlaybackState> {
    let engine = state.engine.lock().await;
    Json(engine.playback_state())
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

/// `POST /devices/{addr}/connect` — pair/trust/connect a device, returning its
/// updated state.
async fn connect(Path(addr): Path<String>) -> Result<Json<DeviceInfo>, AppError> {
    Ok(Json(bluetooth::connect_device(parse_addr(&addr)?).await?))
}

/// `POST /devices/{addr}/disconnect` — disconnect a device, returning its
/// updated state.
async fn disconnect(Path(addr): Path<String>) -> Result<Json<DeviceInfo>, AppError> {
    Ok(Json(bluetooth::disconnect_device(parse_addr(&addr)?).await?))
}

/// Parse a path MAC address, returning a 400-style error on malformed input.
fn parse_addr(addr: &str) -> Result<bluer::Address, AppError> {
    addr.parse::<bluer::Address>()
        .map_err(|e| AppError::bad_request(format!("invalid address '{addr}': {e}")))
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

/// Error type for handlers: renders a status code with the message. BlueZ
/// failures (no adapter, bluetoothd down) convert into it via
/// `From<bluer::Error>`; audio failures via `From<AudioError>`.
struct AppError {
    status: StatusCode,
    message: String,
}

impl AppError {
    /// A 500 error carrying the given message (the historical behaviour).
    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }

    /// A 400 error carrying the given message (bad client input / preconditions).
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        tracing::warn!("request failed: {}", self.message);
        (self.status, self.message).into_response()
    }
}

impl From<bluer::Error> for AppError {
    fn from(err: bluer::Error) -> Self {
        AppError::internal(err.to_string())
    }
}

impl From<AudioError> for AppError {
    fn from(err: AudioError) -> Self {
        match err {
            // No connected speaker is a precondition failure, not a server bug.
            AudioError::NoSpeakerConnected => AppError::bad_request(err.to_string()),
            AudioError::Decode(_) | AudioError::PipeWire(_) => AppError::internal(err.to_string()),
        }
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
