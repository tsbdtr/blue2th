// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared data-transfer types for the blue2th mobile app <-> PC backend contract.
//!
//! Keep this crate target-agnostic (no platform-specific deps): it is compiled
//! both into the Android app and the Linux backend. The contract it carries is
//! described in `docs/ARCHITECTURE.md`.

use serde::{Deserialize, Serialize};

/// Health/version payload returned by the backend `GET /health` endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthStatus {
    /// Liveness marker, e.g. `"ok"`.
    pub status: String,
    /// Version of the backend's *release* — the workspace version the app, the
    /// backend and this crate share (see the root `Cargo.toml`).
    ///
    /// Informational only. It says which release a backend comes from, never
    /// whether this app can talk to it: two binaries published together are not
    /// two binaries running together, since the phone and the PC are updated by
    /// hand at different times. Wire compatibility gets its own field (#33).
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
    /// Newest wire contract this backend speaks — its own [`PROTOCOL_VERSION`].
    ///
    /// Separate from `version` on purpose: the release says which build a
    /// backend comes from, this says what it can talk to. `serde(default)`
    /// reads a backend predating the mechanism as `0`, which fails the upper
    /// bound and is reported as "update the backend".
    #[serde(default)]
    pub protocol: u32,
    /// Oldest client contract this backend still serves — its own
    /// [`MIN_SUPPORTED_PROTOCOL_VERSION`]. A client below it must update.
    #[serde(default)]
    pub protocol_min: u32,
}

/// The wire contract this build of the app/backend speaks.
///
/// Bumped whenever the HTTP surface changes in a way an older peer cannot
/// follow. Compiled into both sides, so the comparison never depends on what a
/// payload claims about itself.
pub const PROTOCOL_VERSION: u32 = 1;

/// The oldest client contract a backend built from this source still serves.
pub const MIN_SUPPORTED_PROTOCOL_VERSION: u32 = 1;

/// Which of the two machines is behind, when the app and the backend cannot
/// agree on a wire contract.
///
/// Typed rather than a formatted string: the app has to name *which machine to
/// update*, and a message built in the client layer cannot be matched on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolMismatch {
    /// The backend speaks an older contract than the client — update the
    /// backend on the server.
    BackendTooOld,
    /// The backend dropped support for a client this old — update the app on
    /// the phone.
    BackendTooNew,
}

/// Check a client contract against the range a backend announces. Pure.
///
/// Accepts `health.protocol_min <= client <= health.protocol`, bounds included.
pub fn check_protocol(health: &HealthStatus, client: u32) -> Result<(), ProtocolMismatch> {
    if client > health.protocol {
        return Err(ProtocolMismatch::BackendTooOld);
    }
    if client < health.protocol_min {
        return Err(ProtocolMismatch::BackendTooNew);
    }
    Ok(())
}

impl HealthStatus {
    /// Build an `"ok"` status carrying the given backend version, with no
    /// authentication announced.
    pub fn ok(version: impl Into<String>) -> Self {
        Self {
            status: "ok".to_string(),
            version: version.into(),
            auth_required: false,
            // Filled from the compiled constants, never by hand: a backend
            // cannot forget to announce the contract it was built with.
            protocol: PROTOCOL_VERSION,
            protocol_min: MIN_SUPPORTED_PROTOCOL_VERSION,
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

/// Whether the backend could read the audio graph when it answered (#145).
///
/// The default is `Responsive`: a reply that says nothing about the graph —
/// one from a backend built before this field — is not a stall.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioGraphStatus {
    /// The graph answered.
    #[default]
    Responsive,
    /// The graph did not answer, or could not be read.
    Unresponsive,
}

/// Current playback state returned by the transport endpoints.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaybackState {
    /// Whether the engine is stopped, playing or paused.
    pub status: PlaybackStatus,
    /// Current sink volume in `0.0..=1.0`.
    pub volume: f32,
    /// Whether the audio graph answered this request. Defaulted so a body
    /// from a backend built before this field still decodes.
    #[serde(default)]
    pub audio_graph: AudioGraphStatus,
}

/// Body of `POST /volume` — the desired sink volume level.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VolumeRequest {
    /// Desired volume; the backend clamps it to `0.0..=1.0`.
    pub level: f32,
}

/// Body of `POST /spotify/volume` — the desired Spotify Connect level (#58).
///
/// A percent, like the Web API's own `volume_percent`, rather than the
/// `0.0..=1.0` of [`VolumeRequest`]: this one is `librespot`'s level, applied
/// before PipeWire sees a sample, and it is what the app reads back in
/// [`NowPlaying::volume_percent`]. The backend refuses anything above 100.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpotifyVolumeRequest {
    /// Desired level in `0..=100`.
    pub percent: u8,
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

/// How many playback targets the selection carries, as `GET /targets` reports it.
///
/// Named for a routing decision the backend no longer makes: a lone target used
/// to feed its own sink directly while two went through a combined sink (#70).
/// The variants report the selection count; the names and their lowercase wire
/// values are kept, since clients parse them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RoutingMode {
    /// No target selected: nothing to play.
    Idle,
    /// Exactly one target selected.
    Single,
    /// Two targets selected.
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
    /// The Connect device's volume in `0..=100`, from `device.volume_percent`
    /// (#58). `None` when no device is active or the field is absent — never 0,
    /// since an absent level is not a silent one. Defaulted so a backend that
    /// predates the field still decodes.
    #[serde(default)]
    pub volume_percent: Option<u8>,
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
    /// Whether the Spotify Connect level is pinned to 100 (#58): with the lock
    /// on, the backend re-asserts 100 whenever a poll sees anything else and
    /// refuses `POST /spotify/volume`. Defaults to **off** — a bare
    /// `serde(default)` is right here, since pinning is an opt-in a client that
    /// predates the field must not make by omission.
    #[serde(default)]
    pub spotify_volume_lock: bool,
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
    /// Whether to pin the Spotify Connect level to 100 (#58). `None` when the
    /// client did not send it, and then the backend **leaves the stored value
    /// alone**: the app re-pushes its whole config on every activation, so a
    /// client that does not know the field would otherwise switch the guard
    /// off each time it comes to the foreground.
    #[serde(default)]
    pub spotify_volume_lock: Option<bool>,
}

#[cfg(test)]
mod tests;
