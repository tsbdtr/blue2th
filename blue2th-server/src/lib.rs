//! blue2th PC backend library facade.
//!
//! Exposes the Axum router (`app()`) and the server entry point (`run()`) so
//! both the binary (`main.rs`) and integration tests can drive the same surface
//! in-process. Phase 0 only exposed `GET /health`; later phases add Bluetooth
//! (`bluer`) and audio (PipeWire) routes — see `docs/ROADMAP.md`.

use std::{convert::Infallible, sync::Arc, time::Duration};

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

use audio::{AudioEngine, AudioError, RodioOutput};

/// Shared application state: the single audio engine guarded for concurrent
/// access. The backend was stateless before phase 3; playback needs shared
/// mutable state injected through the Axum router (no globals).
#[derive(Clone)]
struct AppState {
    engine: Arc<Mutex<AudioEngine>>,
    /// The speaker playback is routed to, if any. `None` until a Bluetooth
    /// speaker is connected; `/play` is rejected with a 4xx while empty so we
    /// never start a stream with nowhere to send it.
    connected_speaker: Arc<Mutex<Option<String>>>,
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
        // Real playback output (rodio → PipeWire); the device is opened lazily on
        // the first `/play`, so building the router stays cheap and CI-safe.
        engine: Arc::new(Mutex::new(AudioEngine::with_output(Box::new(
            RodioOutput::new(),
        )))),
        connected_speaker: Arc::new(Mutex::new(None)),
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

/// `POST /play` — start (or resume) playback of the embedded test file on the
/// connected speaker. Gated on the cached playback target, which `/connect`,
/// `/scan` and `/devices` keep in sync with the live connection state.
async fn play(State(state): State<AppState>) -> Result<Json<PlaybackState>, AppError> {
    // Clone the cached address to release the guard before the PipeWire calls.
    let speaker = state
        .connected_speaker
        .lock()
        .await
        .clone()
        .ok_or(AudioError::NoSpeakerConnected)?;
    // Route audio output + volume to the speaker's PipeWire sink before playing.
    audio::route_to_speaker(&speaker)?;
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

/// `POST /volume` — set the connected speaker's PipeWire sink volume (clamped),
/// targeting that sink by name so it round-trips with `/playback`.
async fn volume(
    State(state): State<AppState>,
    Json(req): Json<VolumeRequest>,
) -> Result<Json<PlaybackState>, AppError> {
    // Clone the cached target to release the guard before the PipeWire call.
    let speaker = state
        .connected_speaker
        .lock()
        .await
        .clone()
        .ok_or(AudioError::NoSpeakerConnected)?;
    audio::set_sink_volume(&speaker, req.level)?;
    let mut engine = state.engine.lock().await;
    Ok(Json(engine.set_volume(req.level)?))
}

/// `GET /playback` — current playback state, reconciled so it returns to
/// `Stopped` once the tone ends on its own, and carrying the *live* sink volume
/// so a change made on the speaker itself is reflected in the app.
async fn playback(State(state): State<AppState>) -> Json<PlaybackState> {
    let mut snapshot = {
        let mut engine = state.engine.lock().await;
        engine.poll_state()
    };
    // Clone the target out of the guard before the (blocking) PipeWire read.
    let speaker = state.connected_speaker.lock().await.clone();
    if let Some(speaker) = speaker {
        if let Some(volume) = audio::sink_volume(&speaker) {
            snapshot.volume = volume;
        }
    }
    Json(snapshot)
}

/// `GET /health` — liveness probe carrying the backend version.
async fn health() -> Json<HealthStatus> {
    Json(HealthStatus::ok(env!("CARGO_PKG_VERSION")))
}

/// `GET /adapters` — Bluetooth adapters present on the host.
async fn adapters() -> Result<Json<Vec<AdapterInfo>>, AppError> {
    Ok(Json(bluetooth::list_adapters().await?))
}

/// `GET /devices` — paired devices on the default adapter. Re-syncs the playback
/// target with the live connection state so a speaker connected (or disconnected)
/// outside the app is reflected for `/play`.
async fn devices(State(state): State<AppState>) -> Result<Json<Vec<DeviceInfo>>, AppError> {
    let devices = bluetooth::list_paired_devices().await?;
    // Clone the address of the first connected device (if any) into the cache.
    let connected = devices
        .iter()
        .find(|d| d.connected)
        .map(|d| d.address.clone());
    *state.connected_speaker.lock().await = connected;
    Ok(Json(devices))
}

/// `POST /devices/{addr}/connect` — pair/trust/connect a device, returning its
/// updated state. On success the device becomes the playback target so `/play`
/// has somewhere to route audio.
async fn connect(
    State(state): State<AppState>,
    Path(addr): Path<String>,
) -> Result<Json<DeviceInfo>, AppError> {
    let device = bluetooth::connect_device(parse_addr(&addr)?).await?;
    // Clone the address: it is both stored as the playback target and returned
    // to the caller in the device payload below.
    *state.connected_speaker.lock().await = Some(device.address.clone());
    Ok(Json(device))
}

/// `POST /devices/{addr}/disconnect` — disconnect a device, returning its
/// updated state. Clears the playback target if it was this device.
async fn disconnect(
    State(state): State<AppState>,
    Path(addr): Path<String>,
) -> Result<Json<DeviceInfo>, AppError> {
    let device = bluetooth::disconnect_device(parse_addr(&addr)?).await?;
    let mut target = state.connected_speaker.lock().await;
    if target.as_deref() == Some(device.address.as_str()) {
        *target = None;
    }
    Ok(Json(device))
}

/// Parse a path MAC address, returning a 400-style error on malformed input.
fn parse_addr(addr: &str) -> Result<bluer::Address, AppError> {
    addr.parse::<bluer::Address>()
        .map_err(|e| AppError::bad_request(format!("invalid address '{addr}': {e}")))
}

/// `GET /scan` — Server-Sent Events stream of devices discovered by an active
/// scan. Emits a `device` event per discovery and an `error` event on failure;
/// the scan stops after `SCAN_DURATION` or when the client disconnects.
async fn scan(State(state): State<AppState>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let deadline = tokio::time::sleep(SCAN_DURATION);
    // Arc clone so the stream closure can heal the playback target as soon as a
    // connected speaker is discovered (no need to wait for a /devices poll).
    let target = state.connected_speaker.clone();
    let stream = bluetooth::scan_events()
        .map(move |result| {
            if let Ok(device) = &result {
                if device.connected {
                    if let Ok(mut guard) = target.try_lock() {
                        // Clone the address into the cache; best-effort, skipped
                        // if the lock is momentarily held elsewhere.
                        *guard = Some(device.address.clone());
                    }
                }
            }
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
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;

    use super::*; // for `oneshot`

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
