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

use blue2th_proto::{DeviceInfo, HealthStatus};
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
}
