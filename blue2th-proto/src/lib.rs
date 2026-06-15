//! Shared data-transfer types for the blue2th mobile app <-> PC backend contract.
//!
//! Keep this crate target-agnostic (no platform-specific deps): it is compiled
//! both into the Android app and the Linux backend. DTOs grow per roadmap phase
//! (see `docs/ROADMAP.md`); phase 0 only needs the health payload.

use serde::{Deserialize, Serialize};

/// Health/version payload returned by the backend `GET /health` endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthStatus {
    /// Liveness marker, e.g. `"ok"`.
    pub status: String,
    /// Backend crate version (its `CARGO_PKG_VERSION`).
    pub version: String,
}

impl HealthStatus {
    /// Build an `"ok"` status carrying the given backend version.
    pub fn ok(version: impl Into<String>) -> Self {
        Self {
            status: "ok".to_string(),
            version: version.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_health_status_round_trips_through_json() {
        let original = HealthStatus::ok("0.1.0");
        let json = serde_json::to_string(&original).expect("serialize HealthStatus");
        let parsed: HealthStatus = serde_json::from_str(&json).expect("deserialize HealthStatus");
        assert_eq!(original, parsed);
    }

    #[test]
    fn test_health_status_ok_sets_status_field() {
        let status = HealthStatus::ok("9.9.9");
        assert_eq!(status.status, "ok");
        assert_eq!(status.version, "9.9.9");
    }
}
