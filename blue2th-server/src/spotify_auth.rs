//! Spotify OAuth (Authorization Code + PKCE) and Web API transport (phase 5.2).
//!
//! The server holds the tokens (in memory), refreshes them silently and drives
//! the Spotify Web API, targeting the `blue2th-PC` Connect device. This module
//! carries the **pure**, unit-testable helpers ([`pkce_challenge`],
//! [`build_authorize_url`], [`parse_now_playing`], [`needs_refresh`],
//! [`map_api_status`]) plus the typed [`SpotifyApiError`]. The stateful token
//! lifecycle / network client is a manual seam (no real HTTP in CI).
//!
//! Phase 5.2 is being written test-first: the helper bodies below are stubs
//! (`todo!()`) so the tests compile and FAIL until the implementer fills them in.

use blue2th_proto::NowPlaying;

/// OAuth scopes the app requests (space-separated, per the Spotify contract).
pub const SPOTIFY_SCOPES: &str =
    "user-read-playback-state user-read-currently-playing user-modify-playback-state";

/// Typed error surfaced by the Spotify Web API layer. Mapped to `AppError` HTTP
/// codes by `From<SpotifyApiError> for AppError` in `lib.rs`.
#[derive(Debug)]
pub enum SpotifyApiError {
    /// A transport call was attempted while no tokens are held (Disconnected).
    NotConnected,
    /// Web API returned 401: the access token is invalid; reauth is required.
    Unauthorized,
    /// Web API returned 403: the account is not Premium (transport forbidden).
    PremiumRequired,
    /// Web API returned 404 / reported no active device.
    NoActiveDevice,
    /// Web API returned 429: rate limited, with the optional `Retry-After` secs.
    RateLimited(Option<u64>),
    /// Token exchange / refresh failed (invalid code, network) — maps to 502.
    Exchange(String),
    /// Any other unexpected HTTP status from the Web API.
    Http(u16),
}

impl std::fmt::Display for SpotifyApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpotifyApiError::NotConnected => {
                write!(f, "Spotify is not connected — log in first")
            },
            SpotifyApiError::Unauthorized => {
                write!(f, "Spotify authorization expired — please reconnect")
            },
            SpotifyApiError::PremiumRequired => write!(f, "Spotify Premium required"),
            SpotifyApiError::NoActiveDevice => write!(f, "no active Spotify device"),
            SpotifyApiError::RateLimited(retry) => {
                write!(f, "Spotify rate limited (retry after {retry:?})")
            },
            SpotifyApiError::Exchange(msg) => write!(f, "Spotify token exchange failed: {msg}"),
            SpotifyApiError::Http(code) => write!(f, "Spotify Web API error (HTTP {code})"),
        }
    }
}

impl std::error::Error for SpotifyApiError {}

/// Derive the PKCE `code_challenge` from a `code_verifier`:
/// `base64url(sha256(verifier))` with no padding (RFC 7636 S256). Pure.
pub fn pkce_challenge(verifier: &str) -> String {
    let _ = verifier;
    todo!("pkce_challenge: base64url(sha256(verifier)) without padding")
}

/// Build the Spotify authorize URL for the Authorization Code + PKCE flow:
/// `accounts.spotify.com/authorize` with `response_type=code`,
/// `code_challenge_method=S256`, and the client id / redirect uri / challenge /
/// state / scopes present and URL-encoded. Pure.
pub fn build_authorize_url(
    client_id: &str,
    redirect_uri: &str,
    challenge: &str,
    scopes: &str,
    state: &str,
) -> String {
    let _ = (client_id, redirect_uri, challenge, scopes, state);
    todo!("build_authorize_url: assemble the PKCE authorize URL")
}

/// Map a Spotify `/me/player` JSON body to a [`NowPlaying`]. An empty body (or a
/// 204 with no content) maps to [`blue2th_proto::NowPlayingState::Idle`]. Pure.
pub fn parse_now_playing(body: &str) -> NowPlaying {
    let _ = body;
    todo!("parse_now_playing: map /me/player JSON to NowPlaying")
}

/// Whether the access token needs a refresh: true once `now` is within `skew`
/// seconds of `expires_at` (or already past it). All values are unix seconds. Pure.
pub fn needs_refresh(expires_at: u64, now: u64, skew: u64) -> bool {
    let _ = (expires_at, now, skew);
    todo!("needs_refresh: now + skew >= expires_at")
}

/// Map a Spotify Web API HTTP status (and optional `Retry-After`) to a typed
/// [`SpotifyApiError`]. Pure.
pub fn map_api_status(code: u16, retry_after: Option<u64>) -> SpotifyApiError {
    let _ = (code, retry_after);
    todo!("map_api_status: 401/403/404/429 -> typed SpotifyApiError")
}

#[cfg(test)]
mod tests {
    use super::*;
    use blue2th_proto::NowPlayingState;

    // Criterion: `pkce_challenge(verifier)` yields base64url(sha256(verifier)) with
    // no padding — the RFC 7636 Appendix B known vector.
    #[test]
    fn test_pkce_challenge_matches_known_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let expected = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert_eq!(pkce_challenge(verifier), expected);
    }

    // Criterion: the PKCE challenge carries no base64 padding (`=`).
    #[test]
    fn test_pkce_challenge_has_no_padding() {
        assert!(!pkce_challenge("some-verifier-value-1234567890").contains('='));
    }

    // Criterion: `build_authorize_url` targets accounts.spotify.com/authorize with
    // response_type=code and code_challenge_method=S256.
    #[test]
    fn test_build_authorize_url_has_authorize_endpoint_and_pkce_params() {
        let url = build_authorize_url(
            "client-123",
            "blue2th://spotify-callback",
            "challenge-xyz",
            SPOTIFY_SCOPES,
            "state-abc",
        );
        assert!(
            url.contains("accounts.spotify.com/authorize"),
            "url must target the authorize endpoint: {url}"
        );
        assert!(
            url.contains("response_type=code"),
            "url must request an authorization code: {url}"
        );
        assert!(
            url.contains("code_challenge_method=S256"),
            "url must use the S256 PKCE method: {url}"
        );
    }

    // Criterion: `build_authorize_url` embeds the client id, challenge and state.
    #[test]
    fn test_build_authorize_url_embeds_client_id_challenge_and_state() {
        let url = build_authorize_url(
            "client-123",
            "blue2th://spotify-callback",
            "challenge-xyz",
            SPOTIFY_SCOPES,
            "state-abc",
        );
        assert!(
            url.contains("client-123"),
            "url must carry the client id: {url}"
        );
        assert!(
            url.contains("challenge-xyz"),
            "url must carry the challenge: {url}"
        );
        assert!(
            url.contains("state-abc"),
            "url must carry the CSRF state: {url}"
        );
    }

    // Criterion: `build_authorize_url` URL-encodes the custom-scheme redirect uri
    // (the `:` / `/` must not appear raw in the query).
    #[test]
    fn test_build_authorize_url_encodes_redirect_uri() {
        let url = build_authorize_url(
            "client-123",
            "blue2th://spotify-callback",
            "challenge-xyz",
            SPOTIFY_SCOPES,
            "state-abc",
        );
        assert!(
            url.contains("blue2th%3A%2F%2Fspotify-callback"),
            "redirect uri must be URL-encoded: {url}"
        );
    }

    // Criterion: a playing `/me/player` fixture maps to a Playing NowPlaying with
    // title, artist, progress and duration.
    #[test]
    fn test_parse_now_playing_maps_playing_fixture() {
        let body = r#"{
            "is_playing": true,
            "progress_ms": 12000,
            "item": {
                "name": "Song",
                "duration_ms": 210000,
                "artists": [{ "name": "Artist" }],
                "album": { "name": "Album" }
            }
        }"#;
        let np = parse_now_playing(body);
        assert_eq!(np.state, NowPlayingState::Playing);
        assert_eq!(np.title.as_deref(), Some("Song"));
        assert_eq!(np.artist.as_deref(), Some("Artist"));
        assert_eq!(np.progress_ms, Some(12_000));
        assert_eq!(np.duration_ms, Some(210_000));
    }

    // Criterion: a paused `/me/player` fixture maps to a Paused NowPlaying.
    #[test]
    fn test_parse_now_playing_maps_paused_fixture() {
        let body = r#"{
            "is_playing": false,
            "progress_ms": 3000,
            "item": {
                "name": "Track",
                "duration_ms": 180000,
                "artists": [{ "name": "Band" }],
                "album": { "name": "Record" }
            }
        }"#;
        let np = parse_now_playing(body);
        assert_eq!(np.state, NowPlayingState::Paused);
        assert_eq!(np.title.as_deref(), Some("Track"));
    }

    // Criterion: an empty body (Web API 204 / no active device) maps to Idle.
    #[test]
    fn test_parse_now_playing_empty_body_is_idle() {
        let np = parse_now_playing("");
        assert_eq!(np.state, NowPlayingState::Idle);
        assert_eq!(np.title, None);
        assert_eq!(np.artist, None);
    }

    // Criterion: `needs_refresh` is true once `now` is past `expires_at`.
    #[test]
    fn test_needs_refresh_true_when_expired() {
        assert!(needs_refresh(100, 200, 10));
    }

    // Criterion: `needs_refresh` is true within the skew window before expiry.
    #[test]
    fn test_needs_refresh_true_within_skew_window() {
        // Expires at 100, now 95, skew 10 -> 95 + 10 >= 100 -> true.
        assert!(needs_refresh(100, 95, 10));
    }

    // Criterion: `needs_refresh` is false while the token is comfortably valid.
    #[test]
    fn test_needs_refresh_false_when_comfortably_valid() {
        assert!(!needs_refresh(1000, 100, 30));
    }

    // Criterion: 401 maps to Unauthorized (reauth / Disconnected).
    #[test]
    fn test_map_api_status_401_is_unauthorized() {
        assert!(matches!(
            map_api_status(401, None),
            SpotifyApiError::Unauthorized
        ));
    }

    // Criterion: 403 maps to PremiumRequired.
    #[test]
    fn test_map_api_status_403_is_premium_required() {
        assert!(matches!(
            map_api_status(403, None),
            SpotifyApiError::PremiumRequired
        ));
    }

    // Criterion: 404 / no device maps to NoActiveDevice.
    #[test]
    fn test_map_api_status_404_is_no_active_device() {
        assert!(matches!(
            map_api_status(404, None),
            SpotifyApiError::NoActiveDevice
        ));
    }

    // Criterion: 429 maps to RateLimited carrying the Retry-After seconds.
    #[test]
    fn test_map_api_status_429_is_rate_limited_with_retry_after() {
        assert!(matches!(
            map_api_status(429, Some(5)),
            SpotifyApiError::RateLimited(Some(5))
        ));
    }
}
