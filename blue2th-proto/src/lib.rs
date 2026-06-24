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

/// A speaker selected as a playback target, with its per-speaker latency offset.
///
/// Phase 4 (fan-out): the user picks up to two connected speakers and tunes each
/// one's offset (ms) to align them. The offset is an absolute additive delay
/// applied as branch latency on the combined sink; the backend clamps it to
/// `0..=750` ms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpeakerTarget {
    /// Target speaker MAC address (matches `DeviceInfo::address`).
    pub address: String,
    /// Per-speaker latency offset in milliseconds (`0..=750`).
    pub offset_ms: u32,
}

/// How the backend routes playback, derived from the number of selected targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RoutingMode {
    /// No target selected: nothing to play.
    Idle,
    /// Exactly one target: phase-3 single-speaker path (`set-default-sink`).
    Single,
    /// Two targets: a PipeWire combined sink spanning both speakers.
    Combined,
}

/// Current playback-target selection returned by `GET /targets`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetsState {
    /// Selected speakers (at most two) with their offsets.
    pub speakers: Vec<SpeakerTarget>,
    /// Routing mode derived from the selection count.
    pub routing: RoutingMode,
}

/// Body of `POST /devices/{addr}/offset` — the desired per-speaker latency offset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OffsetRequest {
    /// Desired offset in milliseconds; the backend clamps it to `0..=750`.
    pub offset_ms: u32,
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
        let parsed: PlaybackState = serde_json::from_str(&json).expect("deserialize PlaybackState");
        assert_eq!(original, parsed);
    }

    // Criterion: `POST /volume` body carries the desired level — VolumeRequest
    // must round-trip through JSON.
    #[test]
    fn test_volume_request_round_trips_through_json() {
        let original = VolumeRequest { level: 0.75 };
        let json = serde_json::to_string(&original).expect("serialize VolumeRequest");
        let parsed: VolumeRequest = serde_json::from_str(&json).expect("deserialize VolumeRequest");
        assert_eq!(original, parsed);
    }

    // Criterion: the new proto DTOs round-trip through serde — SpeakerTarget.
    #[test]
    fn test_speaker_target_round_trips_through_json() {
        let original = SpeakerTarget {
            address: "AA:BB:CC:DD:EE:FF".to_string(),
            offset_ms: 120,
        };
        let json = serde_json::to_string(&original).expect("serialize SpeakerTarget");
        let parsed: SpeakerTarget = serde_json::from_str(&json).expect("deserialize SpeakerTarget");
        assert_eq!(original, parsed);
    }

    // Criterion: the new proto DTOs round-trip through serde — RoutingMode, all
    // three variants, serialized lowercase.
    #[test]
    fn test_routing_mode_round_trips_through_json() {
        for mode in [
            RoutingMode::Idle,
            RoutingMode::Single,
            RoutingMode::Combined,
        ] {
            let json = serde_json::to_string(&mode).expect("serialize RoutingMode");
            let parsed: RoutingMode = serde_json::from_str(&json).expect("deserialize RoutingMode");
            assert_eq!(mode, parsed);
        }
    }

    // Criterion: the routing mode is serialized in lowercase (shared contract).
    #[test]
    fn test_routing_mode_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&RoutingMode::Combined).expect("serialize"),
            "\"combined\""
        );
        assert_eq!(
            serde_json::to_string(&RoutingMode::Single).expect("serialize"),
            "\"single\""
        );
        assert_eq!(
            serde_json::to_string(&RoutingMode::Idle).expect("serialize"),
            "\"idle\""
        );
    }

    // Criterion: `GET /targets` returns the current selection, per-speaker offsets
    // and routing mode — TargetsState must round-trip through JSON.
    #[test]
    fn test_targets_state_round_trips_through_json() {
        let original = TargetsState {
            speakers: vec![
                SpeakerTarget {
                    address: "AA:BB:CC:DD:EE:FF".to_string(),
                    offset_ms: 0,
                },
                SpeakerTarget {
                    address: "11:22:33:44:55:66".to_string(),
                    offset_ms: 250,
                },
            ],
            routing: RoutingMode::Combined,
        };
        let json = serde_json::to_string(&original).expect("serialize TargetsState");
        let parsed: TargetsState = serde_json::from_str(&json).expect("deserialize TargetsState");
        assert_eq!(original, parsed);

        // An empty selection (Idle) must round-trip too.
        let idle = TargetsState {
            speakers: vec![],
            routing: RoutingMode::Idle,
        };
        let json = serde_json::to_string(&idle).expect("serialize idle TargetsState");
        let parsed: TargetsState =
            serde_json::from_str(&json).expect("deserialize idle TargetsState");
        assert_eq!(idle, parsed);
    }

    // Criterion: the new proto DTOs round-trip through serde — OffsetRequest.
    #[test]
    fn test_offset_request_round_trips_through_json() {
        let original = OffsetRequest { offset_ms: 750 };
        let json = serde_json::to_string(&original).expect("serialize OffsetRequest");
        let parsed: OffsetRequest = serde_json::from_str(&json).expect("deserialize OffsetRequest");
        assert_eq!(original, parsed);
    }
}
