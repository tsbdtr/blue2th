// SPDX-License-Identifier: MIT OR Apache-2.0

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
    /// Whether the backend requires `Authorization: Bearer <token>` on every
    /// route but `/health` and `POST /pair` (phase 6.4).
    ///
    /// `/health` stays open precisely so the app can tell "not paired" from
    /// "unreachable": a 200 here with `auth_required` set means the backend is
    /// alive and the app simply has no (or a stale) token. `serde(default)`
    /// keeps a pre-6.4 payload parsing, reading as "no authentication".
    #[serde(default)]
    pub auth_required: bool,
}

impl HealthStatus {
    /// Build an `"ok"` status carrying the given backend version, with no
    /// authentication announced.
    pub fn ok(version: impl Into<String>) -> Self {
        Self {
            status: "ok".to_string(),
            version: version.into(),
            auth_required: false,
        }
    }

    /// Announce whether the backend requires a bearer token.
    pub fn with_auth_required(mut self, required: bool) -> Self {
        self.auth_required = required;
        self
    }
}

/// Custom-scheme prefix of a pairing deep link (phase 6.4).
///
/// The server prints `blue2th://pair?url=…&name=…&code=…` as an ASCII QR; the
/// phone's own camera app routes it to blue2th through the phase 5.2 intent
/// filter, so no camera permission and no scanner live in the app.
pub const PAIR_DEEP_LINK: &str = "blue2th://pair";

/// Put a hand-typed pairing code in the form the server minted it in.
///
/// Codes are drawn from an upper-case alphabet, but Android capitalises only the
/// first character of a text field: `K7m2qx` would otherwise burn one of the five
/// attempts with nothing on screen explaining why. Shared rather than applied on
/// one side only, so a client that skips it still pairs.
pub fn normalize_pairing_code(input: &str) -> String {
    input.trim().to_uppercase()
}

/// Body of `POST /pair` — the short-lived pairing code the operator read off the
/// terminal (typed by hand) or that the QR carried.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairRequest {
    /// The armed pairing code. One-shot, rate limited and short-lived.
    pub code: String,
}

/// Response of a successful `POST /pair` — the long-lived bearer token every
/// other route requires.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairResponse {
    /// The API token the app stores with the backend entry.
    pub token: String,
}

/// What a `blue2th://pair?…` deep link carries.
///
/// It carries the **code**, never the token: the link travels through Android's
/// intent system, where another app declaring the `blue2th` scheme could listen
/// in. A one-shot code that expires makes such an interception worthless.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairLink {
    /// Base URL of the backend, e.g. `http://192.168.1.107:4000`.
    pub url: String,
    /// The backend's own name, when the link carries one. Only `url` and `code`
    /// are required, so a hand-built link without a name still parses.
    pub name: Option<String>,
    /// The short-lived pairing code to exchange on `POST /pair`.
    pub code: String,
}

/// Build the pairing deep link the server renders as a QR. Pure.
///
/// Lives here, next to [`parse_pair_link`], so the server builds exactly what
/// the app parses — the two could not drift apart even if they wanted to.
pub fn pair_deep_link(url: &str, name: &str, code: &str) -> String {
    format!(
        "{PAIR_DEEP_LINK}?url={}&name={}&code={}",
        percent_encode(url),
        percent_encode(name),
        percent_encode(code),
    )
}

/// Percent-encode a query value, keeping only the unreserved set. Hand-written
/// rather than pulled from a crate: this crate must stay target-agnostic and
/// dependency-light, and the rule is three lines. Pure.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            },
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Decode a percent-encoded query value: `%XX` escapes and `+` as a space.
///
/// Decoded from the raw bytes, never by re-slicing the `&str`: a `%` followed by
/// a multi-byte character would split it and panic. An invalid escape is kept
/// as-is rather than dropped, so a malformed value stays visible. Pure.
///
/// Public because the app's own deep-link parser (`src/deep_link.rs`) reads the
/// Spotify OAuth redirect with exactly this rule: one decoder for both custom
/// scheme links, so they cannot drift apart on a mangled escape. It is plain
/// string handling — nothing platform-specific enters the crate with it.
pub fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            },
            b'%' if i + 2 < bytes.len() => {
                match (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2])) {
                    (Some(high), Some(low)) => {
                        out.push((high << 4) | low);
                        i += 3;
                    },
                    _ => {
                        out.push(b'%');
                        i += 1;
                    },
                }
            },
            byte => {
                out.push(byte);
                i += 1;
            },
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The value of a single ASCII hex digit, or `None` if it is not one.
fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Whether a decoded address is usable as a backend base URL: a scheme, a host,
/// and no embedded whitespace. A link carrying anything else is refused rather
/// than stored as an address every later call would fail on. Pure.
fn usable_backend_url(url: &str) -> bool {
    if url.is_empty() || url.chars().any(char::is_whitespace) {
        return false;
    }
    match url.split_once("://") {
        Some((scheme, rest)) => !scheme.is_empty() && !rest.trim_end_matches('/').is_empty(),
        None => false,
    }
}

/// Parse a `blue2th://pair?…` deep link. Pure.
///
/// `None` for anything that is not a pair link, and for a link missing `url` or
/// `code` or carrying a URL with no scheme/host: a malformed link is ignored
/// exactly as an unrelated intent is, never a crash and never a half-created
/// backend entry.
pub fn parse_pair_link(uri: &str) -> Option<PairLink> {
    let rest = uri.strip_prefix(PAIR_DEEP_LINK)?;
    // Tolerate a trailing slash before the query, as the OAuth callback does.
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    let query = rest.strip_prefix('?')?;
    // A fragment is never part of the query.
    let query = query.split('#').next().unwrap_or(query);

    let mut url = None;
    let mut name = None;
    let mut code = None;
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        match key {
            "url" => url = Some(percent_decode(value)),
            "name" => name = Some(percent_decode(value)),
            "code" => code = Some(percent_decode(value)),
            _ => {},
        }
    }

    let url = url.filter(|u| usable_backend_url(u))?;
    let code = code.filter(|c| !c.is_empty())?;
    Some(PairLink {
        url,
        // An empty name is no name: the app keeps whatever it already had.
        name: name.filter(|n| !n.is_empty()),
        code,
    })
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
    /// Whether the backend dials a remembered-but-disconnected speaker back on
    /// its own (phase 6.5). Auto-reconnect only *connects*: the phase 6.3 restore
    /// path re-selects and re-routes. Defaults to on.
    #[serde(default = "auto_reconnect_default")]
    pub auto_reconnect: bool,
}

/// The default for `restore_during_playback`: a returning speaker rejoins on its
/// own, which is the point of the feature. A body that omits the field therefore
/// opts **in**, not out — a bare `serde(default)` would have opted every phase 6.2
/// client out without saying so.
fn restore_during_playback_default() -> bool {
    true
}

/// The default for `auto_reconnect`: the backend dials a remembered speaker back
/// on its own, which is the point of the feature. A body that omits the field
/// therefore opts **in**, not out — a bare `serde(default)` would have silently
/// disabled auto-reconnect for every pre-6.5 client.
fn auto_reconnect_default() -> bool {
    true
}

// ── Phase 6.6: finding the backend on the network ────────────────────────────

/// The mDNS service type the backend advertises and the app browses for.
///
/// Declared here, next to [`discovered_from_txt`], for the same reason
/// `pair_deep_link`/`parse_pair_link` are: the server publishes exactly what the
/// app looks for, so the two cannot drift apart.
pub const SERVICE_TYPE: &str = "_blue2th._tcp.local.";

/// TXT record key carrying the backend's stable id.
pub const TXT_KEY_ID: &str = "id";

/// TXT record key carrying the backend's configured name.
pub const TXT_KEY_NAME: &str = "name";

/// A backend found on the LAN over mDNS.
///
/// The `id` is what makes an entry survive a DHCP lease change: it identifies
/// the *machine*, where the URL only says where it happened to answer. A service
/// with no `id` (hand-rolled or pre-6.6 backend) is still a usable find — it just
/// falls back to URL matching, exactly as before.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredBackend {
    /// The backend's stable id, when it advertises one.
    pub id: Option<String>,
    /// The advertised name, or [`DEFAULT_BACKEND_NAME`] when the record has none.
    pub name: String,
    /// Base URL built from the resolved host and port, e.g. `http://192.168.1.107:4000`.
    pub url: String,
}

/// Build a [`DiscoveredBackend`] from the resolved base URL and the service's TXT
/// records. Pure — plain string handling, nothing platform-specific.
///
/// A missing `name` falls back to [`DEFAULT_BACKEND_NAME`]; a missing, empty or
/// blank `id` yields `None` rather than a rejection, since a backend without an
/// id is still reachable and must not be mistaken for a new machine.
pub fn discovered_from_txt(host_url: &str, txt: &[(&str, &str)]) -> DiscoveredBackend {
    let value = |key: &str| {
        txt.iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.trim())
            .filter(|v| !v.is_empty())
    };
    DiscoveredBackend {
        // A blank id is no id at all: it would match no entry and make a known
        // machine look brand new.
        id: value(TXT_KEY_ID).map(str::to_string),
        name: value(TXT_KEY_NAME)
            .unwrap_or(DEFAULT_BACKEND_NAME)
            .to_string(),
        url: host_url.to_string(),
    }
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
    /// Whether the backend may dial a remembered speaker back by itself. The
    /// default keeps a phase 6.2/6.3 client (which never sends it) working — and
    /// must be **on**, since a bare `serde(default)` would yield `false` and
    /// silently disable auto-reconnect for every such client.
    #[serde(default = "auto_reconnect_default")]
    pub auto_reconnect: bool,
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
            auto_reconnect: true,
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
            auto_reconnect: true,
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

    // ---- phase 6.5: auto-reconnect on the wire ----

    // Criterion: `ServerConfig` carries `auto_reconnect` on the wire and
    // round-trips through serde.
    #[test]
    fn test_server_config_round_trips_with_auto_reconnect() {
        for auto_reconnect in [true, false] {
            let original = ServerConfig {
                name: "Salon".to_string(),
                restore_during_playback: true,
                auto_reconnect,
            };
            let json = serde_json::to_string(&original).expect("serialize ServerConfig");
            let parsed: ServerConfig =
                serde_json::from_str(&json).expect("deserialize ServerConfig");
            assert_eq!(original, parsed);
            assert!(
                json.contains(&format!("\"auto_reconnect\":{auto_reconnect}")),
                "the auto-reconnect flag must be on the wire, got {json}"
            );
        }
    }

    // Criterion: `ConfigRequest` carries `auto_reconnect` on the wire and
    // round-trips through serde — it is what the app pushes to `POST /config`.
    #[test]
    fn test_config_request_round_trips_with_auto_reconnect() {
        for auto_reconnect in [true, false] {
            let original = ConfigRequest {
                name: "Salon".to_string(),
                restore_during_playback: false,
                auto_reconnect,
            };
            let json = serde_json::to_string(&original).expect("serialize ConfigRequest");
            let parsed: ConfigRequest =
                serde_json::from_str(&json).expect("deserialize ConfigRequest");
            assert_eq!(original, parsed);
            assert_eq!(
                parsed.auto_reconnect, auto_reconnect,
                "the flag must survive the round-trip, got {json}"
            );
        }
    }

    // Criterion (non-nominal: a phase 6.2/6.3 client pushes `/config`): a JSON
    // body omitting the field deserializes to `true` — the feature must not
    // silently disable itself for an older app.
    #[test]
    fn test_config_request_without_auto_reconnect_defaults_to_on() {
        let parsed: ConfigRequest =
            serde_json::from_str(r#"{"name":"Salon","restore_during_playback":false}"#)
                .expect("a phase 6.3 body must still parse");
        assert!(
            parsed.auto_reconnect,
            "a body with no auto_reconnect field must leave the feature on"
        );
    }

    // Criterion: `ServerConfig` carries the same default, so a pre-6.5 payload
    // (or on-disk store) decodes with auto-reconnect on.
    #[test]
    fn test_server_config_without_auto_reconnect_defaults_to_on() {
        let parsed: ServerConfig = serde_json::from_str(r#"{"name":"blue2th-PC"}"#)
            .expect("a name-only payload must still parse");
        assert!(parsed.auto_reconnect, "the setting defaults to on");
    }

    // Criterion: the flag is a real boolean on the wire — an explicit `false`
    // is honoured and never overwritten by the default.
    #[test]
    fn test_config_request_explicit_false_auto_reconnect_is_honoured() {
        let parsed: ConfigRequest =
            serde_json::from_str(r#"{"name":"Salon","auto_reconnect":false}"#)
                .expect("an explicit flag must parse");
        assert!(
            !parsed.auto_reconnect,
            "an explicit false must survive the default"
        );
        assert!(
            parsed.restore_during_playback,
            "the phase 6.3 flag keeps its own default"
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

    // ---- phase 6.4: authenticated LAN API with QR or code pairing ----

    /// A well-formed link for the round-trip tests, built by hand rather than
    /// through `pair_deep_link` so a fixture never depends on the function under
    /// test.
    const SAMPLE_URL: &str = "http://192.168.1.107:4000";
    const SAMPLE_NAME: &str = "blue2th-PC";
    const SAMPLE_CODE: &str = "K7M2QX";

    // Criterion: proto — `HealthStatus` gains `auth_required`; it round-trips
    // through serde with the flag set.
    #[test]
    fn test_health_status_round_trips_with_auth_required() {
        let original = HealthStatus::ok("0.1.0").with_auth_required(true);
        let json = serde_json::to_string(&original).expect("serialize HealthStatus");
        assert!(
            json.contains("\"auth_required\":true"),
            "the flag must be on the wire, got {json}"
        );
        let parsed: HealthStatus = serde_json::from_str(&json).expect("deserialize HealthStatus");
        assert_eq!(original, parsed);
        assert!(parsed.auth_required);
    }

    // Criterion: `auth_required` carries `serde(default)` — an old (pre-6.4)
    // `/health` payload must still parse, and read as "no authentication" rather
    // than failing the app's reachability probe outright.
    #[test]
    fn test_health_status_without_auth_required_parses_an_old_payload() {
        let parsed: HealthStatus = serde_json::from_str(r#"{"status":"ok","version":"0.1.0"}"#)
            .expect("a pre-6.4 payload must still parse");
        assert_eq!(parsed.status, "ok");
        assert_eq!(parsed.version, "0.1.0");
        assert!(
            !parsed.auth_required,
            "a payload that never mentioned authentication must not claim it"
        );
    }

    // Criterion: proto — `PairRequest` round-trips through serde.
    // Criterion: a hand-typed code survives the phone keyboard — Android
    // capitalises the first character only, and the alphabet is upper-case.
    #[test]
    fn test_normalize_pairing_code_upper_cases_and_trims() {
        for typed in [" k7m2qx ", "K7m2qx", "k7M2Qx\n", "K7M2QX"] {
            assert_eq!(normalize_pairing_code(typed), "K7M2QX", "typed: {typed:?}");
        }
    }

    #[test]
    fn test_pair_request_round_trips_through_json() {
        let original = PairRequest {
            code: SAMPLE_CODE.to_string(),
        };
        let json = serde_json::to_string(&original).expect("serialize PairRequest");
        assert_eq!(json, format!("{{\"code\":\"{SAMPLE_CODE}\"}}"));
        let parsed: PairRequest = serde_json::from_str(&json).expect("deserialize PairRequest");
        assert_eq!(original, parsed);
    }

    // Criterion: proto — `PairResponse` round-trips through serde.
    #[test]
    fn test_pair_response_round_trips_through_json() {
        let original = PairResponse {
            token: "3P0kq9-token-value_XyZ".to_string(),
        };
        let json = serde_json::to_string(&original).expect("serialize PairResponse");
        let parsed: PairResponse = serde_json::from_str(&json).expect("deserialize PairResponse");
        assert_eq!(original, parsed);
        assert!(json.contains("\"token\""), "got {json}");
    }

    // Criterion: `pair_deep_link(url, name, code)` builds the `blue2th://pair?…`
    // URL, and `parse_pair_link` reads it back — the server builds exactly what
    // the app parses.
    #[test]
    fn test_pair_deep_link_round_trips_through_parse_pair_link() {
        let link = pair_deep_link(SAMPLE_URL, SAMPLE_NAME, SAMPLE_CODE);
        assert!(
            link.starts_with(PAIR_DEEP_LINK),
            "the link must use the pair deep link prefix, got {link}"
        );
        assert_eq!(
            parse_pair_link(&link),
            Some(PairLink {
                url: SAMPLE_URL.to_string(),
                name: Some(SAMPLE_NAME.to_string()),
                code: SAMPLE_CODE.to_string(),
            })
        );
    }

    // Criterion: the QR carries the **code**, never the token — the built link
    // holds the code and nothing that looks like a long-lived credential.
    #[test]
    fn test_pair_deep_link_carries_the_code() {
        let link = pair_deep_link(SAMPLE_URL, SAMPLE_NAME, SAMPLE_CODE);
        assert!(
            link.contains(SAMPLE_CODE),
            "the link must carry the pairing code, got {link}"
        );
        assert!(
            !link.contains("token"),
            "the link must never carry a token, got {link}"
        );
    }

    // Criterion: `parse_pair_link` rejects a link with no `code` — a malformed
    // deep link is ignored, never half-applied.
    #[test]
    fn test_parse_pair_link_rejects_a_missing_code() {
        let uri = format!("{PAIR_DEEP_LINK}?url=http%3A%2F%2F192.168.1.107%3A4000&name=blue2th-PC");
        assert_eq!(parse_pair_link(&uri), None);
    }

    // Criterion: `parse_pair_link` rejects a link with no `url` — there would be
    // no backend to pair with.
    #[test]
    fn test_parse_pair_link_rejects_a_missing_url() {
        let uri = format!("{PAIR_DEEP_LINK}?name=blue2th-PC&code={SAMPLE_CODE}");
        assert_eq!(parse_pair_link(&uri), None);
    }

    // Criterion (non-nominal): a bad URL is refused rather than stored as an
    // address every later call would fail on.
    #[test]
    fn test_parse_pair_link_rejects_a_malformed_url() {
        for bad in [
            "192.168.1.107:4000",
            "http%3A%2F%2F",
            "%20",
            "http%3A%2F%2F%2F",
        ] {
            let uri = format!("{PAIR_DEEP_LINK}?url={bad}&name=blue2th-PC&code={SAMPLE_CODE}");
            assert_eq!(
                parse_pair_link(&uri),
                None,
                "{bad} is not a usable backend address"
            );
        }
    }

    // Criterion (non-nominal): an unrelated intent — the OAuth callback, the bare
    // scheme, the launcher intent — is not a pair link and yields `None`.
    #[test]
    fn test_parse_pair_link_ignores_an_unrelated_uri() {
        for uri in [
            "blue2th://spotify-callback?code=abc&state=xyz",
            "blue2th://",
            "https://example.com/pair?url=http://x&code=ABC123",
            "",
            "not a uri at all",
        ] {
            assert_eq!(parse_pair_link(uri), None, "{uri} must be ignored");
        }
    }

    // Criterion: the link's values are percent-decoded, since that is how the
    // server encodes a URL holding `:` and `/`.
    #[test]
    fn test_parse_pair_link_decodes_percent_encoded_values() {
        let uri = format!(
            "{PAIR_DEEP_LINK}?url=http%3A%2F%2F192.168.1.107%3A4000&name=blue2th-PC&code={SAMPLE_CODE}"
        );
        let link = parse_pair_link(&uri).expect("a percent-encoded link must parse");
        assert_eq!(link.url, SAMPLE_URL);
        assert_eq!(link.name.as_deref(), Some(SAMPLE_NAME));
        assert_eq!(link.code, SAMPLE_CODE);
    }

    // Criterion (non-nominal): a hand-mangled escape must be survived, not
    // panicked on. A `%` followed by a multi-byte character is the case that
    // would panic if the decoder re-sliced the `&str` instead of reading bytes,
    // and a truncated escape at the very end is the one that would index past it.
    #[test]
    fn test_parse_pair_link_survives_a_mangled_percent_escape() {
        for name in ["%é", "%", "%4", "abc%", "%zz", "%C3%A9"] {
            let uri = format!(
                "{PAIR_DEEP_LINK}?url=http%3A%2F%2F192.168.1.107%3A4000&name={name}&code={SAMPLE_CODE}"
            );
            let parsed = parse_pair_link(&uri);
            assert!(parsed.is_some(), "{name} must not stop the link parsing");
            let link = parsed.expect("just asserted");
            assert_eq!(link.url, SAMPLE_URL);
            assert_eq!(link.code, SAMPLE_CODE);
        }
    }

    // Criterion: a well-formed escape decodes to the character it stands for,
    // multi-byte included — the backend name is free text.
    #[test]
    fn test_parse_pair_link_decodes_a_multibyte_name() {
        let encoded = pair_deep_link(SAMPLE_URL, "Salon d'été", SAMPLE_CODE);
        let link = parse_pair_link(&encoded).expect("an accented name must round-trip");
        assert_eq!(link.name.as_deref(), Some("Salon d'été"));
    }

    // ---- phase 6.6: find the backend on the network ----

    // Criterion: proto — a `DiscoveredBackend { id, name, url }` DTO round-trips
    // through JSON, with and without an id.
    #[test]
    fn test_discovered_backend_round_trips_through_json() {
        let found = DiscoveredBackend {
            id: Some("3P0kq9-XyZ_backend-id".to_string()),
            name: "blue2th-PC".to_string(),
            url: SAMPLE_URL.to_string(),
        };
        let json = serde_json::to_string(&found).expect("serialize DiscoveredBackend");
        let parsed: DiscoveredBackend =
            serde_json::from_str(&json).expect("deserialize DiscoveredBackend");
        assert_eq!(found, parsed);

        // A backend that advertises no id must round-trip too — it is a usable
        // find, matched on its URL.
        let anonymous = DiscoveredBackend { id: None, ..found };
        let json = serde_json::to_string(&anonymous).expect("serialize idless DiscoveredBackend");
        let parsed: DiscoveredBackend = serde_json::from_str(&json).expect("deserialize idless");
        assert_eq!(anonymous, parsed);
    }

    // Criterion: `SERVICE_TYPE` (`_blue2th._tcp.local.`) and the TXT keys are
    // declared once in proto, so server and app cannot drift apart.
    #[test]
    fn test_service_type_and_txt_keys_are_declared_once_in_proto() {
        assert_eq!(SERVICE_TYPE, "_blue2th._tcp.local.");
        assert_eq!(TXT_KEY_ID, "id");
        assert_eq!(TXT_KEY_NAME, "name");
    }

    // Criterion: the pure TXT→DTO helper builds the DTO from the TXT records and
    // the resolved host/port — the nominal record carries both keys.
    #[test]
    fn test_discovered_from_txt_reads_the_id_and_the_name() {
        let found = discovered_from_txt(
            SAMPLE_URL,
            &[(TXT_KEY_ID, "backend-id-42"), (TXT_KEY_NAME, "Salon")],
        );
        assert_eq!(
            found,
            DiscoveredBackend {
                id: Some("backend-id-42".to_string()),
                name: "Salon".to_string(),
                url: SAMPLE_URL.to_string(),
            }
        );
    }

    // Criterion: a missing `name` falls back to `DEFAULT_BACKEND_NAME` — a record
    // without one is still listable, it just shows the default label.
    #[test]
    fn test_discovered_from_txt_falls_back_to_the_default_name() {
        let found = discovered_from_txt(SAMPLE_URL, &[(TXT_KEY_ID, "backend-id-42")]);
        assert_eq!(found.name, DEFAULT_BACKEND_NAME);
        assert_eq!(found.id.as_deref(), Some("backend-id-42"));
    }

    // Criterion (non-nominal): a service with no `id` TXT record — hand-rolled or
    // pre-6.6 — yields `id: None`, which is a fallback to URL matching, never a
    // rejection of the find.
    #[test]
    fn test_discovered_from_txt_without_an_id_is_not_a_rejection() {
        let found = discovered_from_txt(SAMPLE_URL, &[(TXT_KEY_NAME, "Salon")]);
        assert_eq!(found.id, None);
        assert_eq!(found.name, "Salon");
        assert_eq!(found.url, SAMPLE_URL);
    }

    // Criterion: a blank/whitespace `id` is treated as absent — an empty string
    // would otherwise match no entry and look like a brand new machine.
    #[test]
    fn test_discovered_from_txt_treats_a_blank_id_as_absent() {
        for blank in ["", " ", "\t", "\n  "] {
            let found = discovered_from_txt(
                SAMPLE_URL,
                &[(TXT_KEY_ID, blank), (TXT_KEY_NAME, SAMPLE_NAME)],
            );
            assert_eq!(found.id, None, "a blank id ({blank:?}) is no id at all");
        }
    }

    // Criterion: a blank/whitespace `name` also falls back to the default label,
    // and unknown TXT keys are ignored rather than refused.
    #[test]
    fn test_discovered_from_txt_ignores_extras_and_blank_names() {
        let found = discovered_from_txt(
            SAMPLE_URL,
            &[
                (TXT_KEY_NAME, "   "),
                (TXT_KEY_ID, "backend-id-42"),
                ("version", "0.1.0"),
            ],
        );
        assert_eq!(found.name, DEFAULT_BACKEND_NAME);
        assert_eq!(found.id.as_deref(), Some("backend-id-42"));
        assert_eq!(found.url, SAMPLE_URL);
    }

    // Criterion: only `url` and `code` are required — a link with no name still
    // parses, and unknown parameters are ignored rather than refused.
    #[test]
    fn test_parse_pair_link_accepts_a_nameless_link_and_ignores_extras() {
        let uri = format!(
            "{PAIR_DEEP_LINK}?url=http%3A%2F%2F192.168.1.107%3A4000&code={SAMPLE_CODE}&v=2"
        );
        assert_eq!(
            parse_pair_link(&uri),
            Some(PairLink {
                url: SAMPLE_URL.to_string(),
                name: None,
                code: SAMPLE_CODE.to_string(),
            })
        );
    }
}
