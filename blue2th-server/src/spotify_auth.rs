// SPDX-License-Identifier: MIT OR Apache-2.0

//! Spotify OAuth (Authorization Code + PKCE) and Web API transport (phase 5.2).
//!
//! The server holds the tokens (in memory), refreshes them silently and drives
//! the Spotify Web API, targeting the `blue2th-PC` Connect device. This module
//! carries the **pure**, unit-testable helpers ([`pkce_challenge`],
//! [`build_authorize_url`], [`parse_now_playing`], [`needs_refresh`],
//! [`map_api_status`]) plus the typed [`SpotifyApiError`]. The stateful token
//! lifecycle / network client is a manual seam (no real HTTP in CI).

use base64::Engine as _;
use blue2th_proto::{NowPlaying, NowPlayingState, SpotifyAuthState, SpotifyAuthStatus};
use rand::Rng as _;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use crate::spotify::SPOTIFY_DEVICE_NAME;

/// OAuth scopes the app requests (space-separated, per the Spotify contract).
pub const SPOTIFY_SCOPES: &str =
    "user-read-playback-state user-read-currently-playing user-modify-playback-state";

/// Spotify authorize endpoint (Authorization Code + PKCE flow).
const AUTHORIZE_ENDPOINT: &str = "https://accounts.spotify.com/authorize";

/// Spotify token endpoint (code exchange + refresh).
const TOKEN_ENDPOINT: &str = "https://accounts.spotify.com/api/token";

/// Spotify Web API base for player/transport calls.
const API_BASE: &str = "https://api.spotify.com/v1";

/// Env var carrying the OAuth client id. Required: there is no default.
const CLIENT_ID_ENV: &str = "BLUE2TH_SPOTIFY_CLIENT_ID";

/// Env var overriding the OAuth redirect URI (the app's custom scheme).
const REDIRECT_URI_ENV: &str = "BLUE2TH_SPOTIFY_REDIRECT_URI";

/// Default custom-scheme redirect URI the Android app registers.
const DEFAULT_REDIRECT_URI: &str = "blue2th://spotify-callback";

/// Refresh the access token this many seconds before it actually expires.
const REFRESH_SKEW_SECS: u64 = 60;

/// File holding the persisted refresh token, under the app's state directory.
const TOKEN_STORE_FILE: &str = "spotify-token.json";

/// Typed error surfaced by the Spotify Web API layer. Mapped to `AppError` HTTP
/// codes by `From<SpotifyApiError> for AppError` in `lib.rs`.
#[derive(Debug)]
pub enum SpotifyApiError {
    /// No OAuth client id is configured, so no Spotify call can be made.
    NotConfigured,
    /// A transport call was attempted while no tokens are held (Disconnected).
    NotConnected,
    /// The `blue2th-PC` Connect device is absent from the account's device list,
    /// i.e. the phase 5.1 librespot backend is not running.
    BackendNotRunning,
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
            SpotifyApiError::NotConfigured => write!(
                f,
                "Spotify client id not configured — start the backend with {CLIENT_ID_ENV}=<your client id>"
            ),
            SpotifyApiError::NotConnected => {
                write!(f, "Spotify is not connected — log in first")
            },
            // Deliberately name-free: the Connect device is named by the app
            // (phase 6.2), so spelling the default here would lie after a rename.
            SpotifyApiError::BackendNotRunning => write!(
                f,
                "the blue2th Connect device is not available — start the Spotify backend first"
            ),
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

/// Path of the file holding the refresh token: `$XDG_STATE_HOME/blue2th/` (or
/// `~/.local/state/blue2th/`). `None` when neither variable is set, in which case
/// tokens simply stay in memory as before.
fn token_store_path() -> Option<std::path::PathBuf> {
    crate::state_store::state_store_path(TOKEN_STORE_FILE)
}

/// Read the persisted refresh token, if any. A missing or unreadable file just
/// means "not logged in yet" — never an error worth failing startup over.
fn load_refresh_token(path: Option<&std::path::Path>) -> Option<String> {
    let raw = std::fs::read_to_string(path?).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    value
        .get("refresh_token")?
        .as_str()
        .map(str::to_string)
        .filter(|token| !token.is_empty())
}

/// Persist the refresh token with owner-only permissions. Failures are reported
/// to the caller, which logs them: losing persistence must never break playback.
fn save_refresh_token(path: &std::path::Path, refresh_token: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::json!({ "refresh_token": refresh_token }).to_string();
    std::fs::write(path, body)?;
    // The refresh token is a long-lived credential: keep it readable by its owner
    // only, and set the mode after writing so it applies to an existing file too.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// A Spotify Connect device as reported by `GET /me/player/devices`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// Spotify's opaque device id, used to target transport calls.
    pub id: String,
    /// Whether playback is currently happening on this device.
    pub is_active: bool,
}

/// Find a Connect device by its exact name in a `/me/player/devices` payload.
/// Returns `None` when the body is malformed or the device is absent (its id
/// may also be null while the device is initialising). Pure.
pub fn find_device(body: &str, name: &str) -> Option<Device> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    value
        .get("devices")?
        .as_array()?
        .iter()
        .find(|device| device.get("name").and_then(|n| n.as_str()) == Some(name))
        .and_then(|device| {
            Some(Device {
                id: device.get("id")?.as_str()?.to_string(),
                is_active: device
                    .get("is_active")
                    .and_then(|a| a.as_bool())
                    .unwrap_or(false),
            })
        })
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

/// Why a token endpoint call failed. `Rejected` means Spotify refused the grant
/// itself (used-up code, revoked refresh token) — the only case where dropping
/// the stored credential is right. A network blip must never cost a login.
enum TokenEndpointError {
    /// The endpoint answered with a non-success status.
    Rejected(u16),
    /// The call never completed (network, malformed body).
    Failed(String),
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
    /// `None` when `BLUE2TH_SPOTIFY_CLIENT_ID` is unset or blank: there is no
    /// usable fallback, so every OAuth path fails loudly instead of sending
    /// Spotify a placeholder id and getting an opaque `invalid_client` back.
    client_id: Option<String>,
    redirect_uri: String,
    pending: Option<Pending>,
    tokens: Option<Tokens>,
    /// Where the refresh token is persisted, or `None` to keep it in memory only.
    store: Option<std::path::PathBuf>,
    /// The Connect device name the Web API lookup matches on (phase 6.2): the
    /// name the app configured, defaulting to [`SPOTIFY_DEVICE_NAME`].
    device_name: String,
    client: reqwest::Client,
}

impl SpotifyAuth {
    /// Build the auth driver from the environment (client id / redirect uri),
    /// with no pending authorization and no tokens held (Disconnected).
    pub fn new() -> Self {
        let mut auth = Self::with_config(
            std::env::var(CLIENT_ID_ENV).ok(),
            std::env::var(REDIRECT_URI_ENV).unwrap_or_else(|_| DEFAULT_REDIRECT_URI.to_string()),
        );
        // Only an env-built driver persists: `with_config` (tests) stays off-disk.
        let store = token_store_path();
        // A stored refresh token restores Connected across restarts. The access
        // token is not kept — it lives an hour — so an empty one that "expired at
        // 0" forces a refresh on the first call.
        if let Some(refresh_token) = load_refresh_token(store.as_deref()) {
            auth.tokens = Some(Tokens {
                access_token: String::new(),
                refresh_token,
                expires_at: 0,
            });
        }
        auth.store = store;
        auth
    }

    /// Build the driver from an explicit config, so the OAuth paths can be
    /// exercised without touching the process environment.
    pub fn with_config(client_id: Option<String>, redirect_uri: String) -> Self {
        Self {
            // A blank value is as unusable as an absent one.
            client_id: client_id.filter(|id| !id.trim().is_empty()),
            redirect_uri,
            pending: None,
            tokens: None,
            // No store: an explicitly configured driver never touches the disk,
            // so tests can never read or delete the real refresh token.
            store: None,
            device_name: SPOTIFY_DEVICE_NAME.to_string(),
            client: reqwest::Client::new(),
        }
    }

    /// The configured client id, or [`SpotifyApiError::NotConfigured`].
    fn client_id(&self) -> Result<&str, SpotifyApiError> {
        self.client_id
            .as_deref()
            .ok_or(SpotifyApiError::NotConfigured)
    }

    /// Adopt the configured Connect device name (phase 6.2). The Web API device
    /// lookup must search for the name `librespot` actually advertises: keeping
    /// the constant here while the backend was renamed makes every transport
    /// call fail with a 412 that blames the user for not starting the backend.
    pub fn set_device_name(&mut self, device_name: &str) {
        self.device_name = device_name.to_string();
    }

    /// The Connect device name the Web API lookup matches on.
    pub fn device_name(&self) -> &str {
        &self.device_name
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
    pub fn authorize_url(&mut self) -> Result<(String, String), SpotifyApiError> {
        let client_id = self.client_id()?;
        let verifier = random_token(48);
        let state = random_token(24);
        let challenge = pkce_challenge(&verifier);
        let url = build_authorize_url(
            client_id,
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
        Ok((url, state))
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
            ("client_id", self.client_id()?),
            ("code_verifier", pending.verifier.as_str()),
        ];
        let tokens = self.request_tokens(&params).await.map_err(|e| match e {
            TokenEndpointError::Rejected(code) => {
                SpotifyApiError::Exchange(format!("token endpoint HTTP {code}"))
            },
            TokenEndpointError::Failed(msg) => SpotifyApiError::Exchange(msg),
        })?;
        self.tokens = Some(tokens);
        // Persist now: this is the only moment a brand-new refresh token exists.
        self.persist_refresh_token();
        Ok(self.auth_state())
    }

    /// Drop the held tokens (log out); the next transport call is rejected.
    pub fn disconnect(&mut self) -> SpotifyAuthState {
        // Also drop the persisted credential: an explicit log out must not be
        // undone by the next server restart.
        self.forget_tokens();
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
            // Clone is required: return an owned token while `self` stays borrowed.
            Some(t) => Ok(t.access_token.clone()),
            None => Err(SpotifyApiError::NotConnected),
        }
    }

    /// Refresh the access token using the held refresh token (network seam).
    async fn refresh(&mut self) -> Result<(), SpotifyApiError> {
        let refresh_token = match &self.tokens {
            // Clone is required: the token is reused below after `self.tokens` is
            // reassigned, so it cannot stay borrowed from `self`.
            Some(t) => t.refresh_token.clone(),
            None => return Err(SpotifyApiError::NotConnected),
        };
        let params = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.as_str()),
            ("client_id", self.client_id()?),
        ];
        let mut tokens = match self.request_tokens(&params).await {
            Ok(tokens) => tokens,
            // Spotify refused the refresh token (revoked, or the user changed the
            // password): forget it so the app prompts a re-login rather than
            // retrying a grant that will keep failing.
            Err(TokenEndpointError::Rejected(_)) => {
                self.forget_tokens();
                return Err(SpotifyApiError::Unauthorized);
            },
            // A network failure leaves the credential alone: it is very likely
            // still valid, and losing it would force a browser round-trip.
            Err(TokenEndpointError::Failed(msg)) => return Err(SpotifyApiError::Exchange(msg)),
        };
        // Spotify may omit a new refresh token; keep the previous one.
        if tokens.refresh_token.is_empty() {
            tokens.refresh_token = refresh_token;
        }
        self.tokens = Some(tokens);
        // Spotify may hand out a rotated refresh token; persist whatever we hold.
        self.persist_refresh_token();
        Ok(())
    }

    /// Persist the refresh token, if this driver has a store. A write failure is
    /// logged, never propagated: the session stays usable, only the "no re-login
    /// after restart" convenience is lost.
    fn persist_refresh_token(&self) {
        let (Some(path), Some(tokens)) = (self.store.as_deref(), self.tokens.as_ref()) else {
            return;
        };
        if let Err(e) = save_refresh_token(path, &tokens.refresh_token) {
            tracing::warn!("could not persist the Spotify refresh token: {e}");
        }
    }

    /// Drop the tokens and the persisted credential — used when Spotify itself
    /// rejects the grant, so the next call prompts a fresh login instead of
    /// retrying something that will keep failing.
    fn forget_tokens(&mut self) {
        self.tokens = None;
        if let Some(path) = self.store.as_deref() {
            if let Err(e) = std::fs::remove_file(path) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!("could not clear the stored Spotify refresh token: {e}");
                }
            }
        }
    }

    /// POST the given form params to the token endpoint and parse the response.
    async fn request_tokens(&self, params: &[(&str, &str)]) -> Result<Tokens, TokenEndpointError> {
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
            .map_err(|e| TokenEndpointError::Failed(e.to_string()))?;
        if !response.status().is_success() {
            return Err(TokenEndpointError::Rejected(response.status().as_u16()));
        }
        let body: TokenResponse = response
            .json()
            .await
            .map_err(|e| TokenEndpointError::Failed(e.to_string()))?;
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
        // Target our own Connect endpoint, never "whatever is currently active":
        // otherwise the command drives the phone (or any other device) and the
        // audio never reaches the speakers wired to the PC backend.
        let device = self.blue2th_device().await?;
        if !device.is_active {
            self.transfer_playback(&device.id, matches!(action, Transport::Play))
                .await?;
        }
        let (method, path) = match action {
            Transport::Play => (reqwest::Method::PUT, "me/player/play"),
            Transport::Pause => (reqwest::Method::PUT, "me/player/pause"),
            Transport::Next => (reqwest::Method::POST, "me/player/next"),
            Transport::Previous => (reqwest::Method::POST, "me/player/previous"),
        };
        let url = format!("{API_BASE}/{path}?device_id={}", device.id);
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

    /// Resolve the `blue2th-PC` Connect device from `GET /me/player/devices`.
    /// Absent from the list means the phase 5.1 librespot backend is not running.
    async fn blue2th_device(&mut self) -> Result<Device, SpotifyApiError> {
        let token = self.valid_access_token().await?;
        let url = format!("{API_BASE}/me/player/devices");
        let response = self
            .client
            .get(&url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| SpotifyApiError::Exchange(e.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(map_api_status(status.as_u16(), None));
        }
        let body = response
            .text()
            .await
            .map_err(|e| SpotifyApiError::Exchange(e.to_string()))?;
        // The *configured* name, never the constant: once the app renamed the
        // backend, librespot advertises the new name and a hard-coded lookup
        // would 412 every transport call.
        find_device(&body, &self.device_name).ok_or(SpotifyApiError::BackendNotRunning)
    }

    /// Move playback to `device_id` (`PUT /me/player`), optionally starting it.
    async fn transfer_playback(
        &mut self,
        device_id: &str,
        play: bool,
    ) -> Result<(), SpotifyApiError> {
        let token = self.valid_access_token().await?;
        let url = format!("{API_BASE}/me/player");
        let response = self
            .client
            .put(&url)
            .bearer_auth(token)
            .json(&serde_json::json!({ "device_ids": [device_id], "play": play }))
            .send()
            .await
            .map_err(|e| SpotifyApiError::Exchange(e.to_string()))?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        Err(map_api_status(status.as_u16(), None))
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

    // Edge case: a 200 body with no `item` (Web API returns `{}` when nothing is
    // loaded / no active device) maps to Idle rather than a title-less Playing.
    #[test]
    fn test_parse_now_playing_no_item_is_idle() {
        let np = parse_now_playing("{}");
        assert_eq!(np.state, NowPlayingState::Idle);
        assert_eq!(np.title, None);
        assert_eq!(np.artist, None);
    }

    // Edge case: a malformed body is treated as Idle rather than propagated as an
    // error, so a transient bad payload never breaks the now-playing SSE feed.
    #[test]
    fn test_parse_now_playing_malformed_body_is_idle() {
        let np = parse_now_playing("not json at all");
        assert_eq!(np.state, NowPlayingState::Idle);
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

    // Criterion: the blue2th-PC device is resolved by name, with its id and
    // active flag, so transport can target it instead of the active device.
    #[test]
    fn test_find_device_resolves_blue2th_pc_by_name() {
        let body = r#"{"devices":[
            {"id":"phone-id","name":"Pixel 7","is_active":true,"type":"Smartphone"},
            {"id":"pc-id","name":"blue2th-PC","is_active":false,"type":"Computer"}
        ]}"#;
        assert_eq!(
            find_device(body, SPOTIFY_DEVICE_NAME),
            Some(Device {
                id: "pc-id".to_string(),
                is_active: false,
            })
        );
    }

    // Criterion: an active blue2th-PC is reported as such, so no needless
    // playback transfer is issued before the command.
    #[test]
    fn test_find_device_reports_active_device() {
        let body = r#"{"devices":[{"id":"pc-id","name":"blue2th-PC","is_active":true}]}"#;
        assert_eq!(
            find_device(body, SPOTIFY_DEVICE_NAME),
            Some(Device {
                id: "pc-id".to_string(),
                is_active: true,
            })
        );
    }

    // Criterion: blue2th-PC absent from the list (librespot not running) yields
    // None, which the caller maps to BackendNotRunning rather than guessing.
    #[test]
    fn test_find_device_absent_is_none() {
        let body = r#"{"devices":[{"id":"phone-id","name":"Pixel 7","is_active":true}]}"#;
        assert_eq!(find_device(body, SPOTIFY_DEVICE_NAME), None);
    }

    // Criterion: a device still initialising carries a null id and cannot be
    // targeted; a malformed body must not panic either.
    #[test]
    fn test_find_device_null_id_or_malformed_body_is_none() {
        let null_id = r#"{"devices":[{"id":null,"name":"blue2th-PC","is_active":false}]}"#;
        assert_eq!(find_device(null_id, SPOTIFY_DEVICE_NAME), None);
        assert_eq!(find_device("not json", SPOTIFY_DEVICE_NAME), None);
        assert_eq!(find_device("{}", SPOTIFY_DEVICE_NAME), None);
    }

    // Criterion (phase 6.2): the Web API device lookup uses the *configured*
    // name. A driver that never got configured still searches for the default.
    #[test]
    fn test_device_name_defaults_to_the_spotify_device_name() {
        let auth = SpotifyAuth::with_config(None, "blue2th://spotify-callback".to_string());
        assert_eq!(auth.device_name(), SPOTIFY_DEVICE_NAME);
    }

    // Criterion (phase 6.2): once the app renames the backend, the lookup follows.
    // This is the 412 trap: `librespot` advertises `Salon` while a hard-coded
    // lookup still searches for `blue2th-PC`, and transport silently fails.
    #[test]
    fn test_device_lookup_follows_the_configured_name() {
        let mut auth = SpotifyAuth::with_config(None, "blue2th://spotify-callback".to_string());
        auth.set_device_name("Salon");
        assert_eq!(auth.device_name(), "Salon");

        // What librespot advertises after the rename: only `Salon` is there.
        let body = r#"{"devices":[
            {"id":"phone-id","name":"Pixel 7","is_active":true,"type":"Smartphone"},
            {"id":"pc-id","name":"Salon","is_active":false,"type":"Computer"}
        ]}"#;
        assert_eq!(
            find_device(body, auth.device_name()),
            Some(Device {
                id: "pc-id".to_string(),
                is_active: false,
            }),
            "the lookup must find the renamed Connect device"
        );
        assert_eq!(
            find_device(body, SPOTIFY_DEVICE_NAME),
            None,
            "the constant must no longer be what transport searches for"
        );
    }

    // Criterion: a saved refresh token is read back, so a server restart restores
    // Connected instead of sending the user through the browser again.
    #[test]
    fn test_refresh_token_round_trips_through_the_store() {
        let path = std::env::temp_dir()
            .join("blue2th-test-token-roundtrip")
            .join(TOKEN_STORE_FILE);
        let _ = std::fs::remove_file(&path);

        save_refresh_token(&path, "AQD-refresh-token").expect("save the refresh token");
        assert_eq!(
            load_refresh_token(Some(&path)),
            Some("AQD-refresh-token".to_string())
        );

        // The credential must not be world-readable.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path)
                .expect("stat the token store")
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o077,
                0,
                "token store must be owner-only, got {mode:o}"
            );
        }
        let _ = std::fs::remove_file(&path);
    }

    // Criterion: a missing, empty or malformed store reads as "not logged in"
    // rather than failing startup.
    #[test]
    fn test_load_refresh_token_tolerates_missing_or_malformed_store() {
        let dir = std::env::temp_dir().join("blue2th-test-token-malformed");
        std::fs::create_dir_all(&dir).expect("create the test dir");
        let missing = dir.join("absent.json");
        let _ = std::fs::remove_file(&missing);
        assert_eq!(load_refresh_token(Some(&missing)), None);
        assert_eq!(load_refresh_token(None), None);

        let malformed = dir.join("malformed.json");
        std::fs::write(&malformed, "not json").expect("write the malformed store");
        assert_eq!(load_refresh_token(Some(&malformed)), None);

        let empty = dir.join("empty.json");
        std::fs::write(&empty, r#"{"refresh_token":""}"#).expect("write the empty store");
        assert_eq!(load_refresh_token(Some(&empty)), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Criterion: an explicitly configured driver never touches the disk, so tests
    // can neither read nor delete the real credential.
    #[test]
    fn test_with_config_driver_has_no_token_store() {
        let auth = SpotifyAuth::with_config(Some("id".to_string()), "blue2th://cb".to_string());
        assert!(auth.store.is_none(), "with_config must stay off-disk");
    }

    // Criterion: an unconfigured driver refuses every OAuth path up front.
    #[test]
    fn test_authorize_url_without_client_id_is_not_configured() {
        let mut auth = SpotifyAuth::with_config(None, DEFAULT_REDIRECT_URI.to_string());
        assert!(matches!(
            auth.authorize_url(),
            Err(SpotifyApiError::NotConfigured)
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
