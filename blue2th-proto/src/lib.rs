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

/// Whether the Spotify Connect source backend (a `librespot` subprocess) is up.
///
/// Phase 5.1: the PC advertises itself as a Spotify Connect device; the user
/// activates/deactivates the backend from the app. Actual transport is driven by
/// the official Spotify app in this slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpotifyStatus {
    /// The `librespot` subprocess is not running.
    Stopped,
    /// The `librespot` subprocess is running and advertised as a Connect device.
    Running,
}

/// State of the Spotify source backend returned by the `/spotify/*` endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpotifyState {
    /// Whether the backend subprocess is stopped or running.
    pub status: SpotifyStatus,
    /// The Spotify Connect device name advertised (e.g. `blue2th-PC`).
    pub device_name: String,
}

/// Whether the app is authenticated against the Spotify Web API (phase 5.2).
///
/// The server holds the OAuth (Authorization Code + PKCE) tokens; the app only
/// observes this coarse state via `GET /spotify/auth/status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpotifyAuthStatus {
    /// No tokens held: the user has not logged in (or the refresh was revoked).
    Disconnected,
    /// Valid tokens held: the server can drive the Spotify Web API.
    Connected,
}

/// Auth state returned by `GET /spotify/auth/status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpotifyAuthState {
    /// Whether the app is connected (tokens held) or disconnected.
    pub status: SpotifyAuthStatus,
}

/// Response of `GET /spotify/auth/url`: the Spotify authorize URL to open in the
/// system browser, plus the CSRF `state` the app must echo back on callback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthUrlResponse {
    /// The `accounts.spotify.com/authorize` URL (PKCE challenge embedded).
    pub url: String,
    /// Opaque CSRF token the server validates on `POST /spotify/auth/callback`.
    pub state: String,
}

/// Body of `POST /spotify/auth/callback`: the authorization `code` returned by
/// Spotify on the custom-scheme redirect, and the CSRF `state` to validate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthCallbackRequest {
    /// The one-time authorization code to exchange for tokens.
    pub code: String,
    /// The CSRF state echoed back from the authorize step.
    pub state: String,
}

/// High-level now-playing state pushed over the `/spotify/now-playing` SSE feed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NowPlayingState {
    /// Nothing playing / no active device (Web API 204 or empty body).
    Idle,
    /// A track is actively playing.
    Playing,
    /// A track is loaded but paused.
    Paused,
}

/// A now-playing snapshot mapped from the Spotify `/me/player` payload and pushed
/// to the app over SSE. All track fields are absent when the state is `Idle`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NowPlaying {
    /// Playing / paused / idle.
    pub state: NowPlayingState,
    /// Track title, if any.
    pub title: Option<String>,
    /// Primary artist name, if any.
    pub artist: Option<String>,
    /// Album name, if any.
    pub album: Option<String>,
    /// Playback position in milliseconds, if known.
    pub progress_ms: Option<u64>,
    /// Track duration in milliseconds, if known.
    pub duration_ms: Option<u64>,
}

/// What the app is doing, reported to the backend so it can tell "the user left"
/// from "Android froze the app in the background" (phase 5.2).
///
/// The backend cannot infer this: a frozen app and a dead one both stop reading
/// the now-playing SSE feed, so the app says which one it is on its way out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClientPresence {
    /// The app is on screen.
    Foreground,
    /// The app is backgrounded but alive; Android may freeze it at any moment,
    /// so losing the SSE feed says nothing about the user's intent.
    Background,
    /// The app is closing for good: playback should stop being kept alive for it.
    Gone,
}

/// Body of `POST /client/presence`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenceRequest {
    /// The app's new presence.
    pub presence: ClientPresence,
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

    // Criterion: `SpotifyState` DTO round-trips through serde — every status
    // variant survives serialize -> deserialize.
    #[test]
    fn test_spotify_state_round_trips_through_json() {
        for status in [SpotifyStatus::Stopped, SpotifyStatus::Running] {
            let original = SpotifyState {
                status,
                device_name: "blue2th-PC".to_string(),
            };
            let json = serde_json::to_string(&original).expect("serialize SpotifyState");
            let parsed: SpotifyState =
                serde_json::from_str(&json).expect("deserialize SpotifyState");
            assert_eq!(original, parsed);
        }
    }

    // Criterion: `SpotifyState` DTO round-trips through serde — the status is
    // serialized in lowercase (shared mobile<->server contract).
    #[test]
    fn test_spotify_status_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&SpotifyStatus::Running).expect("serialize"),
            "\"running\""
        );
        assert_eq!(
            serde_json::to_string(&SpotifyStatus::Stopped).expect("serialize"),
            "\"stopped\""
        );
    }

    // Criterion (phase 5.2): `SpotifyAuthState` round-trips through serde for every
    // status variant.
    #[test]
    fn test_spotify_auth_state_round_trips_through_json() {
        for status in [
            SpotifyAuthStatus::Disconnected,
            SpotifyAuthStatus::Connected,
        ] {
            let original = SpotifyAuthState { status };
            let json = serde_json::to_string(&original).expect("serialize SpotifyAuthState");
            let parsed: SpotifyAuthState =
                serde_json::from_str(&json).expect("deserialize SpotifyAuthState");
            assert_eq!(original, parsed);
        }
    }

    // Criterion (phase 5.2): the auth status serializes lowercase (shared contract).
    #[test]
    fn test_spotify_auth_status_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&SpotifyAuthStatus::Connected).expect("serialize"),
            "\"connected\""
        );
        assert_eq!(
            serde_json::to_string(&SpotifyAuthStatus::Disconnected).expect("serialize"),
            "\"disconnected\""
        );
    }

    // Criterion (phase 5.2): the auth request/response DTOs round-trip through serde.
    #[test]
    fn test_auth_url_response_round_trips_through_json() {
        let original = AuthUrlResponse {
            url: "https://accounts.spotify.com/authorize?response_type=code".to_string(),
            state: "csrf-abc123".to_string(),
        };
        let json = serde_json::to_string(&original).expect("serialize AuthUrlResponse");
        let parsed: AuthUrlResponse =
            serde_json::from_str(&json).expect("deserialize AuthUrlResponse");
        assert_eq!(original, parsed);
    }

    // Criterion (phase 5.2): the auth callback request DTO round-trips through serde.
    #[test]
    fn test_auth_callback_request_round_trips_through_json() {
        let original = AuthCallbackRequest {
            code: "auth-code-xyz".to_string(),
            state: "csrf-abc123".to_string(),
        };
        let json = serde_json::to_string(&original).expect("serialize AuthCallbackRequest");
        let parsed: AuthCallbackRequest =
            serde_json::from_str(&json).expect("deserialize AuthCallbackRequest");
        assert_eq!(original, parsed);
    }

    // Criterion (phase 5.2): `NowPlayingState` round-trips and serializes lowercase.
    #[test]
    fn test_now_playing_state_round_trips_and_serializes_lowercase() {
        for state in [
            NowPlayingState::Idle,
            NowPlayingState::Playing,
            NowPlayingState::Paused,
        ] {
            let json = serde_json::to_string(&state).expect("serialize NowPlayingState");
            let parsed: NowPlayingState =
                serde_json::from_str(&json).expect("deserialize NowPlayingState");
            assert_eq!(state, parsed);
        }
        assert_eq!(
            serde_json::to_string(&NowPlayingState::Playing).expect("serialize"),
            "\"playing\""
        );
        assert_eq!(
            serde_json::to_string(&NowPlayingState::Idle).expect("serialize"),
            "\"idle\""
        );
    }

    // Criterion (phase 5.2): `NowPlaying` round-trips through serde with a full
    // track payload (playing) and with an idle payload (all track fields absent).
    #[test]
    fn test_now_playing_round_trips_through_json() {
        let playing = NowPlaying {
            state: NowPlayingState::Playing,
            title: Some("Song".to_string()),
            artist: Some("Artist".to_string()),
            album: Some("Album".to_string()),
            progress_ms: Some(12_000),
            duration_ms: Some(210_000),
        };
        let json = serde_json::to_string(&playing).expect("serialize NowPlaying");
        let parsed: NowPlaying = serde_json::from_str(&json).expect("deserialize NowPlaying");
        assert_eq!(playing, parsed);

        // An idle snapshot (nothing playing) must round-trip too.
        let idle = NowPlaying {
            state: NowPlayingState::Idle,
            title: None,
            artist: None,
            album: None,
            progress_ms: None,
            duration_ms: None,
        };
        let json = serde_json::to_string(&idle).expect("serialize idle NowPlaying");
        let parsed: NowPlaying = serde_json::from_str(&json).expect("deserialize idle NowPlaying");
        assert_eq!(idle, parsed);
    }

    // Criterion (phase 5.2): the presence report round-trips through serde, so the
    // app and the backend agree on the three states the watchdog keys off.
    #[test]
    fn test_presence_request_round_trips_through_json() {
        for presence in [
            ClientPresence::Foreground,
            ClientPresence::Background,
            ClientPresence::Gone,
        ] {
            let request = PresenceRequest { presence };
            let json = serde_json::to_string(&request).expect("serialize PresenceRequest");
            let parsed: PresenceRequest =
                serde_json::from_str(&json).expect("deserialize PresenceRequest");
            assert_eq!(request, parsed);
        }
        // The wire form stays lowercase, as for the other status enums.
        let json = serde_json::to_string(&PresenceRequest {
            presence: ClientPresence::Background,
        })
        .expect("serialize PresenceRequest");
        assert_eq!(json, r#"{"presence":"background"}"#);
    }
}
