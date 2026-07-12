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

use base64::Engine as _;
use blue2th_proto::{NowPlaying, NowPlayingState, SpotifyAuthState, SpotifyAuthStatus};
use rand::Rng as _;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

/// OAuth scopes the app requests (space-separated, per the Spotify contract).
pub const SPOTIFY_SCOPES: &str =
    "user-read-playback-state user-read-currently-playing user-modify-playback-state";

/// Spotify authorize endpoint (Authorization Code + PKCE flow).
const AUTHORIZE_ENDPOINT: &str = "https://accounts.spotify.com/authorize";

/// Spotify token endpoint (code exchange + refresh).
const TOKEN_ENDPOINT: &str = "https://accounts.spotify.com/api/token";

/// Spotify Web API base for player/transport calls.
const API_BASE: &str = "https://api.spotify.com/v1";

/// Env var overriding the OAuth client id (defaults to a placeholder for dev/CI).
const CLIENT_ID_ENV: &str = "BLUE2TH_SPOTIFY_CLIENT_ID";

/// Default OAuth client id used when the env var is unset (dev/CI placeholder).
const DEFAULT_CLIENT_ID: &str = "blue2th-spotify-client-id";

/// Env var overriding the OAuth redirect URI (the app's custom scheme).
const REDIRECT_URI_ENV: &str = "BLUE2TH_SPOTIFY_REDIRECT_URI";

/// Default custom-scheme redirect URI the Android app registers.
const DEFAULT_REDIRECT_URI: &str = "blue2th://spotify-callback";

/// Refresh the access token this many seconds before it actually expires.
const REFRESH_SKEW_SECS: u64 = 60;

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
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
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
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("code_challenge_method", "S256")
        .append_pair("code_challenge", challenge)
        .append_pair("state", state)
        .append_pair("scope", scopes)
        .finish();
    format!("{AUTHORIZE_ENDPOINT}?{query}")
}

/// Map a Spotify `/me/player` JSON body to a [`NowPlaying`]. An empty body (or a
/// 204 with no content) maps to [`blue2th_proto::NowPlayingState::Idle`]. Pure.
pub fn parse_now_playing(body: &str) -> NowPlaying {
    // A 204 / empty body means nothing is playing (or no active device).
    if body.trim().is_empty() {
        return idle_now_playing();
    }

    #[derive(Deserialize)]
    struct Artist {
        name: Option<String>,
    }
    #[derive(Deserialize)]
    struct Album {
        name: Option<String>,
    }
    #[derive(Deserialize)]
    struct Item {
        name: Option<String>,
        duration_ms: Option<u64>,
        #[serde(default)]
        artists: Vec<Artist>,
        album: Option<Album>,
    }
    #[derive(Deserialize)]
    struct Player {
        #[serde(default)]
        is_playing: bool,
        progress_ms: Option<u64>,
        item: Option<Item>,
    }

    match serde_json::from_str::<Player>(body) {
        Ok(player) => match player.item {
            Some(item) => NowPlaying {
                state: if player.is_playing {
                    NowPlayingState::Playing
                } else {
                    NowPlayingState::Paused
                },
                title: item.name,
                artist: item.artists.into_iter().find_map(|a| a.name),
                album: item.album.and_then(|a| a.name),
                progress_ms: player.progress_ms,
                duration_ms: item.duration_ms,
            },
            // No track loaded (e.g. `{}`): treat as idle.
            None => idle_now_playing(),
        },
        // A malformed body is treated as idle rather than propagated.
        Err(_) => idle_now_playing(),
    }
}

/// An empty [`NowPlaying`] snapshot (nothing playing).
fn idle_now_playing() -> NowPlaying {
    NowPlaying {
        state: NowPlayingState::Idle,
        title: None,
        artist: None,
        album: None,
        progress_ms: None,
        duration_ms: None,
    }
}

/// Whether the access token needs a refresh: true once `now` is within `skew`
/// seconds of `expires_at` (or already past it). All values are unix seconds. Pure.
pub fn needs_refresh(expires_at: u64, now: u64, skew: u64) -> bool {
    now.saturating_add(skew) >= expires_at
}

/// Map a Spotify Web API HTTP status (and optional `Retry-After`) to a typed
/// [`SpotifyApiError`]. Pure.
pub fn map_api_status(code: u16, retry_after: Option<u64>) -> SpotifyApiError {
    match code {
        401 => SpotifyApiError::Unauthorized,
        403 => SpotifyApiError::PremiumRequired,
        404 => SpotifyApiError::NoActiveDevice,
        429 => SpotifyApiError::RateLimited(retry_after),
        other => SpotifyApiError::Http(other),
    }
}

/// Current unix time in seconds (0 if the clock is before the epoch — never in
/// practice). Used to time token refreshes.
fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Generate a URL-safe random string of `bytes` entropy (base64url, no padding).
/// Used for the PKCE `code_verifier` and the CSRF `state`.
fn random_token(bytes: usize) -> String {
    let mut raw = vec![0u8; bytes];
    rand::thread_rng().fill(&mut raw[..]);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw)
}

/// The OAuth tokens held after a successful code exchange.
#[derive(Clone)]
struct Tokens {
    access_token: String,
    refresh_token: String,
    /// Unix second at which `access_token` expires.
    expires_at: u64,
}

/// A pending authorization: the PKCE verifier and CSRF state issued by
/// `authorize_url`, awaiting the redirect callback.
struct Pending {
    verifier: String,
    state: String,
}

/// The Spotify transport action a handler asks the Web API to perform.
#[derive(Clone, Copy)]
pub enum Transport {
    /// Resume playback (`PUT /me/player/play`).
    Play,
    /// Pause playback (`PUT /me/player/pause`).
    Pause,
    /// Skip to the next track (`POST /me/player/next`).
    Next,
    /// Skip to the previous track (`POST /me/player/previous`).
    Previous,
}

/// Stateful Spotify auth + Web API driver held behind the router's
/// `Arc<Mutex<_>>`. Owns the OAuth config, the pending authorization and the
/// in-memory tokens. The network paths (token exchange/refresh, transport,
/// now-playing) are a manual seam and are not exercised in CI.
pub struct SpotifyAuth {
    client_id: String,
    redirect_uri: String,
    pending: Option<Pending>,
    tokens: Option<Tokens>,
    client: reqwest::Client,
}

impl SpotifyAuth {
    /// Build the auth driver from the environment (client id / redirect uri),
    /// with no pending authorization and no tokens held (Disconnected).
    pub fn new() -> Self {
        Self {
            client_id: std::env::var(CLIENT_ID_ENV)
                .unwrap_or_else(|_| DEFAULT_CLIENT_ID.to_string()),
            redirect_uri: std::env::var(REDIRECT_URI_ENV)
                .unwrap_or_else(|_| DEFAULT_REDIRECT_URI.to_string()),
            pending: None,
            tokens: None,
            client: reqwest::Client::new(),
        }
    }

    /// Coarse auth state observed by the app (Connected iff tokens are held).
    pub fn auth_state(&self) -> SpotifyAuthState {
        let status = if self.tokens.is_some() {
            SpotifyAuthStatus::Connected
        } else {
            SpotifyAuthStatus::Disconnected
        };
        SpotifyAuthState { status }
    }

    /// Mint a fresh PKCE verifier + CSRF state, remember them as pending, and
    /// return the authorize URL the app opens plus the state to echo back.
    pub fn authorize_url(&mut self) -> (String, String) {
        let verifier = random_token(48);
        let state = random_token(24);
        let challenge = pkce_challenge(&verifier);
        let url = build_authorize_url(
            &self.client_id,
            &self.redirect_uri,
            &challenge,
            SPOTIFY_SCOPES,
            &state,
        );
        // Owned copy: `state` is both stored and returned to the caller.
        self.pending = Some(Pending {
            verifier,
            state: state.clone(),
        });
        (url, state)
    }

    /// Exchange the callback `code` (validated against the pending CSRF `state`)
    /// for tokens. The real token endpoint call is a manual network seam.
    pub async fn exchange_code(
        &mut self,
        code: &str,
        state: &str,
    ) -> Result<SpotifyAuthState, SpotifyApiError> {
        let pending = self
            .pending
            .take()
            .filter(|p| p.state == state)
            .ok_or_else(|| SpotifyApiError::Exchange("unknown or mismatched state".to_string()))?;

        let params = [
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", self.redirect_uri.as_str()),
            ("client_id", self.client_id.as_str()),
            ("code_verifier", pending.verifier.as_str()),
        ];
        let tokens = self.request_tokens(&params).await?;
        self.tokens = Some(tokens);
        Ok(self.auth_state())
    }

    /// Drop the held tokens (log out); the next transport call is rejected.
    pub fn disconnect(&mut self) -> SpotifyAuthState {
        self.tokens = None;
        self.auth_state()
    }

    /// Ensure a usable access token, refreshing it if it is within the skew
    /// window. Returns [`SpotifyApiError::NotConnected`] if no tokens are held.
    async fn valid_access_token(&mut self) -> Result<String, SpotifyApiError> {
        let expires_at = match &self.tokens {
            Some(t) => t.expires_at,
            None => return Err(SpotifyApiError::NotConnected),
        };
        if needs_refresh(expires_at, now_unix_secs(), REFRESH_SKEW_SECS) {
            self.refresh().await?;
        }
        match &self.tokens {
            Some(t) => Ok(t.access_token.clone()),
            None => Err(SpotifyApiError::NotConnected),
        }
    }

    /// Refresh the access token using the held refresh token (network seam).
    async fn refresh(&mut self) -> Result<(), SpotifyApiError> {
        let refresh_token = match &self.tokens {
            Some(t) => t.refresh_token.clone(),
            None => return Err(SpotifyApiError::NotConnected),
        };
        let params = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.as_str()),
            ("client_id", self.client_id.as_str()),
        ];
        let mut tokens = self.request_tokens(&params).await?;
        // Spotify may omit a new refresh token; keep the previous one.
        if tokens.refresh_token.is_empty() {
            tokens.refresh_token = refresh_token;
        }
        self.tokens = Some(tokens);
        Ok(())
    }

    /// POST the given form params to the token endpoint and parse the response.
    async fn request_tokens(&self, params: &[(&str, &str)]) -> Result<Tokens, SpotifyApiError> {
        #[derive(Deserialize)]
        struct TokenResponse {
            access_token: String,
            #[serde(default)]
            refresh_token: String,
            expires_in: u64,
        }

        let response = self
            .client
            .post(TOKEN_ENDPOINT)
            .form(params)
            .send()
            .await
            .map_err(|e| SpotifyApiError::Exchange(e.to_string()))?;
        if !response.status().is_success() {
            let code = response.status().as_u16();
            return Err(SpotifyApiError::Exchange(format!(
                "token endpoint HTTP {code}"
            )));
        }
        let body: TokenResponse = response
            .json()
            .await
            .map_err(|e| SpotifyApiError::Exchange(e.to_string()))?;
        Ok(Tokens {
            access_token: body.access_token,
            refresh_token: body.refresh_token,
            expires_at: now_unix_secs().saturating_add(body.expires_in),
        })
    }

    /// Drive a transport action on the Web API. Rejects with
    /// [`SpotifyApiError::NotConnected`] before any network call when no tokens
    /// are held (so a Disconnected transport makes no outbound request).
    pub async fn transport(&mut self, action: Transport) -> Result<(), SpotifyApiError> {
        let token = self.valid_access_token().await?;
        let (method, path) = match action {
            Transport::Play => (reqwest::Method::PUT, "me/player/play"),
            Transport::Pause => (reqwest::Method::PUT, "me/player/pause"),
            Transport::Next => (reqwest::Method::POST, "me/player/next"),
            Transport::Previous => (reqwest::Method::POST, "me/player/previous"),
        };
        let url = format!("{API_BASE}/{path}");
        let response = self
            .client
            .request(method, &url)
            .bearer_auth(token)
            .header(reqwest::header::CONTENT_LENGTH, 0)
            .send()
            .await
            .map_err(|e| SpotifyApiError::Exchange(e.to_string()))?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());
        Err(map_api_status(status.as_u16(), retry_after))
    }

    /// Fetch the current now-playing snapshot from `/me/player`. A 204 (no active
    /// device) maps to an idle snapshot. Rejects if no tokens are held.
    pub async fn now_playing(&mut self) -> Result<NowPlaying, SpotifyApiError> {
        let token = self.valid_access_token().await?;
        let url = format!("{API_BASE}/me/player");
        let response = self
            .client
            .get(&url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| SpotifyApiError::Exchange(e.to_string()))?;
        let status = response.status();
        if status == reqwest::StatusCode::NO_CONTENT {
            return Ok(idle_now_playing());
        }
        if !status.is_success() {
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok());
            return Err(map_api_status(status.as_u16(), retry_after));
        }
        let body = response
            .text()
            .await
            .map_err(|e| SpotifyApiError::Exchange(e.to_string()))?;
        Ok(parse_now_playing(&body))
    }
}

impl Default for SpotifyAuth {
    fn default() -> Self {
        Self::new()
    }
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
