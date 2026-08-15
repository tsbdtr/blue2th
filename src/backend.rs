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
pub struct BackendError {
    /// What to show the user.
    message: String,
    /// Whether the backend refused the app's credential (or it has none). Kept
    /// apart from the message so the UI can point at pairing instead of showing
    /// yet another network failure — "not paired" is not "unreachable".
    not_paired: bool,
}

impl BackendError {
    fn new(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
            not_paired: false,
        }
    }

    /// The typed "the app is not paired with this backend" failure: no token
    /// stored, or the backend answered 401.
    pub fn not_paired() -> Self {
        Self {
            message: NOT_PAIRED.to_string(),
            not_paired: true,
        }
    }

    /// Whether this failure means the app must pair (again) rather than that the
    /// backend is unreachable.
    pub fn is_not_paired(&self) -> bool {
        self.not_paired
    }
}

impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

/// Message carried by every call made while no backend is configured. The app
/// fails fast with it instead of guessing an address and timing out.
pub const NO_BACKEND_CONFIGURED: &str = "no backend configured";

/// Message carried by every call made while the active backend has no token, or
/// answered 401 (phase 6.4).
pub const NOT_PAIRED: &str = "not paired";

/// Build the `{base}/pair` URL, tolerating a trailing slash on the base.
fn pair_url(_base: &str) -> String {
    // STUB (phase 6.4).
    todo!("phase 6.4: build the /pair URL")
}

/// The `Authorization` header value carrying `token`.
fn auth_header_value(_token: &str) -> String {
    // STUB (phase 6.4).
    todo!("phase 6.4: build the bearer header value")
}

/// Map a failed backend response to a typed error. Pure.
///
/// A 401 becomes [`BackendError::not_paired`] whatever the body says; any other
/// status keeps the backend's own message (which `error_for_status` would throw
/// away, leaving the phone showing a bare status line).
fn backend_error_for(_status: u16, _body: &str) -> BackendError {
    // STUB (phase 6.4).
    todo!("phase 6.4: map a failed response to a typed error, 401 = not paired")
}

/// The active backend's base URL **and** token, or a typed failure. Pure.
///
/// Nothing configured fails with [`NO_BACKEND_CONFIGURED`]; an active backend
/// that was never paired fails with [`BackendError::not_paired`] — in both cases
/// before any request is built, so an unpaired app never waits on a timeout.
fn authed_base_from(_settings: &AppSettings) -> Result<(String, String), BackendError> {
    // STUB (phase 6.4).
    todo!("phase 6.4: resolve the active backend's base URL and bearer token")
}

/// `POST {base}/pair` — exchange a short-lived pairing code for the backend's
/// long-lived API token. The one call that carries no bearer, since the app has
/// none yet.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub async fn pair(_base: &str, _code: &str) -> Result<String, BackendError> {
    // STUB (phase 6.4).
    todo!("phase 6.4: exchange the pairing code for the API token")
}

/// How long a settings-page call waits before giving up.
///
/// A mistyped LAN address is the normal case here: the host either refuses at
/// once or, when it silently drops packets, never answers at all. Without a
/// bound, `Test` would spin forever and the two best-effort steps of
/// [`activate_backend`] would leak a task per switch.
const SETTINGS_CALL_TIMEOUT: Duration = Duration::from_secs(5);

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

/// `POST {base}/config` against an explicit address — push the app's name to the
/// backend, which adopts it as its Spotify Connect device name.
///
/// Addressed explicitly rather than through `backend_base_url()`: the only caller
/// is [`activate_backend`], which must reach the backend it *just* switched to
/// even if a concurrent switch has already moved the resolved address on.
async fn set_config_at(
    base: &str,
    name: &str,
    restore_during_playback: bool,
) -> Result<ServerConfig, BackendError> {
    let url = config_url(base);
    let response = reqwest::Client::new()
        .post(&url)
        .timeout(SETTINGS_CALL_TIMEOUT)
        .json(&ConfigRequest {
            // Owned copy: `ConfigRequest` is a plain DTO built for serialization.
            name: name.to_string(),
            restore_during_playback,
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
        .timeout(SETTINGS_CALL_TIMEOUT)
        .send()
        .await
        .map_err(|e| BackendError::new(describe(&e)))?;
    backend_error_message(response).await?;
    Ok(())
}

/// Push the active backend's name to it, best-effort and silent.
///
/// The app is the source of truth for that name, but it only reaches the backend
/// when something sends it: a push that failed while the backend was down, or a
/// server that restarted since, would otherwise leave the Connect device
/// advertising a stale name. Called when the backend becomes reachable again, so
/// a failure here is expected — it will simply be retried on the next transition,
/// and there is no user action to prompt.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub async fn push_active_name() {
    let _ = push_active_config().await;
}

/// Push the active backend's whole config (name **and** settings), surfacing the
/// failure. Used by the settings toggles, where the user is watching and deserves
/// to be told; `push_active_name` is the same call made silently on reconnection.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub async fn push_active_config() -> Result<(), BackendError> {
    let settings = crate::settings::current();
    let Some(entry) = settings.active_backend() else {
        return Err(BackendError::new(NO_BACKEND_CONFIGURED));
    };
    set_config_at(&entry.url, &entry.name, entry.restore_during_playback).await?;
    Ok(())
}

/// Whether the backend at `previous` is really being left behind by a switch to
/// `next`. Re-activating the backend already in use must quieten nothing:
/// pausing it is the opposite of what the user asked for. Pure.
fn is_left_behind(previous: &str, next: Option<&str>) -> bool {
    next != Some(previous)
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

    // Owned copy: the borrow of `settings` must not survive the awaits below,
    // and the address is the one to push to whatever the cache does meanwhile.
    let target = settings
        .active_backend()
        .map(|b| (b.url.clone(), b.name.clone(), b.restore_during_playback));

    // Both remote steps are best-effort and independent; the last failure is
    // surfaced so the toast says something, but neither undoes the switch.
    let mut failure = None;
    if let Some(base) = previous {
        if is_left_behind(&base, target.as_ref().map(|(url, _, _)| url.as_str())) {
            if let Err(e) = pause_at(&base).await {
                failure = Some(e);
            }
        }
    }
    if let Some((base, name, restore_during_playback)) = target {
        if let Err(e) = set_config_at(&base, &name, restore_during_playback).await {
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
    // Bounded: a typo'd address that drops packets would otherwise leave the
    // `Test` button waiting forever with no answer either way.
    let response = reqwest::Client::new()
        .get(health_url(url))
        .timeout(SETTINGS_CALL_TIMEOUT)
        .send()
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

    // ---- phase 6.3: the config push carries the restore setting ----

    /// Extract the body of a raw HTTP request, once it has fully arrived.
    /// `None` means "keep reading". Fallible rather than asserting: `clippy`'s
    /// `allow-expect-in-tests` does not excuse a helper from panicking, and the
    /// test function is the right place to fail.
    fn request_body(raw: &[u8]) -> Option<String> {
        let text = String::from_utf8_lossy(raw).to_string();
        let (head, body) = text.split_once("\r\n\r\n")?;
        let length: usize = head.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())
                .flatten()
        })?;
        (body.len() >= length).then(|| body[..length].to_string())
    }

    /// Run one `set_config_at` against a throwaway loopback listener and return
    /// the JSON body it pushed. No real backend, no hardware: the point is what
    /// goes on the wire.
    async fn captured_config_push(
        name: &str,
        restore_during_playback: bool,
    ) -> Result<serde_json::Value, String> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("bind the test listener: {e}"))?;
        let addr = listener
            .local_addr()
            .map_err(|e| format!("read the test listener address: {e}"))?;

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener
                .accept()
                .await
                .map_err(|e| format!("accept: {e}"))?;
            let mut raw = Vec::new();
            let mut chunk = [0u8; 1024];
            let body = loop {
                match request_body(&raw) {
                    Some(body) => break body,
                    None => {
                        let read = stream
                            .read(&mut chunk)
                            .await
                            .map_err(|e| format!("read the request: {e}"))?;
                        if read == 0 {
                            return Err("the client closed before sending a body".to_string());
                        }
                        raw.extend_from_slice(&chunk[..read]);
                    },
                }
            };
            // A well-formed `ServerConfig` so the client's decode step succeeds.
            let payload = r#"{"name":"Salon","restore_during_playback":true}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{payload}",
                payload.len()
            );
            stream
                .write_all(response.as_bytes())
                .await
                .map_err(|e| format!("write the response: {e}"))?;
            let _ = stream.flush().await;
            Ok::<String, String>(body)
        });

        let base = format!("http://{addr}");
        let pushed = set_config_at(&base, name, restore_during_playback).await;
        let body = server
            .await
            .map_err(|e| format!("join the test listener: {e}"))??;
        pushed.map_err(|e| format!("set_config_at: {e}"))?;
        serde_json::from_str(&body).map_err(|e| format!("parse the pushed body ({body}): {e}"))
    }

    // Criterion (phase 6.3): the config push carries the flag — `POST /config`
    // sends `restore_during_playback` next to the name, in both states.
    #[tokio::test]
    async fn test_config_push_carries_the_restore_flag() {
        let pushed = captured_config_push("Salon", true)
            .await
            .expect("push the config with the flag on");
        assert_eq!(pushed.get("name").and_then(|v| v.as_str()), Some("Salon"));
        assert_eq!(
            pushed
                .get("restore_during_playback")
                .and_then(serde_json::Value::as_bool),
            Some(true),
            "the pushed body must carry the caller's flag, got {pushed}"
        );

        let pushed = captured_config_push("Salon", false)
            .await
            .expect("push the config with the flag off");
        assert_eq!(
            pushed
                .get("restore_during_playback")
                .and_then(serde_json::Value::as_bool),
            Some(false),
            "turning the setting off must reach the backend, got {pushed}"
        );
    }

    // Criterion (phase 6.2): switching backends quietens the one being left
    // behind — a different address than the newly active one.
    #[test]
    fn test_is_left_behind_is_true_for_another_backend() {
        assert!(is_left_behind(
            "http://127.0.0.1:1",
            Some("http://127.0.0.1:2")
        ));
    }

    // Criterion (phase 6.2): re-activating the backend already in use must not
    // pause it — that would stop the playback the user just asked to keep.
    #[test]
    fn test_is_left_behind_is_false_for_the_same_backend() {
        assert!(!is_left_behind(
            "http://127.0.0.1:1",
            Some("http://127.0.0.1:1")
        ));
    }

    // Criterion (phase 6.2): with no backend left active (the switch target
    // vanished), the previous one is still quietened rather than left playing.
    #[test]
    fn test_is_left_behind_is_true_when_nothing_is_active() {
        assert!(is_left_behind("http://127.0.0.1:1", None));
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

    // ---- phase 6.4: the bearer on every call, and the pairing exchange ----

    use crate::settings::{BackendEntry, PairingMethod};

    /// Settings holding one active backend at `url`, paired or not.
    ///
    /// Built by hand rather than through `add`/`set_token`, so a fixture never
    /// depends on the functions under test.
    fn active_with_token(url: &str, token: Option<&str>) -> AppSettings {
        AppSettings {
            backends: vec![BackendEntry {
                name: "Salon".to_string(),
                url: url.to_string(),
                restore_during_playback: true,
                token: token.map(str::to_string),
                pairing: PairingMethod::Code,
            }],
            active: Some(0),
        }
    }

    /// The whole raw request once it has fully arrived (head, and body when a
    /// `content-length` announces one). `None` means "keep reading".
    fn request_complete(raw: &[u8]) -> Option<String> {
        let text = String::from_utf8_lossy(raw).to_string();
        let (head, body) = text.split_once("\r\n\r\n")?;
        let length: usize = head
            .lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().ok())
                    .flatten()
            })
            .unwrap_or(0);
        (body.len() >= length).then(|| text.clone())
    }

    /// Read a header value out of a raw request.
    fn header_value(raw: &str, name: &str) -> Option<String> {
        raw.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name)
                .then(|| value.trim().to_string())
        })
    }

    /// Serve exactly one request on a throwaway loopback listener with a canned
    /// reply, and hand back the base URL plus the raw request the client sent.
    /// No real backend and no hardware: the point is what goes on the wire.
    async fn canned_backend(
        status_line: &'static str,
        payload: &'static str,
    ) -> Result<(String, tokio::task::JoinHandle<Result<String, String>>), String> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("bind the test listener: {e}"))?;
        let addr = listener
            .local_addr()
            .map_err(|e| format!("read the test listener address: {e}"))?;

        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener
                .accept()
                .await
                .map_err(|e| format!("accept: {e}"))?;
            let mut raw = Vec::new();
            let mut chunk = [0u8; 1024];
            let request = loop {
                match request_complete(&raw) {
                    Some(request) => break request,
                    None => {
                        let read = stream
                            .read(&mut chunk)
                            .await
                            .map_err(|e| format!("read the request: {e}"))?;
                        if read == 0 {
                            return Err("the client closed before sending a request".to_string());
                        }
                        raw.extend_from_slice(&chunk[..read]);
                    },
                }
            };
            let response = format!(
                "HTTP/1.1 {status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{payload}",
                payload.len()
            );
            stream
                .write_all(response.as_bytes())
                .await
                .map_err(|e| format!("write the response: {e}"))?;
            let _ = stream.flush().await;
            Ok::<String, String>(request)
        });

        Ok((format!("http://{addr}"), handle))
    }

    // Criterion: every backend call carries the bearer when the active entry has
    // one — asserted on the wire, for a GET call.
    #[tokio::test]
    async fn test_backend_calls_carry_the_bearer_token() {
        let _guard = SETTINGS_GUARD.lock().await;
        let (base, served) = canned_backend("200 OK", r#"{"speakers":[],"routing":"idle"}"#)
            .await
            .expect("start the canned backend");
        crate::settings::set_current(active_with_token(&base, Some("tok-123")));

        let outcome = fetch_targets().await;
        let request = served
            .await
            .expect("join the test listener")
            .expect("serve one request");
        crate::settings::set_current(AppSettings::default());

        assert!(outcome.is_ok(), "the call must succeed: {outcome:?}");
        assert_eq!(
            header_value(&request, "authorization").as_deref(),
            Some("Bearer tok-123"),
            "every guarded call must carry the bearer, got {request}"
        );
    }

    // Criterion: the same holds for the calls that push a body — the config push
    // is the one the settings page and the reconnection both go through.
    #[tokio::test]
    async fn test_the_config_push_carries_the_bearer_token() {
        let _guard = SETTINGS_GUARD.lock().await;
        let (base, served) = canned_backend(
            "200 OK",
            r#"{"name":"Salon","restore_during_playback":true}"#,
        )
        .await
        .expect("start the canned backend");
        crate::settings::set_current(active_with_token(&base, Some("tok-123")));

        let outcome = push_active_config().await;
        let request = served
            .await
            .expect("join the test listener")
            .expect("serve one request");
        crate::settings::set_current(AppSettings::default());

        assert!(outcome.is_ok(), "the push must succeed: {outcome:?}");
        assert_eq!(
            header_value(&request, "authorization").as_deref(),
            Some("Bearer tok-123")
        );
    }

    // Criterion (non-nominal): with no token for the active backend the calls
    // fail fast — the way an unconfigured backend does — instead of every screen
    // failing on its own after a timeout.
    #[tokio::test]
    async fn test_a_call_without_a_token_fails_fast_as_not_paired() {
        let _guard = SETTINGS_GUARD.lock().await;
        // Port 1 is never listening: reaching it at all would take a timeout.
        crate::settings::set_current(active_with_token("http://127.0.0.1:1", None));

        let started = std::time::Instant::now();
        let error = fetch_targets()
            .await
            .expect_err("an unpaired app must not call the backend");
        crate::settings::set_current(AppSettings::default());

        assert!(
            error.is_not_paired(),
            "the failure must point at pairing, got {error}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "no request may be attempted, so the call must return immediately"
        );
    }

    // Criterion: `pair(base, code)` exchanges the code for the token — and sends
    // no bearer, since the app has none yet.
    #[tokio::test]
    async fn test_pair_exchanges_the_code_for_the_token() {
        let (base, served) = canned_backend("200 OK", r#"{"token":"api-token-value"}"#)
            .await
            .expect("start the canned backend");

        let token = pair(&base, "K7M2QX").await;
        let request = served
            .await
            .expect("join the test listener")
            .expect("serve one request");

        assert_eq!(
            token.map_err(|e| e.to_string()),
            Ok("api-token-value".to_string())
        );
        assert!(
            request.starts_with("POST /pair "),
            "the exchange must POST /pair, got {request}"
        );
        assert!(
            request.contains("K7M2QX"),
            "the submitted code must be on the wire, got {request}"
        );
        assert_eq!(
            header_value(&request, "authorization"),
            None,
            "pairing is the one call made without a bearer"
        );
    }

    // Criterion (non-nominal): a refused code reads as "not paired", distinct
    // from an unreachable backend, so the settings page can say so.
    #[tokio::test]
    async fn test_pair_with_a_refused_code_reports_not_paired() {
        let (base, served) = canned_backend("401 Unauthorized", "pairing refused")
            .await
            .expect("start the canned backend");

        let error = pair(&base, "AAAAAA")
            .await
            .expect_err("a refused code must not yield a token");
        let _ = served.await;

        assert!(error.is_not_paired(), "got {error}");
    }

    // Criterion (non-nominal): the SSE feeds must treat a 401 as terminal rather
    // than as a network blip — the subscription returns a "not paired" failure,
    // which is what lets the reconnect loop stop instead of spinning forever.
    #[tokio::test]
    async fn test_now_playing_subscription_reports_not_paired_on_401() {
        let _guard = SETTINGS_GUARD.lock().await;
        let (base, served) = canned_backend("401 Unauthorized", "not paired")
            .await
            .expect("start the canned backend");
        crate::settings::set_current(active_with_token(&base, Some("stale-token")));

        let error = subscribe_now_playing(|_| {})
            .await
            .expect_err("a 401 must end the subscription");
        let _ = served.await;
        crate::settings::set_current(AppSettings::default());

        assert!(error.is_not_paired(), "got {error}");
    }

    // Criterion: a 401 from any route maps to the typed "not paired" failure,
    // whatever the backend wrote in the body.
    #[test]
    fn test_backend_error_for_401_is_not_paired() {
        let error = backend_error_for(401, "some server wording");
        assert!(error.is_not_paired());
        assert!(
            error.to_string().contains(NOT_PAIRED),
            "the user must read that the app is not paired, got {error}"
        );
    }

    // Criterion: any other failure keeps the backend's own message, which is the
    // only thing telling the user what actually went wrong.
    #[test]
    fn test_backend_error_for_another_status_keeps_the_backend_message() {
        let error = backend_error_for(503, "Spotify client id not configured");
        assert!(!error.is_not_paired());
        assert_eq!(error.to_string(), "Spotify client id not configured");

        // An empty body still has to say something.
        let bare = backend_error_for(500, "");
        assert!(!bare.to_string().trim().is_empty());
    }

    // Criterion: the app resolves the active backend's address **and** token in
    // one place, so no call site can forget the bearer.
    #[test]
    fn test_authed_base_from_returns_the_url_and_token() {
        let settings = active_with_token("http://192.168.1.107:4000", Some("tok-123"));
        assert_eq!(
            authed_base_from(&settings).map_err(|e| e.to_string()),
            Ok((
                "http://192.168.1.107:4000".to_string(),
                "tok-123".to_string()
            ))
        );
    }

    // Criterion (non-nominal): an active backend with no token is "not paired",
    // not "no backend configured" — the two send the user to different places.
    #[test]
    fn test_authed_base_from_without_a_token_reports_not_paired() {
        let settings = active_with_token("http://192.168.1.107:4000", None);
        let error = authed_base_from(&settings).expect_err("an unpaired backend has no bearer");
        assert!(error.is_not_paired(), "got {error}");
    }

    // Criterion: with nothing active the failure stays the phase 6.2 one.
    #[test]
    fn test_authed_base_from_without_an_active_backend_is_unconfigured() {
        let error = authed_base_from(&AppSettings::default())
            .expect_err("an unconfigured app must have no address");
        assert!(
            error.to_string().contains(NO_BACKEND_CONFIGURED),
            "got {error}"
        );
        assert!(
            !error.is_not_paired(),
            "nothing configured is not the same as not paired"
        );
    }

    // Criterion: the pairing exchange posts to `{base}/pair`, tolerating a
    // trailing slash on the base like every other URL builder here.
    #[test]
    fn test_pair_url_appends_path() {
        assert_eq!(
            pair_url("http://10.0.0.5:4000"),
            "http://10.0.0.5:4000/pair"
        );
        assert_eq!(
            pair_url("http://10.0.0.5:4000/"),
            "http://10.0.0.5:4000/pair"
        );
    }

    // Criterion: the credential travels as a bearer, the scheme the server
    // parses.
    #[test]
    fn test_auth_header_value_is_a_bearer() {
        assert_eq!(auth_header_value("tok-123"), "Bearer tok-123");
    }
}
