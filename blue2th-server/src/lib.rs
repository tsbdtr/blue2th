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
use blue2th_proto::{
    AdapterInfo, AuthCallbackRequest, AuthUrlResponse, DeviceInfo, HealthStatus, OffsetRequest,
    PlaybackState, SpotifyAuthState, SpotifyState, TargetsState, VolumeRequest,
};
use futures::{Stream, StreamExt};
use tokio::sync::Mutex;
use tower_http::cors::CorsLayer;
use tracing_subscriber::EnvFilter;

pub mod audio;
mod bluetooth;
pub mod spotify;
pub mod spotify_auth;
pub mod targets;

use audio::{AudioEngine, AudioError, RodioOutput};
use spotify::{SpotifyBackend, SpotifyError};
use spotify_auth::{SpotifyApiError, SpotifyAuth, Transport};
use targets::{SelectError, SpeakerTargets};

/// Shared application state injected through the Axum router (no globals).
#[derive(Clone)]
struct AppState {
    /// The audio engine, guarded for concurrent access.
    engine: Arc<Mutex<AudioEngine>>,
    /// The user's playback-target selection (0–2 speakers + offsets). `/play`
    /// derives its routing mode from this; an empty selection (`Idle`) is
    /// rejected with a 4xx so a stream never starts with nowhere to go.
    targets: Arc<Mutex<SpeakerTargets>>,
    /// Addresses of the currently connected speakers, kept in sync by
    /// `connect`/`disconnect`/`devices`/`scan`. Used to validate a `select`
    /// request and to drop disconnected speakers from the selection.
    connected: Arc<Mutex<Vec<String>>>,
    /// The Spotify source backend (a `librespot` subprocess), guarded for
    /// concurrent access by the `/spotify/*` handlers.
    spotify: Arc<Mutex<SpotifyBackend>>,
    /// The Spotify Web API auth driver (OAuth PKCE tokens + transport), guarded
    /// for concurrent access by the `/spotify/auth/*` and transport handlers.
    spotify_auth: Arc<Mutex<SpotifyAuth>>,
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
        targets: Arc::new(Mutex::new(SpeakerTargets::new())),
        connected: Arc::new(Mutex::new(Vec::new())),
        spotify: Arc::new(Mutex::new(SpotifyBackend::new())),
        spotify_auth: Arc::new(Mutex::new(SpotifyAuth::new())),
    };

    Router::new()
        .route("/health", get(health))
        .route("/adapters", get(adapters))
        .route("/devices", get(devices))
        .route("/devices/{addr}/connect", post(connect))
        .route("/devices/{addr}/disconnect", post(disconnect))
        .route("/devices/{addr}/select", post(select_target))
        .route("/devices/{addr}/deselect", post(deselect_target))
        .route("/devices/{addr}/offset", post(set_target_offset))
        .route("/targets", get(get_targets))
        .route("/scan", get(scan))
        .route("/play", post(play))
        .route("/pause", post(pause))
        .route("/stop", post(stop))
        .route("/volume", post(volume))
        .route("/playback", get(playback))
        .route("/spotify/start", post(spotify_start))
        .route("/spotify/stop", post(spotify_stop))
        .route("/spotify/status", get(spotify_status))
        .route("/spotify/auth/url", get(spotify_auth_url))
        .route("/spotify/auth/callback", post(spotify_auth_callback))
        .route("/spotify/auth/status", get(spotify_auth_status))
        .route("/spotify/play", post(spotify_play))
        .route("/spotify/pause", post(spotify_pause))
        .route("/spotify/next", post(spotify_next))
        .route("/spotify/previous", post(spotify_previous))
        .route("/spotify/now-playing", get(spotify_now_playing))
        // Permissive CORS for LAN development; tightened in a later phase.
        .layer(CorsLayer::permissive())
        .with_state(state)
}

/// `POST /play` — start (or resume) playback of the embedded test file, routed
/// per the current target selection: one speaker uses the single-sink path, two
/// build a PipeWire combined sink. An empty selection (`Idle`) is rejected (4xx).
async fn play(State(state): State<AppState>) -> Result<Json<PlaybackState>, AppError> {
    // Snapshot the selection and release the guard before the blocking PipeWire calls.
    let speakers = state.targets.lock().await.speakers();
    match speakers.len() {
        0 => return Err(AudioError::NoSpeakerConnected.into()),
        // One target: keep the phase-3 single-sink path (default-sink hijack).
        1 => audio::route_to_speaker(&speakers[0].address)?,
        // Two targets: fan out through a combined sink with per-speaker latency.
        _ => audio::route_to_combined(&audio::combine_sink_plan(&speakers))?,
    }
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

/// `POST /volume` — set the selected speakers' PipeWire sink volume (clamped),
/// applied to every target sink so both stay in step and round-trip with
/// `/playback`. An empty selection is rejected (4xx).
async fn volume(
    State(state): State<AppState>,
    Json(req): Json<VolumeRequest>,
) -> Result<Json<PlaybackState>, AppError> {
    // Snapshot the selection and release the guard before the PipeWire calls.
    let speakers = state.targets.lock().await.speakers();
    if speakers.is_empty() {
        return Err(AudioError::NoSpeakerConnected.into());
    }
    for target in &speakers {
        audio::set_sink_volume(&target.address, req.level)?;
    }
    let mut engine = state.engine.lock().await;
    Ok(Json(engine.set_volume(req.level)?))
}

/// `GET /playback` — current playback state, reconciled so it returns to
/// `Stopped` once the tone ends on its own, and carrying the *live* sink volume
/// (of the first target) so a change made on the speaker itself is reflected.
async fn playback(State(state): State<AppState>) -> Json<PlaybackState> {
    let mut snapshot = {
        let mut engine = state.engine.lock().await;
        engine.poll_state()
    };
    // Read the live volume from the first target's sink, outside the guard.
    let first = state.targets.lock().await.speakers().into_iter().next();
    if let Some(target) = first {
        if let Some(volume) = audio::sink_volume(&target.address) {
            snapshot.volume = volume;
        }
    }
    Json(snapshot)
}

/// `POST /spotify/start` — activate the Spotify source backend: snapshot the
/// current target selection (like `/play`) and spawn the `librespot` Connect
/// device pointed at the matching sink. An empty selection is rejected (400).
async fn spotify_start(State(state): State<AppState>) -> Result<Json<SpotifyState>, AppError> {
    // Snapshot the selection and release the guard before touching the backend.
    let speakers = state.targets.lock().await.speakers();
    let mut spotify = state.spotify.lock().await;
    Ok(Json(spotify.start(&speakers)?))
}

/// `POST /spotify/stop` — deactivate the Spotify source backend (kill the
/// subprocess), returning its reconciled state.
async fn spotify_stop(State(state): State<AppState>) -> Result<Json<SpotifyState>, AppError> {
    let mut spotify = state.spotify.lock().await;
    Ok(Json(spotify.stop()?))
}

/// `GET /spotify/status` — the Spotify backend's current state, reconciled so a
/// subprocess that exited on its own is reported as `Stopped`.
async fn spotify_status(State(state): State<AppState>) -> Json<SpotifyState> {
    let mut spotify = state.spotify.lock().await;
    Json(spotify.poll_liveness())
}

/// `GET /spotify/auth/url` — mint a PKCE authorize URL and CSRF `state` for the
/// app to open in the system browser; the pending verifier/state is remembered
/// server-side until the callback.
async fn spotify_auth_url(State(state): State<AppState>) -> Json<AuthUrlResponse> {
    let (url, csrf) = state.spotify_auth.lock().await.authorize_url();
    Json(AuthUrlResponse { url, state: csrf })
}

/// `POST /spotify/auth/callback` — exchange the authorization `code` (validated
/// against the pending CSRF `state`) for tokens. A body missing `code` is a
/// malformed callback and is rejected with 400; the token exchange is a manual
/// network seam.
async fn spotify_auth_callback(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<Json<SpotifyAuthState>, AppError> {
    // Parse leniently so a missing `code` yields a 400 (not Axum's default 422).
    let req: AuthCallbackRequest = serde_json::from_slice(&body)
        .map_err(|e| AppError::bad_request(format!("invalid callback body: {e}")))?;
    let mut auth = state.spotify_auth.lock().await;
    Ok(Json(auth.exchange_code(&req.code, &req.state).await?))
}

/// `GET /spotify/auth/status` — the coarse auth state (Connected/Disconnected).
async fn spotify_auth_status(State(state): State<AppState>) -> Json<SpotifyAuthState> {
    Json(state.spotify_auth.lock().await.auth_state())
}

/// `POST /spotify/play` — resume Web API playback (409 while Disconnected).
async fn spotify_play(State(state): State<AppState>) -> Result<StatusCode, AppError> {
    spotify_transport(&state, Transport::Play).await
}

/// `POST /spotify/pause` — pause Web API playback (409 while Disconnected).
async fn spotify_pause(State(state): State<AppState>) -> Result<StatusCode, AppError> {
    spotify_transport(&state, Transport::Pause).await
}

/// `POST /spotify/next` — skip to the next track (409 while Disconnected).
async fn spotify_next(State(state): State<AppState>) -> Result<StatusCode, AppError> {
    spotify_transport(&state, Transport::Next).await
}

/// `POST /spotify/previous` — skip to the previous track (409 while Disconnected).
async fn spotify_previous(State(state): State<AppState>) -> Result<StatusCode, AppError> {
    spotify_transport(&state, Transport::Previous).await
}

/// Drive a transport action on the Spotify Web API, returning 204 on success.
/// While Disconnected the auth driver rejects before any outbound call (→ 409).
async fn spotify_transport(state: &AppState, action: Transport) -> Result<StatusCode, AppError> {
    let mut auth = state.spotify_auth.lock().await;
    auth.transport(action).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /spotify/now-playing` — Server-Sent Events stream of now-playing
/// snapshots polled from the Web API. Emits a `now-playing` event per tick; a
/// Disconnected server keeps the stream alive with keep-alive comments only.
async fn spotify_now_playing(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let auth = state.spotify_auth.clone();
    let stream = async_stream::stream! {
        loop {
            let snapshot = {
                let mut guard = auth.lock().await;
                guard.now_playing().await
            };
            if let Ok(now_playing) = snapshot {
                let event = Event::default()
                    .event("now-playing")
                    .json_data(now_playing)
                    .unwrap_or_else(|_| Event::default().comment("serialization failed"));
                yield Ok(event);
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// `GET /health` — liveness probe carrying the backend version.
async fn health() -> Json<HealthStatus> {
    Json(HealthStatus::ok(env!("CARGO_PKG_VERSION")))
}

/// `GET /adapters` — Bluetooth adapters present on the host.
async fn adapters() -> Result<Json<Vec<AdapterInfo>>, AppError> {
    Ok(Json(bluetooth::list_adapters().await?))
}

/// `GET /devices` — paired devices on the default adapter. Re-syncs the connected
/// cache with the live state so a speaker connected (or disconnected) outside the
/// app is reflected, and drops any disconnected speaker from the target selection.
async fn devices(State(state): State<AppState>) -> Result<Json<Vec<DeviceInfo>>, AppError> {
    let devices = bluetooth::list_paired_devices().await?;
    sync_connected(&state, &devices).await;
    Ok(Json(devices))
}

/// Refresh the connected-address cache from a device list and drop any selected
/// target that is no longer connected (keeping the selection and routing honest).
async fn sync_connected(state: &AppState, devices: &[DeviceInfo]) {
    let connected: Vec<String> = devices
        .iter()
        .filter(|d| d.connected)
        .map(|d| d.address.clone())
        .collect();
    *state.connected.lock().await = connected.clone();
    state.targets.lock().await.retain_connected(&connected);
}

/// `POST /devices/{addr}/connect` — pair/trust/connect a device, returning its
/// updated state. The device joins the connected cache so it becomes selectable
/// as a playback target (selection itself is explicit, via `/select`).
async fn connect(
    State(state): State<AppState>,
    Path(addr): Path<String>,
) -> Result<Json<DeviceInfo>, AppError> {
    let device = bluetooth::connect_device(parse_addr(&addr)?).await?;
    {
        let mut conn = state.connected.lock().await;
        if !conn.iter().any(|a| a == &device.address) {
            conn.push(device.address.clone());
        }
    }
    Ok(Json(device))
}

/// `POST /devices/{addr}/disconnect` — disconnect a device, returning its updated
/// state. Drops it from the connected cache and from the target selection.
async fn disconnect(
    State(state): State<AppState>,
    Path(addr): Path<String>,
) -> Result<Json<DeviceInfo>, AppError> {
    let device = bluetooth::disconnect_device(parse_addr(&addr)?).await?;
    let connected = {
        let mut conn = state.connected.lock().await;
        conn.retain(|a| a != &device.address);
        conn.clone()
    };
    state.targets.lock().await.retain_connected(&connected);
    Ok(Json(device))
}

/// `POST /devices/{addr}/select` — select a connected speaker as a playback
/// target. Rejected (4xx) if it is not connected or the two-speaker cap is hit.
async fn select_target(
    State(state): State<AppState>,
    Path(addr): Path<String>,
) -> Result<Json<TargetsState>, AppError> {
    let connected = { state.connected.lock().await.clone() };
    let mut targets = state.targets.lock().await;
    targets.select(&addr, &connected)?;
    Ok(Json(targets.state()))
}

/// `POST /devices/{addr}/deselect` — drop a speaker from the target selection.
async fn deselect_target(
    State(state): State<AppState>,
    Path(addr): Path<String>,
) -> Json<TargetsState> {
    let mut targets = state.targets.lock().await;
    targets.deselect(&addr);
    Json(targets.state())
}

/// `POST /devices/{addr}/offset` — set a target speaker's latency offset (clamped
/// server-side to `0..=750` ms). A no-op if the speaker is not selected.
async fn set_target_offset(
    State(state): State<AppState>,
    Path(addr): Path<String>,
    Json(req): Json<OffsetRequest>,
) -> Json<TargetsState> {
    let mut targets = state.targets.lock().await;
    targets.set_offset(&addr, req.offset_ms);
    Json(targets.state())
}

/// `GET /targets` — the current selection, per-speaker offsets and routing mode.
async fn get_targets(State(state): State<AppState>) -> Json<TargetsState> {
    Json(state.targets.lock().await.state())
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
    // Arc clone so the stream closure can add a discovered connected speaker to
    // the connected cache as soon as it is seen (no need to wait for a /devices poll).
    let connected = state.connected.clone();
    let stream = bluetooth::scan_events()
        .map(move |result| {
            if let Ok(device) = &result {
                if device.connected {
                    if let Ok(mut guard) = connected.try_lock() {
                        // Best-effort cache update; skipped if the lock is momentarily
                        // held elsewhere. Removal is handled by /devices + /disconnect.
                        if !guard.iter().any(|a| a == &device.address) {
                            guard.push(device.address.clone());
                        }
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

impl From<SpotifyError> for AppError {
    fn from(err: SpotifyError) -> Self {
        match err {
            // No selected speaker is a precondition failure, not a server bug.
            SpotifyError::NoSpeakerSelected => AppError::bad_request(err.to_string()),
            // A missing binary or failed spawn is a backend/server-side fault.
            SpotifyError::BackendMissing | SpotifyError::Spawn(_) => {
                AppError::internal(err.to_string())
            },
        }
    }
}

impl From<SpotifyApiError> for AppError {
    fn from(err: SpotifyApiError) -> Self {
        let status = match err {
            // Transport while Disconnected: a precondition conflict, not a bug.
            SpotifyApiError::NotConnected => StatusCode::CONFLICT,
            // Access token rejected: the app must reauth (log in again).
            SpotifyApiError::Unauthorized => StatusCode::UNAUTHORIZED,
            // Non-Premium account: transport is forbidden by Spotify.
            SpotifyApiError::PremiumRequired => StatusCode::FORBIDDEN,
            // No active device to target.
            SpotifyApiError::NoActiveDevice => StatusCode::NOT_FOUND,
            // Rate limited by the Web API.
            SpotifyApiError::RateLimited(_) => StatusCode::TOO_MANY_REQUESTS,
            // Token exchange/refresh or any upstream HTTP failure: a bad gateway.
            SpotifyApiError::Exchange(_) | SpotifyApiError::Http(_) => StatusCode::BAD_GATEWAY,
        };
        AppError {
            status,
            message: err.to_string(),
        }
    }
}

impl From<SelectError> for AppError {
    fn from(err: SelectError) -> Self {
        // Both are bad client requests (not connected / cap exceeded), not server bugs.
        match err {
            SelectError::NotConnected => {
                AppError::bad_request("speaker is not connected".to_string())
            },
            SelectError::CapExceeded => {
                AppError::bad_request("at most two speakers can be selected".to_string())
            },
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

    // Criterion: a `NoSpeakerSelected` error maps to a 400 (precondition failure),
    // so `POST /spotify/start` with no target rejects the client.
    #[test]
    fn test_spotify_no_speaker_selected_maps_to_bad_request() {
        let err: AppError = SpotifyError::NoSpeakerSelected.into();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    // Criterion: a `NotFound` spawn error (`BackendMissing`) maps to a 500 with a
    // clear message ("Spotify backend unavailable").
    #[test]
    fn test_spotify_backend_missing_maps_to_internal_error() {
        let err: AppError = SpotifyError::BackendMissing.into();
        assert_eq!(err.status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    // Criterion: any other spawn failure maps to a 500 carrying the OS message.
    #[test]
    fn test_spotify_spawn_error_maps_to_internal_error() {
        let err: AppError = SpotifyError::Spawn("permission denied".to_string()).into();
        assert_eq!(err.status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    // Criterion (phase 5.2): a transport call while Disconnected maps to 409.
    #[test]
    fn test_spotify_api_not_connected_maps_to_conflict() {
        let err: AppError = SpotifyApiError::NotConnected.into();
        assert_eq!(err.status, StatusCode::CONFLICT);
    }

    // Criterion (phase 5.2): a token exchange failure maps to 502 (Bad Gateway).
    #[test]
    fn test_spotify_api_exchange_failure_maps_to_bad_gateway() {
        let err: AppError = SpotifyApiError::Exchange("invalid code".to_string()).into();
        assert_eq!(err.status, StatusCode::BAD_GATEWAY);
    }

    // Criterion (phase 5.2): a Premium-required rejection maps to 403 (Forbidden).
    #[test]
    fn test_spotify_api_premium_required_maps_to_forbidden() {
        let err: AppError = SpotifyApiError::PremiumRequired.into();
        assert_eq!(err.status, StatusCode::FORBIDDEN);
    }
}
