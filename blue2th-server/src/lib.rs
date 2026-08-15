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
    AdapterInfo, AuthCallbackRequest, AuthUrlResponse, ClientPresence, ConfigRequest, DeviceInfo,
    HealthStatus, OffsetRequest, PlaybackState, PlaybackStatus, PresenceRequest, ServerConfig,
    SpeakerTarget, SpotifyAuthState, SpotifyState, SpotifyStatus, TargetsState, VolumeRequest,
};
use futures::{Stream, StreamExt};
use tokio::sync::Mutex;
use tower_http::cors::CorsLayer;
use tracing_subscriber::EnvFilter;

pub mod audio;
mod bluetooth;
pub mod config;
pub mod spotify;
pub mod spotify_auth;
mod state_store;
pub mod targets;
pub mod watchdog;

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
    /// Reader count for the now-playing SSE feed. The app holds that stream open
    /// for as long as it runs, so losing every reader means the phone is gone —
    /// the watchdog then pauses playback (see `watchdog`).
    sse_watch: Arc<watchdog::SseWatch>,
    /// The backend's own name, pushed by the app. It is the Spotify Connect
    /// device name `librespot` advertises *and* the name the Web API lookup
    /// matches on, so the two can never disagree.
    name: Arc<Mutex<config::ServerName>>,
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
    // The env-built driver restores the persisted refresh token, so a restart
    // keeps the user logged in; the store-backed selection restores each
    // speaker's tuned offset the same way.
    app_with_auth_and_targets(
        SpotifyAuth::new(),
        SpeakerTargets::with_store(targets::offsets_store_path()),
        // Reloaded from disk so the Web API lookup keeps matching the running
        // librespot even before the app talks to us again.
        config::ServerName::with_store(config::name_store_path()),
    )
}

/// Build the router around an explicit Spotify auth driver. Tests use this with
/// `SpotifyAuth::with_config`, which never touches the on-disk token store — so a
/// test run can neither read nor clobber the real user's credential. The selection
/// is store-free for the same reason.
pub fn app_with_auth(spotify_auth: SpotifyAuth) -> Router {
    app_with_auth_and_targets(
        spotify_auth,
        SpeakerTargets::new(),
        config::ServerName::new(),
    )
}

/// Build the router around an explicit Spotify auth driver and an explicit
/// playback selection, so the on-disk seams stay in the caller's hands.
fn app_with_auth_and_targets(
    mut spotify_auth: SpotifyAuth,
    speaker_targets: SpeakerTargets,
    server_name: config::ServerName,
) -> Router {
    // The auth driver and the subprocess must start out agreeing with the stored
    // name, or the very first transport call would look up a device nobody
    // advertises.
    spotify_auth.set_device_name(server_name.name());
    let spotify = SpotifyBackend::with_name(server_name.name());
    let state = AppState {
        // Real playback output (rodio → PipeWire); the device is opened lazily on
        // the first `/play`, so building the router stays cheap and CI-safe.
        engine: Arc::new(Mutex::new(AudioEngine::with_output(Box::new(
            RodioOutput::new(),
        )))),
        targets: Arc::new(Mutex::new(speaker_targets)),
        connected: Arc::new(Mutex::new(Vec::new())),
        spotify: Arc::new(Mutex::new(spotify)),
        spotify_auth: Arc::new(Mutex::new(spotify_auth)),
        sse_watch: Arc::new(watchdog::SseWatch::default()),
        name: Arc::new(Mutex::new(server_name)),
    };

    spawn_idle_watchdog(state.clone());

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
        .route("/client/presence", post(client_presence))
        .route("/config", get(get_config).post(set_config))
        // Permissive CORS for LAN development; tightened in a later phase.
        .layer(CorsLayer::permissive())
        .with_state(state)
}

/// Start the idle watchdog: once the now-playing SSE feed has had no reader for
/// longer than the grace period its presence allows, pause Spotify.
///
/// The app reports `Gone` when it closes, which pauses immediately; this covers
/// what that report cannot — a crash, an OOM kill, a dropped network — where the
/// PC would otherwise keep streaming to nobody.
fn spawn_idle_watchdog(state: AppState) {
    // A router built outside an async context (a bare unit test) has no runtime to
    // spawn on; the watchdog is a safety net, never a requirement.
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(watchdog::WATCHDOG_TICK).await;
            // Only worth anything while our own Connect backend is up: with
            // librespot stopped there is nothing of ours playing to pause.
            let running = {
                let mut spotify = state.spotify.lock().await;
                spotify.poll_liveness().status == SpotifyStatus::Running
            };
            // A backgrounded app is frozen by Android, so its feed drops without
            // the user having left: the grace period follows what the app reported.
            let grace = watchdog::grace_for(state.sse_watch.presence());
            if !running || !state.sse_watch.claim_idle_pause(grace) {
                continue;
            }
            tracing::info!("no now-playing reader for {grace:?}: pausing Spotify");
            let mut auth = state.spotify_auth.lock().await;
            if let Err(e) = auth.transport(Transport::Pause).await {
                // Nothing playing, or no login: not worth more than a trace.
                tracing::warn!("idle watchdog could not pause Spotify: {e}");
            }
        }
    });
}

/// `POST /play` — start (or resume) playback of the embedded test file, routed
/// per the current target selection: one speaker uses the single-sink path, two
/// build a PipeWire combined sink. An empty selection (`Idle`) is rejected (4xx).
async fn play(State(state): State<AppState>) -> Result<Json<PlaybackState>, AppError> {
    // Snapshot the selection and release the guard before the blocking PipeWire calls.
    let speakers = state.targets.lock().await.speakers();
    // Combined sink for fan-out or any non-zero offset, direct single-sink route
    // otherwise; an empty selection is rejected.
    audio::route_for_targets(&speakers)?;
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
/// server-side until the callback. Returns 503 when no client id is configured.
async fn spotify_auth_url(
    State(state): State<AppState>,
) -> Result<Json<AuthUrlResponse>, AppError> {
    let (url, csrf) = state.spotify_auth.lock().await.authorize_url()?;
    Ok(Json(AuthUrlResponse { url, state: csrf }))
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
    let guard = state.sse_watch.subscribe();
    let stream = async_stream::stream! {
        // Held for the stream's lifetime: whichever way the stream ends (client
        // gone, task cancelled), dropping it starts the idle clock.
        let _guard = guard;
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

/// `POST /client/presence` — the app reports whether it is on screen, backgrounded
/// or closing. The backend cannot tell a frozen app from a dead one on its own, so
/// this drives the watchdog's grace period; `Gone` pauses playback straight away.
async fn client_presence(
    State(state): State<AppState>,
    Json(req): Json<PresenceRequest>,
) -> StatusCode {
    // Logged: this is the only visible trace that the app's lifecycle hooks are
    // reaching the backend at all (Android does not guarantee `onDestroy`).
    tracing::info!("client presence: {:?}", req.presence);
    state.sse_watch.set_presence(req.presence);
    if req.presence == ClientPresence::Gone {
        pause_spotify_now(&state).await;
    }
    StatusCode::NO_CONTENT
}

/// Pause the Spotify Web API playback, best-effort. Used when the app reports it
/// is closing: nothing here is worth failing that report over, and the backend
/// answers 409 when there is nothing to pause anyway.
async fn pause_spotify_now(state: &AppState) {
    let running = {
        let mut spotify = state.spotify.lock().await;
        spotify.poll_liveness().status == SpotifyStatus::Running
    };
    if !running {
        return;
    }
    let mut auth = state.spotify_auth.lock().await;
    if let Err(e) = auth.transport(Transport::Pause).await {
        tracing::warn!("could not pause Spotify on client exit: {e}");
    }
}

/// `GET /config` — the backend's current name.
async fn get_config(State(state): State<AppState>) -> Json<ServerConfig> {
    let stored = state.name.lock().await;
    Json(ServerConfig {
        name: stored.name().to_string(),
        restore_during_playback: stored.restore_during_playback(),
    })
}

/// `POST /config` — set the backend's name, which becomes its Spotify Connect
/// device name.
///
/// The name is re-validated here rather than trusted: this route is reachable by
/// anything on the LAN until the authenticated API lands.
async fn set_config(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<Json<ServerConfig>, AppError> {
    // Parsed leniently so a malformed body is a 400 rather than Axum's 422.
    let req: ConfigRequest = serde_json::from_slice(&body)
        .map_err(|e| AppError::bad_request(format!("invalid config body: {e}")))?;
    let name = {
        let mut stored = state.name.lock().await;
        let name = stored
            .set_name(&req.name)
            .map_err(|e| AppError::bad_request(e.to_string()))?;
        // Applied only once the name was accepted, so a rejected body changes
        // nothing at all.
        stored.set_restore_during_playback(req.restore_during_playback);
        name
    };

    // The Web API lookup must follow the advertised name, or transport would 412
    // while blaming the user for not starting the backend.
    state.spotify_auth.lock().await.set_device_name(&name);

    let speakers = state.targets.lock().await.speakers();
    let mut spotify = state.spotify.lock().await;
    // `--name` is fixed at spawn: a running backend has to be respawned to be
    // renamed, which briefly drops the Connect device.
    let restart = spotify::should_restart_for_rename(
        spotify.poll_liveness().status,
        spotify.device_name(),
        &name,
    );
    spotify.set_device_name(&name);
    if restart {
        // Both halves are logged rather than propagated: the name *is* stored, so
        // a rename must not report failure because the subprocess dance did.
        if let Err(e) = spotify.stop() {
            tracing::warn!("could not stop the Spotify backend before a rename: {e}");
        }
        if let Err(e) = spotify.start(&speakers) {
            tracing::warn!("could not restart the Spotify backend after a rename: {e}");
        }
    }

    Ok(Json(ServerConfig {
        name,
        restore_during_playback: req.restore_during_playback,
    }))
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

    // Whether restoring right now is allowed: mid-playback it is opt-in, since
    // moving the target sink respawns `librespot` and cuts the sound.
    let playing = {
        let mut engine = state.engine.lock().await;
        engine.poll_state().status == PlaybackStatus::Playing
    } || {
        let mut spotify = state.spotify.lock().await;
        spotify.poll_liveness().status == SpotifyStatus::Running
    };
    let allowed =
        targets::should_restore(playing, state.name.lock().await.restore_during_playback());

    let changed = {
        let mut targets = state.targets.lock().await;
        targets.retain_connected(&connected);
        // `restore` reports whether the selection really moved. This runs on
        // every `/devices` poll, so re-routing unconditionally would tear the
        // PipeWire graph down and rebuild it every couple of seconds.
        allowed && targets.restore(&connected)
    };
    if changed {
        let speakers = state.targets.lock().await.speakers();
        apply_selection_change(state, &speakers).await;
    }
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
    let (speakers, updated) = {
        let mut targets = state.targets.lock().await;
        targets.select(&addr, &connected)?;
        (targets.speakers(), targets.state())
    };
    // Symmetric with deselect: a speaker added mid-playback must be brought into
    // the routing, not just into the stored selection.
    apply_selection_change(&state, &speakers).await;
    Ok(Json(updated))
}

/// `POST /devices/{addr}/deselect` — drop a speaker from the target selection.
async fn deselect_target(
    State(state): State<AppState>,
    Path(addr): Path<String>,
) -> Json<TargetsState> {
    let (speakers, updated) = {
        let mut targets = state.targets.lock().await;
        targets.deselect(&addr);
        (targets.speakers(), targets.state())
    };
    // Dropping a speaker must actually stop the audio reaching it, not just
    // update the selection.
    apply_selection_change(&state, &speakers).await;
    Json(updated)
}

/// `POST /devices/{addr}/offset` — set a target speaker's latency offset (clamped
/// server-side to `0..=750` ms). A no-op if the speaker is not selected.
async fn set_target_offset(
    State(state): State<AppState>,
    Path(addr): Path<String>,
    Json(req): Json<OffsetRequest>,
) -> Json<TargetsState> {
    let (speakers, updated) = {
        let mut targets = state.targets.lock().await;
        targets.set_offset(&addr, req.offset_ms);
        (targets.speakers(), targets.state())
    };
    apply_offset_live(&state, &addr, &speakers).await;
    Json(updated)
}

/// Make a just-changed offset audible without replaying: the offset only exists
/// as `module-loopback` latency, so it has to be pushed into the live PipeWire
/// graph. Best-effort — a failure here must not turn a slider drag into an error,
/// and the new value is applied anyway on the next `/play` or Spotify start.
async fn apply_offset_live(state: &AppState, addr: &str, speakers: &[SpeakerTarget]) {
    let Some(target) = speakers.iter().find(|s| s.address == addr) else {
        return;
    };
    let plan = audio::combine_sink_plan(speakers);

    // Moving a lone speaker off (or onto) a zero offset switches it between the
    // direct route and the combined sink, which needs a respawn; a plain latency
    // change does not, and is retuned in place below.
    if resync_spotify_sink(state, speakers).await {
        return;
    }

    if audio::combined_sink_exists(&plan.sink_name) {
        let branch = audio::CombineBranch {
            sink: audio::bluez_sink_prefix(&target.address),
            latency_ms: target.offset_ms,
        };
        if let Err(e) = audio::retune_combined_branch(&plan.sink_name, &branch) {
            tracing::warn!("could not retune the speaker offset live: {e}");
        }
    }
}

/// Respawn `librespot` when the selection moves it to a different sink.
/// `--device` is fixed at spawn, so re-routing alone would leave it feeding the
/// sink it was started with. Returns whether it was restarted.
async fn resync_spotify_sink(state: &AppState, speakers: &[SpeakerTarget]) -> bool {
    let mut spotify = state.spotify.lock().await;
    if spotify.poll_liveness().status != SpotifyStatus::Running {
        return false;
    }
    let wanted = spotify::spotify_target_sink(speakers);
    if spotify.current_sink() == Some(wanted.as_str()) {
        return false;
    }
    let _ = spotify.stop();
    if let Err(e) = spotify.start(speakers) {
        tracing::warn!("could not restart the Spotify backend after a routing change: {e}");
    }
    true
}

/// Push a selection change into the live audio graph.
///
/// Selecting or deselecting a speaker used to only update the stored selection:
/// the PipeWire routing stayed exactly as it was, so a speaker dropped from the
/// selection kept receiving the stream and playing on.
async fn apply_selection_change(state: &AppState, speakers: &[SpeakerTarget]) {
    if speakers.is_empty() {
        // Nothing left to play to. Pause both sources, then tear the combined
        // sink down so no loopback keeps feeding a speaker nobody selected.
        pause_spotify_now(state).await;
        {
            let mut engine = state.engine.lock().await;
            if let Err(e) = engine.pause() {
                tracing::warn!("could not pause playback after the last speaker was dropped: {e}");
            }
        }
        if let Err(e) = audio::teardown_combined(spotify::COMBINED_SINK_NAME) {
            tracing::warn!("could not tear the combined sink down: {e}");
        }
        return;
    }
    // Still a target: rebuild the routing so it spans exactly the current
    // selection (this is what stops feeding a speaker that was just dropped).
    if let Err(e) = audio::route_for_targets(speakers) {
        tracing::warn!("could not re-route after a selection change: {e}");
        return;
    }
    resync_spotify_sink(state, speakers).await;
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
            // Server misconfiguration (no client id): nothing the app can fix.
            SpotifyApiError::NotConfigured => StatusCode::SERVICE_UNAVAILABLE,
            // Transport while Disconnected: a precondition conflict, not a bug.
            SpotifyApiError::NotConnected => StatusCode::CONFLICT,
            // The librespot backend must be started before targeting blue2th-PC.
            SpotifyApiError::BackendNotRunning => StatusCode::PRECONDITION_FAILED,
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
    use blue2th_proto::RoutingMode;
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

    // Criterion (phase 6.1): `app()` builds its selection from the remembered
    // offsets store and still serves `/targets`. Only the offsets are persisted,
    // never the selection: a freshly built router reports nothing selected, since
    // at startup no speaker is connected yet.
    #[tokio::test]
    async fn test_app_builds_with_the_offsets_store_and_restores_no_selection() {
        let request = Request::builder()
            .uri("/targets")
            .body(Body::empty())
            .expect("build request");

        let response = app().oneshot(request).await.expect("router response");
        assert_eq!(response.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let state: TargetsState = serde_json::from_slice(&bytes).expect("parse TargetsState");
        assert!(
            state.speakers.is_empty(),
            "the selection itself must never be restored, got {:?}",
            state.speakers
        );
        assert_eq!(state.routing, RoutingMode::Idle);
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
