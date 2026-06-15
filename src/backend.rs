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

use blue2th_proto::HealthStatus;

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

/// `GET {base}/health` and decode the backend's `HealthStatus`.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub async fn ping_backend() -> Result<HealthStatus, BackendError> {
    let url = health_url(backend_base_url());
    let response = reqwest::get(&url)
        .await
        .map_err(|e| BackendError::new(e.to_string()))?;
    response
        .error_for_status()
        .map_err(|e| BackendError::new(e.to_string()))?
        .json::<HealthStatus>()
        .await
        .map_err(|e| BackendError::new(e.to_string()))
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
}
