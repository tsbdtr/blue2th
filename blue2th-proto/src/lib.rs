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

/// A Bluetooth adapter present on the backend host (e.g. `hci0`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterInfo {
    /// BlueZ adapter name, e.g. `"hci0"`.
    pub name: String,
    /// Adapter MAC address.
    pub address: String,
    /// Whether the adapter is powered on.
    pub powered: bool,
    /// Whether the adapter is currently discovering devices.
    pub discovering: bool,
}

/// A Bluetooth device known to the backend host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// Device MAC address (stable identifier used by the API).
    pub address: String,
    /// Friendly name/alias, absent if the device never advertised one.
    pub name: Option<String>,
    /// Whether the device is bonded (paired) with the host.
    pub paired: bool,
    /// Whether the device is currently connected.
    pub connected: bool,
    /// Last known signal strength in dBm, if available.
    pub rssi: Option<i16>,
}

/// High-level playback status of the backend audio engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlaybackStatus {
    /// Nothing is playing.
    Stopped,
    /// Audio is actively playing.
    Playing,
    /// Playback is paused and can be resumed.
    Paused,
}

/// Current playback state returned by the transport endpoints.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaybackState {
    /// Whether the engine is stopped, playing or paused.
    pub status: PlaybackStatus,
    /// Current sink volume in `0.0..=1.0`.
    pub volume: f32,
}

/// Body of `POST /volume` — the desired sink volume level.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VolumeRequest {
    /// Desired volume; the backend clamps it to `0.0..=1.0`.
    pub level: f32,
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

    #[test]
    fn test_adapter_info_round_trips_through_json() {
        let original = AdapterInfo {
            name: "hci0".to_string(),
            address: "AA:BB:CC:DD:EE:FF".to_string(),
            powered: true,
            discovering: false,
        };
        let json = serde_json::to_string(&original).expect("serialize AdapterInfo");
        let parsed: AdapterInfo = serde_json::from_str(&json).expect("deserialize AdapterInfo");
        assert_eq!(original, parsed);
    }

    #[test]
    fn test_device_info_round_trips_with_optional_fields() {
        let original = DeviceInfo {
            address: "11:22:33:44:55:66".to_string(),
            name: Some("L3".to_string()),
            paired: true,
            connected: false,
            rssi: Some(-57),
        };
        let json = serde_json::to_string(&original).expect("serialize DeviceInfo");
        let parsed: DeviceInfo = serde_json::from_str(&json).expect("deserialize DeviceInfo");
        assert_eq!(original, parsed);

        // Absent optionals must round-trip too.
        let nameless = DeviceInfo {
            name: None,
            rssi: None,
            ..original
        };
        let json = serde_json::to_string(&nameless).expect("serialize nameless DeviceInfo");
        let parsed: DeviceInfo = serde_json::from_str(&json).expect("deserialize nameless");
        assert_eq!(nameless, parsed);
    }

    // Criterion: `GET /playback` returns the current `PlaybackState` —
    // PlaybackStatus must round-trip through JSON for every variant.
    #[test]
    fn test_playback_status_round_trips_through_json() {
        for status in [
            PlaybackStatus::Stopped,
            PlaybackStatus::Playing,
            PlaybackStatus::Paused,
        ] {
            let json = serde_json::to_string(&status).expect("serialize PlaybackStatus");
            let parsed: PlaybackStatus =
                serde_json::from_str(&json).expect("deserialize PlaybackStatus");
            assert_eq!(status, parsed);
        }
    }

    // Criterion: `GET /playback` returns the current `PlaybackState`.
    #[test]
    fn test_playback_state_round_trips_through_json() {
        let original = PlaybackState {
            status: PlaybackStatus::Playing,
            volume: 0.5,
        };
        let json = serde_json::to_string(&original).expect("serialize PlaybackState");
        let parsed: PlaybackState =
            serde_json::from_str(&json).expect("deserialize PlaybackState");
        assert_eq!(original, parsed);
    }

    // Criterion: `POST /volume` body carries the desired level — VolumeRequest
    // must round-trip through JSON.
    #[test]
    fn test_volume_request_round_trips_through_json() {
        let original = VolumeRequest { level: 0.75 };
        let json = serde_json::to_string(&original).expect("serialize VolumeRequest");
        let parsed: VolumeRequest =
            serde_json::from_str(&json).expect("deserialize VolumeRequest");
        assert_eq!(original, parsed);
    }
}
