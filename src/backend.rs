// Copyright 2026 Blue2th
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Thin HTTP client for the blue2th PC backend (see `docs/ROADMAP.md`).
//!
//! Phase 0: resolve the backend base URL and ping `GET /health`. Real feature
//! calls (scan, connect, play…) land in later phases.

use std::time::Duration;

use blue2th_proto::{
    AuthCallbackRequest, AuthUrlResponse, ClientPresence, ConfigRequest, DeviceInfo, HealthStatus,
    NowPlaying, OffsetRequest, PlaybackState, PresenceRequest, ServerConfig, SpotifyAuthState,
    SpotifyState, TargetsState, VolumeRequest,
};
use futures::StreamExt;

use crate::settings::AppSettings;

/// How long the app keeps reading the `/scan` SSE feed before stopping. The
/// backend caps discovery on its side too; this is the client-side window.
const SCAN_WINDOW: Duration = Duration::from_secs(8);

/// Error talking to the backend; surfaced to the UI as a string.
#[derive(Debug, Clone)]
pub struct BackendError(String);

impl BackendError {
    fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Message carried by every call made while no backend is configured. The app
/// fails fast with it instead of guessing an address and timing out.
pub const NO_BACKEND_CONFIGURED: &str = "no backend configured";

/// The active backend's base URL, resolved at **runtime** from the app settings.
///
/// There is no compile-time address, no seeded default, not even a localhost
/// fallback: an unconfigured app must attempt no network call at all, so this
/// returns an error and every call site propagates it with `?`.
pub fn backend_base_url() -> Result<String, BackendError> {
    base_url_from(&crate::settings::current())
}

/// Resolve the base URL from an explicit settings snapshot (pure, testable).
fn base_url_from(settings: &AppSettings) -> Result<String, BackendError> {
    settings
        .active_url()
        .ok_or_else(|| BackendError::new(NO_BACKEND_CONFIGURED))
}

/// Build the `{base}/config` URL, tolerating a trailing slash on the base.
fn config_url(base: &str) -> String {
    format!("{}/config", base.trim_end_matches('/'))
}

/// `GET {base}/config` — the name the active backend currently holds.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub async fn get_config() -> Result<ServerConfig, BackendError> {
    let url = config_url(&backend_base_url()?);
    let response = reqwest::get(&url)
        .await
        .map_err(|e| BackendError::new(describe(&e)))?;
    backend_error_message(response)
        .await?
        .json::<ServerConfig>()
        .await
        .map_err(|e| BackendError::new(describe(&e)))
}

/// `POST {base}/config` — push the app's name for the active backend, which
/// adopts it as its Spotify Connect device name.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub async fn set_config(name: &str) -> Result<ServerConfig, BackendError> {
    let url = config_url(&backend_base_url()?);
    let response = reqwest::Client::new()
        .post(&url)
        .json(&ConfigRequest {
            name: name.to_string(),
        })
        .send()
        .await
        .map_err(|e| BackendError::new(describe(&e)))?;
    backend_error_message(response)
        .await?
        .json::<ServerConfig>()
        .await
        .map_err(|e| BackendError::new(describe(&e)))
}

/// `POST {base}/spotify/pause` against an explicit address — used to quieten the
/// backend being left behind, which is no longer the one `backend_base_url()`
/// resolves to.
async fn pause_at(base: &str) -> Result<(), BackendError> {
    let url = format!("{}/spotify/pause", base.trim_end_matches('/'));
    let response = reqwest::Client::new()
        .post(&url)
        .send()
        .await
        .map_err(|e| BackendError::new(describe(&e)))?;
    backend_error_message(response).await?;
    Ok(())
}

/// Switch the active backend: pause the previous one (best-effort), repoint the
/// app, and push the new backend's name to it.
///
/// The settings page and the status-encart quick switch must both go through
/// this, so the two ways to switch cannot drift apart. The local switch always
/// happens: a failure talking to either backend is surfaced, never blocking —
/// the app must never be stuck on a dead backend.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub async fn activate_backend(
    settings: &mut AppSettings,
    index: usize,
) -> Result<(), BackendError> {
    // Captured before the switch: afterwards this address is no longer the one
    // the app resolves, and it is the one that must be quietened.
    let previous = settings.active_url();

    // Switch locally first, and persist: a slow or dead backend must never hold
    // the app on a target the user has left.
    settings
        .activate(index)
        .map_err(|e| BackendError::new(e.to_string()))?;
    // Owned copy: the cache keeps its own settings beyond this borrow.
    crate::settings::set_current(settings.clone());

    // Both remote steps are best-effort and independent; the last failure is
    // surfaced so the toast says something, but neither undoes the switch.
    let mut failure = None;
    if let Some(base) = previous {
        if let Err(e) = pause_at(&base).await {
            failure = Some(e);
        }
    }
    if let Some(name) = settings.active_backend().map(|b| b.name.clone()) {
        if let Err(e) = set_config(&name).await {
            failure = Some(e);
        }
    }
    match failure {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// `GET {base}/health` against an explicit address — the settings page's `Test`
/// action, which pings a backend that is not (yet) the active one.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub async fn test_backend(url: &str) -> Result<HealthStatus, BackendError> {
    let response = reqwest::get(&health_url(url))
        .await
        .map_err(|e| BackendError::new(describe(&e)))?;
    backend_error_message(response)
        .await?
        .json::<HealthStatus>()
        .await
        .map_err(|e| BackendError::new(describe(&e)))
}

/// Build the `/health` URL from a base, tolerating a trailing slash.
fn health_url(base: &str) -> String {
    format!("{}/health", base.trim_end_matches('/'))
}

/// Flatten a `reqwest::Error` and its source chain into one string, so the
/// on-device UI shows the *underlying* cause (e.g. "Connection refused" vs
/// "CLEARTEXT communication not permitted") instead of just "error sending request".
fn describe(err: &reqwest::Error) -> String {
    use std::error::Error as _;
    let mut msg = err.to_string();
    let mut source = err.source();
    while let Some(e) = source {
        msg.push_str(" -> ");
        msg.push_str(&e.to_string());
        source = e.source();
    }
    msg
}

/// `GET {base}/health` and decode the backend's `HealthStatus`.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub async fn ping_backend() -> Result<HealthStatus, BackendError> {
    let url = health_url(&backend_base_url()?);
    let response = reqwest::get(&url)
        .await
        .map_err(|e| BackendError::new(describe(&e)))?;
    response
        .error_for_status()
        .map_err(|e| BackendError::new(describe(&e)))?
        .json::<HealthStatus>()
        .await
        .map_err(|e| BackendError::new(describe(&e)))
}

/// Run a backend scan: consume the `/scan` SSE feed for `SCAN_WINDOW`, collecting
/// each discovered device (deduplicated by address).
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub async fn scan_devices() -> Result<Vec<DeviceInfo>, BackendError> {
    let url = format!("{}/scan", backend_base_url()?.trim_end_matches('/'));
    let response = reqwest::get(&url)
        .await
        .map_err(|e| BackendError::new(describe(&e)))?
        .error_for_status()
        .map_err(|e| BackendError::new(describe(&e)))?;

    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut found: Vec<DeviceInfo> = Vec::new();

    let collect = async {
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| BackendError::new(describe(&e)))?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            // SSE events are separated by a blank line.
            while let Some(pos) = buffer.find("\n\n") {
                let block: String = buffer.drain(..pos + 2).collect();
                if let Some(json) = sse_device_payload(&block) {
                    if let Ok(device) = serde_json::from_str::<DeviceInfo>(&json) {
                        if !found.iter().any(|d| d.address == device.address) {
                            found.push(device);
                        }
                    }
                }
            }
        }
        Ok::<(), BackendError>(())
    };

    // Stop after the window even if the server keeps the stream open.
    let _ = tokio::time::timeout(SCAN_WINDOW, collect).await;
    Ok(found)
}

/// `POST {base}/devices/{address}/connect` — pair/trust/connect on the backend,
/// returning the device's updated state.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub async fn connect_device(address: &str) -> Result<DeviceInfo, BackendError> {
    post_device_action(address, "connect").await
}

/// `POST {base}/devices/{address}/disconnect` — disconnect on the backend,
/// returning the device's updated state.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub async fn disconnect_device(address: &str) -> Result<DeviceInfo, BackendError> {
    post_device_action(address, "disconnect").await
}

/// POST `{base}/devices/{address}/{action}` and decode the updated `DeviceInfo`.
async fn post_device_action(address: &str, action: &str) -> Result<DeviceInfo, BackendError> {
    let url = format!(
        "{}/devices/{address}/{action}",
        backend_base_url()?.trim_end_matches('/')
    );
    reqwest::Client::new()
        .post(&url)
        .send()
        .await
        .map_err(|e| BackendError::new(describe(&e)))?
        .error_for_status()
        .map_err(|e| BackendError::new(describe(&e)))?
        .json::<DeviceInfo>()
        .await
        .map_err(|e| BackendError::new(describe(&e)))
}

/// `GET {base}/devices` — the backend's paired devices and their current state.
/// Used by the periodic poll to refresh `connected`/`rssi` without re-scanning.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub async fn fetch_devices() -> Result<Vec<DeviceInfo>, BackendError> {
    let url = format!("{}/devices", backend_base_url()?.trim_end_matches('/'));
    reqwest::get(&url)
        .await
        .map_err(|e| BackendError::new(describe(&e)))?
        .error_for_status()
        .map_err(|e| BackendError::new(describe(&e)))?
        .json::<Vec<DeviceInfo>>()
        .await
        .map_err(|e| BackendError::new(describe(&e)))
}

/// `POST {base}/play` — start (or resume) playback on the backend, returning the
/// new playback state.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub async fn play() -> Result<PlaybackState, BackendError> {
    post_transport("play").await
}

/// `POST {base}/pause` — pause playback, returning the new state.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub async fn pause() -> Result<PlaybackState, BackendError> {
    post_transport("pause").await
}

/// `POST {base}/stop` — stop playback, returning the new state.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub async fn stop() -> Result<PlaybackState, BackendError> {
    post_transport("stop").await
}

/// `GET {base}/playback` — the backend's current playback state.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub async fn playback_state() -> Result<PlaybackState, BackendError> {
    let url = format!("{}/playback", backend_base_url()?.trim_end_matches('/'));
    reqwest::get(&url)
        .await
        .map_err(|e| BackendError::new(describe(&e)))?
        .error_for_status()
        .map_err(|e| BackendError::new(describe(&e)))?
        .json::<PlaybackState>()
        .await
        .map_err(|e| BackendError::new(describe(&e)))
}

/// `POST {base}/volume` — set the connected speaker's PipeWire sink volume
/// (clamped server-side to `0.0..=1.0`), returning the new state.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub async fn set_volume(level: f32) -> Result<PlaybackState, BackendError> {
    let url = format!("{}/volume", backend_base_url()?.trim_end_matches('/'));
    reqwest::Client::new()
        .post(&url)
        .json(&VolumeRequest { level })
        .send()
        .await
        .map_err(|e| BackendError::new(describe(&e)))?
        .error_for_status()
        .map_err(|e| BackendError::new(describe(&e)))?
        .json::<PlaybackState>()
        .await
        .map_err(|e| BackendError::new(describe(&e)))
}

/// POST `{base}/{action}` (no body) and decode the updated `PlaybackState`.
async fn post_transport(action: &str) -> Result<PlaybackState, BackendError> {
    let url = format!("{}/{action}", backend_base_url()?.trim_end_matches('/'));
    reqwest::Client::new()
        .post(&url)
        .send()
        .await
        .map_err(|e| BackendError::new(describe(&e)))?
        .error_for_status()
        .map_err(|e| BackendError::new(describe(&e)))?
        .json::<PlaybackState>()
        .await
        .map_err(|e| BackendError::new(describe(&e)))
}

/// Build the `{base}/devices/{address}/{action}` URL for a target action
/// (`select`/`deselect`/`offset`), tolerating a trailing slash on the base.
fn device_action_url(base: &str, address: &str, action: &str) -> String {
    format!("{}/devices/{address}/{action}", base.trim_end_matches('/'))
}

/// Build the `{base}/targets` URL, tolerating a trailing slash on the base.
fn targets_url(base: &str) -> String {
    format!("{}/targets", base.trim_end_matches('/'))
}

/// `POST {base}/devices/{address}/select` — select a connected speaker as a
/// playback target, returning the updated selection state.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub async fn select_target(address: &str) -> Result<TargetsState, BackendError> {
    let url = device_action_url(&backend_base_url()?, address, "select");
    reqwest::Client::new()
        .post(&url)
        .send()
        .await
        .map_err(|e| BackendError::new(describe(&e)))?
        .error_for_status()
        .map_err(|e| BackendError::new(describe(&e)))?
        .json::<TargetsState>()
        .await
        .map_err(|e| BackendError::new(describe(&e)))
}

/// `POST {base}/devices/{address}/deselect` — drop a speaker from the playback
/// target selection, returning the updated selection state.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub async fn deselect_target(address: &str) -> Result<TargetsState, BackendError> {
    let url = device_action_url(&backend_base_url()?, address, "deselect");
    reqwest::Client::new()
        .post(&url)
        .send()
        .await
        .map_err(|e| BackendError::new(describe(&e)))?
        .error_for_status()
        .map_err(|e| BackendError::new(describe(&e)))?
        .json::<TargetsState>()
        .await
        .map_err(|e| BackendError::new(describe(&e)))
}

/// `POST {base}/devices/{address}/offset` — set a target speaker's latency offset
/// (clamped server-side to `0..=750` ms), returning the updated selection state.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub async fn set_offset(address: &str, offset_ms: u32) -> Result<TargetsState, BackendError> {
    let url = device_action_url(&backend_base_url()?, address, "offset");
    reqwest::Client::new()
        .post(&url)
        .json(&OffsetRequest { offset_ms })
        .send()
        .await
        .map_err(|e| BackendError::new(describe(&e)))?
        .error_for_status()
        .map_err(|e| BackendError::new(describe(&e)))?
        .json::<TargetsState>()
        .await
        .map_err(|e| BackendError::new(describe(&e)))
}

/// `GET {base}/targets` — the backend's current playback-target selection,
/// per-speaker offsets and routing mode.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub async fn fetch_targets() -> Result<TargetsState, BackendError> {
    let url = targets_url(&backend_base_url()?);
    reqwest::get(&url)
        .await
        .map_err(|e| BackendError::new(describe(&e)))?
        .error_for_status()
        .map_err(|e| BackendError::new(describe(&e)))?
        .json::<TargetsState>()
        .await
        .map_err(|e| BackendError::new(describe(&e)))
}

/// Build the `{base}/spotify/{action}` URL for a Spotify backend action
/// (`start`/`stop`/`status`), tolerating a trailing slash on the base.
fn spotify_url(base: &str, action: &str) -> String {
    format!("{}/spotify/{action}", base.trim_end_matches('/'))
}

/// `POST {base}/spotify/start` — activate the Spotify source backend (spawn the
/// `librespot` Connect device), returning its new state.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub async fn start_spotify() -> Result<SpotifyState, BackendError> {
    post_spotify("start").await
}

/// `POST {base}/spotify/stop` — deactivate the Spotify source backend (kill the
/// subprocess), returning its new state.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub async fn stop_spotify() -> Result<SpotifyState, BackendError> {
    post_spotify("stop").await
}

/// `GET {base}/spotify/status` — the Spotify backend's current state.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub async fn spotify_status() -> Result<SpotifyState, BackendError> {
    let url = spotify_url(&backend_base_url()?, "status");
    reqwest::get(&url)
        .await
        .map_err(|e| BackendError::new(describe(&e)))?
        .error_for_status()
        .map_err(|e| BackendError::new(describe(&e)))?
        .json::<SpotifyState>()
        .await
        .map_err(|e| BackendError::new(describe(&e)))
}

/// POST `{base}/spotify/{action}` (no body) and decode the updated `SpotifyState`.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
async fn post_spotify(action: &str) -> Result<SpotifyState, BackendError> {
    let url = spotify_url(&backend_base_url()?, action);
    reqwest::Client::new()
        .post(&url)
        .send()
        .await
        .map_err(|e| BackendError::new(describe(&e)))?
        .error_for_status()
        .map_err(|e| BackendError::new(describe(&e)))?
        .json::<SpotifyState>()
        .await
        .map_err(|e| BackendError::new(describe(&e)))
}

/// Build the `{base}/spotify/now-playing` SSE URL, tolerating a trailing slash.
fn now_playing_url(base: &str) -> String {
    format!("{}/spotify/now-playing", base.trim_end_matches('/'))
}

/// Surface the backend's own message for a failed response. `AppError` replies
/// with a plain-text body ("Spotify client id not configured — …", "start the
/// Spotify backend first"), which `error_for_status` would throw away, leaving
/// the user with a bare "503 Service Unavailable" on the phone.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
async fn backend_error_message(
    response: reqwest::Response,
) -> Result<reqwest::Response, BackendError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    let message = body.trim();
    Err(BackendError::new(if message.is_empty() {
        status.to_string()
    } else {
        message.to_string()
    }))
}

/// `GET {base}/spotify/auth/url` — ask the backend for a Spotify authorize URL
/// (PKCE) and the CSRF `state` to echo back on callback.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub async fn spotify_auth_url() -> Result<AuthUrlResponse, BackendError> {
    let url = spotify_url(&backend_base_url()?, "auth/url");
    let response = reqwest::get(&url)
        .await
        .map_err(|e| BackendError::new(describe(&e)))?;
    backend_error_message(response)
        .await?
        .json::<AuthUrlResponse>()
        .await
        .map_err(|e| BackendError::new(describe(&e)))
}

/// `POST {base}/spotify/auth/callback` — hand the backend the authorization
/// `code` (and CSRF `state`) captured from the custom-scheme redirect.
///
/// Invoked from the root deep-link poll in `App`, which consumes the
/// redirect through `deep_link::take_pending_deep_link`.
pub async fn spotify_auth_callback(
    code: &str,
    state: &str,
) -> Result<SpotifyAuthState, BackendError> {
    let url = spotify_url(&backend_base_url()?, "auth/callback");
    let response = reqwest::Client::new()
        .post(&url)
        .json(&AuthCallbackRequest {
            code: code.to_string(),
            state: state.to_string(),
        })
        .send()
        .await
        .map_err(|e| BackendError::new(describe(&e)))?;
    backend_error_message(response)
        .await?
        .json::<SpotifyAuthState>()
        .await
        .map_err(|e| BackendError::new(describe(&e)))
}

/// `GET {base}/spotify/auth/status` — the current auth state (Connected/Disconnected).
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub async fn spotify_auth_status() -> Result<SpotifyAuthState, BackendError> {
    let url = spotify_url(&backend_base_url()?, "auth/status");
    reqwest::get(&url)
        .await
        .map_err(|e| BackendError::new(describe(&e)))?
        .error_for_status()
        .map_err(|e| BackendError::new(describe(&e)))?
        .json::<SpotifyAuthState>()
        .await
        .map_err(|e| BackendError::new(describe(&e)))
}

/// `POST {base}/client/presence` — tell the backend whether the app is on screen,
/// backgrounded or closing.
///
/// The backend cannot infer this: Android freezes a backgrounded app, so its
/// dropped SSE feed looks exactly like a phone that is gone. Reporting keeps a
/// background listening session alive and pauses at once on a real exit.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub async fn report_presence(presence: ClientPresence) -> Result<(), BackendError> {
    let url = format!(
        "{}/client/presence",
        backend_base_url()?.trim_end_matches('/')
    );
    let response = reqwest::Client::new()
        .post(&url)
        .json(&PresenceRequest { presence })
        .send()
        .await
        .map_err(|e| BackendError::new(describe(&e)))?;
    backend_error_message(response).await?;
    Ok(())
}

/// A Spotify transport action, so the UI can carry one in a prop instead of a
/// stringly-typed path.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SpotifyAction {
    /// Skip to the previous track.
    Previous,
    /// Resume playback.
    Play,
    /// Pause playback.
    Pause,
    /// Skip to the next track.
    Next,
}

impl SpotifyAction {
    /// The `/spotify/{…}` path segment this action posts to.
    fn path(self) -> &'static str {
        match self {
            SpotifyAction::Previous => "previous",
            SpotifyAction::Play => "play",
            SpotifyAction::Pause => "pause",
            SpotifyAction::Next => "next",
        }
    }
}

/// `POST {base}/spotify/{action}` — drive playback through the Web API, which the
/// server applies to the `blue2th-PC` Connect device.
pub async fn spotify_transport(action: SpotifyAction) -> Result<(), BackendError> {
    post_spotify_transport(action.path()).await
}

/// POST `{base}/spotify/{action}` (no body) for a transport action; the backend
/// replies 204 (no content) on success, so no body is decoded.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
async fn post_spotify_transport(action: &str) -> Result<(), BackendError> {
    let url = spotify_url(&backend_base_url()?, action);
    let response = reqwest::Client::new()
        .post(&url)
        .send()
        .await
        .map_err(|e| BackendError::new(describe(&e)))?;
    backend_error_message(response).await?;
    Ok(())
}

/// Subscribe to the `{base}/spotify/now-playing` SSE feed, invoking `on_event`
/// for each `now-playing` snapshot until the stream ends or the caller drops the
/// future. Errors talking to the backend are surfaced to the caller.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub async fn subscribe_now_playing<F>(mut on_event: F) -> Result<(), BackendError>
where
    F: FnMut(NowPlaying),
{
    let url = now_playing_url(&backend_base_url()?);
    let response = reqwest::get(&url)
        .await
        .map_err(|e| BackendError::new(describe(&e)))?
        .error_for_status()
        .map_err(|e| BackendError::new(describe(&e)))?;

    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| BackendError::new(describe(&e)))?;
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        // SSE events are separated by a blank line.
        while let Some(pos) = buffer.find("\n\n") {
            let block: String = buffer.drain(..pos + 2).collect();
            if let Some(now_playing) = sse_now_playing_payload(&block) {
                on_event(now_playing);
            }
        }
    }
    Ok(())
}

/// Extract and parse the `NowPlaying` payload of a `now-playing` SSE event block,
/// ignoring keep-alive comments and non-`now-playing` events.
fn sse_now_playing_payload(block: &str) -> Option<NowPlaying> {
    let mut is_now_playing = false;
    let mut data: Option<String> = None;
    for line in block.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            is_now_playing = rest.trim() == "now-playing";
        } else if let Some(rest) = line.strip_prefix("data:") {
            data = Some(rest.trim().to_string());
        }
    }
    if is_now_playing {
        data.and_then(|json| serde_json::from_str::<NowPlaying>(&json).ok())
    } else {
        None
    }
}

/// Extract the JSON payload of a `device` SSE event block, ignoring keep-alive
/// comments and non-device events.
fn sse_device_payload(block: &str) -> Option<String> {
    let mut is_device = false;
    let mut data: Option<String> = None;
    for line in block.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            is_device = rest.trim() == "device";
        } else if let Some(rest) = line.strip_prefix("data:") {
            data = Some(rest.trim().to_string());
        }
    }
    if is_device {
        data
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The settings cache is process-wide, so a test that replaces it and one
    /// that asserts "nothing is configured" cannot run at the same time: the
    /// first one's active backend leaks into the second one's assertion. Tests
    /// touching the cache take this lock and leave it empty behind them.
    /// Async-aware on purpose: these tests hold the guard across `await`s, which
    /// a `std::sync::Mutex` must never do.
    static SETTINGS_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[test]
    fn test_health_url_appends_path() {
        assert_eq!(
            health_url("http://10.0.0.5:4000"),
            "http://10.0.0.5:4000/health"
        );
    }

    #[test]
    fn test_health_url_tolerates_trailing_slash() {
        assert_eq!(
            health_url("http://10.0.0.5:4000/"),
            "http://10.0.0.5:4000/health"
        );
    }

    /// Settings holding a single active backend at `url`.
    fn active_at(url: &str) -> AppSettings {
        let mut settings = AppSettings::default();
        settings.add("Salon", url).expect("add the test backend");
        settings.activate(0).expect("activate the test backend");
        settings
    }

    // Criterion (phase 6.2): `backend_base_url()` resolves at runtime from the
    // settings — the active entry's URL, with no compile-time value involved.
    #[test]
    fn test_base_url_from_settings_returns_the_active_backend_url() {
        let settings = active_at("http://192.168.1.107:4000");
        assert_eq!(
            base_url_from(&settings).map_err(|e| e.to_string()),
            Ok("http://192.168.1.107:4000".to_string())
        );
    }

    // Criterion (phase 6.2): with nothing configured the lookup fails fast with a
    // "no backend configured" error — there is no fallback address, not even
    // localhost, so no request can be built at all.
    #[test]
    fn test_base_url_from_settings_without_a_backend_is_an_error() {
        let error = base_url_from(&AppSettings::default())
            .expect_err("an unconfigured app must have no address");
        assert!(
            error.to_string().contains(NO_BACKEND_CONFIGURED),
            "the error must name the missing configuration, got {error}"
        );
    }

    // Criterion (phase 6.2): a configured but inactive backend is still no
    // address — the app only knows the backend the user activated.
    #[test]
    fn test_base_url_from_settings_without_an_active_backend_is_an_error() {
        let mut settings = AppSettings::default();
        settings
            .add("Salon", "http://192.168.1.107:4000")
            .expect("add a backend without activating it");
        assert!(base_url_from(&settings).is_err());
    }

    // Criterion (phase 6.2): every `backend.rs` call goes through the runtime
    // lookup — with nothing configured a call fails fast with the "no backend
    // configured" error rather than performing a request (and timing out).
    #[tokio::test]
    async fn test_ping_backend_without_a_configured_backend_fails_fast() {
        let _guard = SETTINGS_GUARD.lock().await;
        crate::settings::set_current(AppSettings::default());

        let started = std::time::Instant::now();
        let error = ping_backend()
            .await
            .expect_err("an unconfigured app must not reach any backend");
        assert!(
            error.to_string().contains(NO_BACKEND_CONFIGURED),
            "expected a configuration error, got {error}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "no request may be attempted, so the call must return immediately"
        );
    }

    // Criterion (phase 6.2): the app pushes the name over `POST /config` — the
    // client builds the `/config` URL, tolerating a trailing slash on the base.
    #[test]
    fn test_config_url_appends_path() {
        assert_eq!(
            config_url("http://10.0.0.5:4000"),
            "http://10.0.0.5:4000/config"
        );
        assert_eq!(
            config_url("http://10.0.0.5:4000/"),
            "http://10.0.0.5:4000/config"
        );
    }

    // Criterion (phase 6.2): switching backends repoints the app even when the
    // name push fails (the new backend is unreachable) and even when pausing the
    // previous one fails — often *why* the user is switching. The failure is
    // surfaced, never blocking: the app must never be stuck on a dead backend.
    #[tokio::test]
    async fn test_activate_backend_switches_locally_even_when_the_push_fails() {
        let _guard = SETTINGS_GUARD.lock().await;

        let mut settings = AppSettings::default();
        // Port 1 is never listening: both the pause and the push are refused.
        settings
            .add("Salon", "http://127.0.0.1:1")
            .expect("add Salon");
        settings
            .add("Bureau", "http://127.0.0.1:2")
            .expect("add Bureau");
        settings.activate(0).expect("start on Salon");

        let outcome = activate_backend(&mut settings, 1).await;
        assert!(
            outcome.is_err(),
            "an unreachable backend must surface the failure to the toast"
        );
        assert_eq!(
            settings.active_backend().map(|b| b.name.as_str()),
            Some("Bureau"),
            "the switch must still happen locally"
        );

        // Leave the process-wide cache as we found it, as the guard's contract says.
        crate::settings::set_current(AppSettings::default());
    }

    #[test]
    fn test_sse_device_payload_extracts_device_json() {
        let block = "event:device\ndata:{\"address\":\"AA\"}\n\n";
        assert_eq!(
            sse_device_payload(block).as_deref(),
            Some("{\"address\":\"AA\"}")
        );
    }

    #[test]
    fn test_sse_device_payload_ignores_non_device_and_comments() {
        assert_eq!(sse_device_payload("event:error\ndata:boom\n\n"), None);
        assert_eq!(sse_device_payload(": keep-alive\n\n"), None);
    }

    // Criterion: a connected speaker can be selected as a playback target — the
    // client posts to `/devices/{addr}/select`.
    #[test]
    fn test_device_action_url_builds_select_path() {
        assert_eq!(
            device_action_url("http://10.0.0.5:4000", "AA:BB:CC:DD:EE:FF", "select"),
            "http://10.0.0.5:4000/devices/AA:BB:CC:DD:EE:FF/select"
        );
    }

    // Criterion: deselecting removes the speaker — the client posts to
    // `/devices/{addr}/deselect`.
    #[test]
    fn test_device_action_url_builds_deselect_path() {
        assert_eq!(
            device_action_url("http://10.0.0.5:4000", "AA:BB:CC:DD:EE:FF", "deselect"),
            "http://10.0.0.5:4000/devices/AA:BB:CC:DD:EE:FF/deselect"
        );
    }

    // Criterion: a per-speaker latency offset can be set — the client posts to
    // `/devices/{addr}/offset`.
    #[test]
    fn test_device_action_url_builds_offset_path_tolerating_trailing_slash() {
        assert_eq!(
            device_action_url("http://10.0.0.5:4000/", "AA:BB:CC:DD:EE:FF", "offset"),
            "http://10.0.0.5:4000/devices/AA:BB:CC:DD:EE:FF/offset"
        );
    }

    // Criterion: `GET /targets` returns the current selection — the client builds
    // the `/targets` URL, tolerating a trailing slash.
    #[test]
    fn test_targets_url_appends_path() {
        assert_eq!(
            targets_url("http://10.0.0.5:4000"),
            "http://10.0.0.5:4000/targets"
        );
        assert_eq!(
            targets_url("http://10.0.0.5:4000/"),
            "http://10.0.0.5:4000/targets"
        );
    }

    // Criterion: mobile exposes a Spotify start call — the client posts to
    // `/spotify/start`.
    #[test]
    fn test_spotify_url_builds_start_path() {
        assert_eq!(
            spotify_url("http://10.0.0.5:4000", "start"),
            "http://10.0.0.5:4000/spotify/start"
        );
    }

    // Criterion: mobile exposes a Spotify stop call — the client posts to
    // `/spotify/stop`, tolerating a trailing slash on the base.
    #[test]
    fn test_spotify_url_builds_stop_path_tolerating_trailing_slash() {
        assert_eq!(
            spotify_url("http://10.0.0.5:4000/", "stop"),
            "http://10.0.0.5:4000/spotify/stop"
        );
    }

    // Criterion: mobile exposes a Spotify status call — the client builds the
    // `/spotify/status` URL.
    #[test]
    fn test_spotify_url_builds_status_path() {
        assert_eq!(
            spotify_url("http://10.0.0.5:4000", "status"),
            "http://10.0.0.5:4000/spotify/status"
        );
    }

    // Criterion (phase 5.2): mobile exposes an SSE now-playing subscription — the
    // client builds the `/spotify/now-playing` URL, tolerating a trailing slash.
    #[test]
    fn test_now_playing_url_appends_path() {
        assert_eq!(
            now_playing_url("http://10.0.0.5:4000"),
            "http://10.0.0.5:4000/spotify/now-playing"
        );
        assert_eq!(
            now_playing_url("http://10.0.0.5:4000/"),
            "http://10.0.0.5:4000/spotify/now-playing"
        );
    }

    // Criterion (phase 5.2): the SSE reader parses a `now-playing` event block into
    // a `NowPlaying` snapshot.
    #[test]
    fn test_sse_now_playing_payload_parses_now_playing_event() {
        let block = concat!(
            "event:now-playing\n",
            "data:{\"state\":\"playing\",\"title\":\"Song\",\"artist\":\"Artist\",",
            "\"album\":\"Album\",\"progress_ms\":12000,\"duration_ms\":210000}\n\n",
        );
        let np = sse_now_playing_payload(block).expect("parse now-playing event");
        assert_eq!(np.state, blue2th_proto::NowPlayingState::Playing);
        assert_eq!(np.title.as_deref(), Some("Song"));
    }

    // Criterion (phase 5.2): the SSE reader ignores keep-alive comments and other
    // event kinds (returns None).
    #[test]
    fn test_sse_now_playing_payload_ignores_non_now_playing_and_comments() {
        assert!(sse_now_playing_payload("event:error\ndata:boom\n\n").is_none());
        assert!(sse_now_playing_payload(": keep-alive\n\n").is_none());
    }
}
