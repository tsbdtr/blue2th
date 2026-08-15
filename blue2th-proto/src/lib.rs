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

/// Hard cap on a backend name (phase 6.2). The value ends up as a
/// `librespot --name` argument and as the string the Spotify Web API device
/// lookup matches on, so it stays short and boring.
pub const MAX_BACKEND_NAME_LEN: usize = 12;

/// The name a backend answers to until the app configures another one.
pub const DEFAULT_BACKEND_NAME: &str = "blue2th-PC";

/// Why a backend name was refused. The app rejects at save time and the server
/// re-validates on `POST /config` (unauthenticated on the LAN), so both layers
/// speak the same vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameError {
    /// Empty, or nothing but whitespace.
    Empty,
    /// Longer than [`MAX_BACKEND_NAME_LEN`].
    TooLong,
    /// Does not start with an ASCII letter (`2salon`, `-salon`, `_salon`).
    BadStart,
    /// Holds a character outside `[A-Za-z0-9_-]` (space, accent, emoji, `!`).
    BadChar,
}

impl std::fmt::Display for NameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NameError::Empty => write!(f, "the name cannot be empty"),
            NameError::TooLong => write!(
                f,
                "the name cannot exceed {MAX_BACKEND_NAME_LEN} characters"
            ),
            NameError::BadStart => write!(f, "the name must start with a letter (a-z, A-Z)"),
            NameError::BadChar => write!(
                f,
                "the name may only hold letters, digits, '-' and '_' (no space or accent)"
            ),
        }
    }
}

impl std::error::Error for NameError {}

/// Validate a backend name, returning the trimmed value.
///
/// The rule: starts with an ASCII letter, then only ASCII letters, digits, `-`
/// or `_`, at most [`MAX_BACKEND_NAME_LEN`] characters. It lives here precisely
/// because the app and the server must apply the *same* rule — duplicating it
/// would let them drift, and the server cannot trust the client.
pub fn validate_backend_name(raw: &str) -> Result<String, NameError> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(NameError::Empty);
    }
    if name.chars().count() > MAX_BACKEND_NAME_LEN {
        return Err(NameError::TooLong);
    }
    let mut chars = name.chars();
    // Checked before the character scan so `2salon` reports the start rule
    // rather than a generic "bad character".
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() => {},
        _ => return Err(NameError::BadStart),
    }
    if chars.any(|c| !(c.is_ascii_alphanumeric() || c == '-' || c == '_')) {
        return Err(NameError::BadChar);
    }
    Ok(name.to_string())
}

/// The name a backend advertises, returned by `GET /config`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerConfig {
    /// The configured backend name (also its Spotify Connect device name).
    pub name: String,
    /// Whether a remembered speaker that comes back mid-playback is re-selected
    /// straight away (phase 6.3). Opting in accepts a brief cut, since moving the
    /// target sink respawns `librespot`. Defaults to on.
    #[serde(default = "restore_during_playback_default")]
    pub restore_during_playback: bool,
}

/// The default for `restore_during_playback`: a returning speaker rejoins on its
/// own, which is the point of the feature. A body that omits the field therefore
/// opts **in**, not out — a bare `serde(default)` would have opted every phase 6.2
/// client out without saying so.
fn restore_during_playback_default() -> bool {
    true
}

/// Body of `POST /config` — the name the app pushes to the backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigRequest {
    /// The desired backend name; the server re-validates and trims it.
    pub name: String,
    /// Whether the backend may restore a returning speaker while playback runs.
    /// The default keeps a phase 6.2 client (name only) working — and must be
    /// **on**, since a bare `serde(default)` would yield `false` and silently
    /// disable restoration for every such client.
    #[serde(default = "restore_during_playback_default")]
    pub restore_during_playback: bool,
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

    // Criterion (phase 6.2 + 6.3): the config DTO (`ServerConfig`) round-trips
    // through serde, restore flag included.
    #[test]
    fn test_server_config_round_trips_through_json() {
        let original = ServerConfig {
            name: "Salon".to_string(),
            restore_during_playback: false,
        };
        let json = serde_json::to_string(&original).expect("serialize ServerConfig");
        let parsed: ServerConfig = serde_json::from_str(&json).expect("deserialize ServerConfig");
        assert_eq!(original, parsed);
        assert!(
            json.contains("\"name\":\"Salon\""),
            "the name must stay on the wire, got {json}"
        );
        assert!(
            json.contains("\"restore_during_playback\":false"),
            "the restore flag must be on the wire, got {json}"
        );
    }

    // Criterion (phase 6.2 + 6.3): the config request DTO (`ConfigRequest`)
    // round-trips through serde — it is what the app pushes to `POST /config`.
    #[test]
    fn test_config_request_round_trips_through_json() {
        let original = ConfigRequest {
            name: "blue2th-PC".to_string(),
            restore_during_playback: true,
        };
        let json = serde_json::to_string(&original).expect("serialize ConfigRequest");
        let parsed: ConfigRequest = serde_json::from_str(&json).expect("deserialize ConfigRequest");
        assert_eq!(original, parsed);
        assert!(
            parsed.restore_during_playback,
            "the flag must survive the round-trip, got {json}"
        );
    }

    // Criterion (phase 6.3): `ConfigRequest` gains `restore_during_playback` with
    // `serde(default)` — a phase 6.2 client pushing only a name must still parse
    // (non-nominal: old client, new server), and the default is **on**.
    #[test]
    fn test_config_request_without_the_flag_defaults_to_restoring() {
        let parsed: ConfigRequest =
            serde_json::from_str(r#"{"name":"Salon"}"#).expect("a name-only body must still parse");
        assert_eq!(parsed.name, "Salon");
        assert!(
            parsed.restore_during_playback,
            "the setting defaults to on, so a name-only body must not silently disable restoration"
        );
    }

    // Criterion (phase 6.3): `ServerConfig` carries the same `serde(default)`, so
    // a phase 6.2-era payload (or on-disk store) still decodes, restoration on.
    #[test]
    fn test_server_config_without_the_flag_defaults_to_restoring() {
        let parsed: ServerConfig = serde_json::from_str(r#"{"name":"blue2th-PC"}"#)
            .expect("a name-only payload must still parse");
        assert_eq!(parsed.name, "blue2th-PC");
        assert!(parsed.restore_during_playback, "the setting defaults to on");
    }

    // Criterion (phase 6.3): the flag is a real boolean on the wire — an explicit
    // `false` is honoured and never overwritten by the default.
    #[test]
    fn test_config_request_explicit_false_is_honoured() {
        let parsed: ConfigRequest =
            serde_json::from_str(r#"{"name":"Salon","restore_during_playback":false}"#)
                .expect("an explicit flag must parse");
        assert!(
            !parsed.restore_during_playback,
            "an explicit false must survive the default"
        );
    }

    // Criterion (phase 6.2): `validate_backend_name` accepts `Salon`, `blue2th-PC`,
    // `salon_tv` and `pc2` — a leading ASCII letter then letters/digits/`-`/`_`.
    #[test]
    fn test_validate_backend_name_accepts_the_allowed_shapes() {
        for name in ["Salon", "blue2th-PC", "salon_tv", "pc2"] {
            assert_eq!(
                validate_backend_name(name),
                Ok(name.to_string()),
                "{name} must be accepted"
            );
        }
    }

    // Criterion (phase 6.2): an empty or blank name is rejected — an unnamed
    // backend would show as nothing at all in the status encart.
    #[test]
    fn test_validate_backend_name_rejects_empty_and_blank() {
        assert_eq!(validate_backend_name(""), Err(NameError::Empty));
        assert_eq!(validate_backend_name("   "), Err(NameError::Empty));
        assert_eq!(validate_backend_name("\t\n"), Err(NameError::Empty));
    }

    // Criterion (phase 6.2): a name starting with a digit or a separator is
    // rejected (`2salon`, `-salon`, `_salon`).
    #[test]
    fn test_validate_backend_name_rejects_a_non_letter_start() {
        for name in ["2salon", "-salon", "_salon"] {
            assert_eq!(
                validate_backend_name(name),
                Err(NameError::BadStart),
                "{name} must be rejected: a name starts with a letter"
            );
        }
    }

    // Criterion (phase 6.2): a space, an accent, an emoji or punctuation is
    // rejected — the value becomes a `librespot --name` argv entry and the string
    // the Web API device lookup matches on.
    #[test]
    fn test_validate_backend_name_rejects_disallowed_characters() {
        for name in ["salon tv", "séjour", "salon!", "salon\u{1F3B5}", "sa/lon"] {
            assert_eq!(
                validate_backend_name(name),
                Err(NameError::BadChar),
                "{name} must be rejected: only ASCII letters, digits, - and _"
            );
        }
    }

    // Criterion (phase 6.2): the cap is `MAX_BACKEND_NAME_LEN` — exactly that many
    // characters is accepted, one more is refused.
    #[test]
    fn test_validate_backend_name_enforces_the_length_cap() {
        let at_cap: String = "a".repeat(MAX_BACKEND_NAME_LEN);
        assert_eq!(validate_backend_name(&at_cap), Ok(at_cap.clone()));

        let over_cap: String = "a".repeat(MAX_BACKEND_NAME_LEN + 1);
        assert_eq!(validate_backend_name(&over_cap), Err(NameError::TooLong));
    }

    // Criterion (phase 6.2): the validator trims, and the trimmed value is what
    // comes back (that is what gets stored and pushed to the backend).
    #[test]
    fn test_validate_backend_name_returns_the_trimmed_value() {
        assert_eq!(validate_backend_name("  Salon \n"), Ok("Salon".to_string()));
    }

    // Criterion (phase 6.2): the default name `blue2th-PC` satisfies its own
    // validator — a server that never got configured must not hold a name its own
    // rule would refuse.
    #[test]
    fn test_default_backend_name_satisfies_the_validator() {
        assert_eq!(
            validate_backend_name(DEFAULT_BACKEND_NAME),
            Ok(DEFAULT_BACKEND_NAME.to_string())
        );
        assert!(DEFAULT_BACKEND_NAME.len() <= MAX_BACKEND_NAME_LEN);
    }
}
