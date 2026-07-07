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
    DeviceInfo, HealthStatus, OffsetRequest, PlaybackState, SpotifyState, TargetsState,
    VolumeRequest,
};
use futures::StreamExt;

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

/// Compile-time-overridable backend base URL. Set `BLUE2TH_BACKEND_URL` at build
/// time (e.g. the PC's LAN address `http://192.168.x.y:4000`) to point the app at
/// a real backend; defaults to localhost for host/dev runs.
pub fn backend_base_url() -> &'static str {
    option_env!("BLUE2TH_BACKEND_URL").unwrap_or("http://127.0.0.1:4000")
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
    let url = health_url(backend_base_url());
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
    let url = format!("{}/scan", backend_base_url().trim_end_matches('/'));
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
        backend_base_url().trim_end_matches('/')
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
    let url = format!("{}/devices", backend_base_url().trim_end_matches('/'));
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
    let url = format!("{}/playback", backend_base_url().trim_end_matches('/'));
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
    let url = format!("{}/volume", backend_base_url().trim_end_matches('/'));
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
    let url = format!("{}/{action}", backend_base_url().trim_end_matches('/'));
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
    let url = device_action_url(backend_base_url(), address, "select");
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
    let url = device_action_url(backend_base_url(), address, "deselect");
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
    let url = device_action_url(backend_base_url(), address, "offset");
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
    let url = targets_url(backend_base_url());
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
    let url = spotify_url(backend_base_url(), "status");
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
    let url = spotify_url(backend_base_url(), action);
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

    #[test]
    fn test_backend_base_url_defaults_to_http() {
        assert!(backend_base_url().starts_with("http"));
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
}
