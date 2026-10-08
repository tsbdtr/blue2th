// SPDX-License-Identifier: MIT OR Apache-2.0

//! Thin HTTP client for the blue2th PC backend (see `docs/ARCHITECTURE.md`):
//! resolves the active backend's base URL and token, then wraps every route the
//! app calls, from `GET /health` to the Spotify transport.

use std::time::Duration;

use blue2th_proto::{
    AuthCallbackRequest, AuthUrlResponse, ClientPresence, ConfigRequest, DeviceInfo, HealthStatus,
    NowPlaying, OffsetRequest, PairRequest, PairResponse, PlaybackState, PresenceRequest,
    ProtocolMismatch, ServerConfig, SpotifyAuthState, SpotifyState, TargetsState, VolumeRequest,
};
use futures::StreamExt;

use crate::settings::{AppSettings, ConfigSync};

/// How long the app keeps reading the `/scan` SSE feed before stopping. The
/// backend caps discovery on its side too; this is the client-side window.
const SCAN_WINDOW: Duration = Duration::from_secs(8);

/// Error talking to the backend; surfaced to the UI as a string.
#[derive(Debug, Clone)]
pub struct BackendError {
    /// What to show the user.
    message: String,
    /// Whether the backend refused the app's credential (or it has none). Kept
    /// apart from the message so the UI can point at pairing instead of showing
    /// yet another network failure — "not paired" is not "unreachable".
    not_paired: bool,
    /// Which machine is behind, when the app and the backend disagree on the
    /// wire contract. Typed rather than folded into the message, exactly like
    /// `not_paired`: the UI has to name the side to update.
    mismatch: Option<ProtocolMismatch>,
    /// Whether a **speaker** refused the Bluetooth bond. Set by the connect
    /// route alone (see [`BackendError::pairing_failure_if_conflict`]), never by
    /// the status mapping. Kept apart from `not_paired`, which is about this app
    /// and its backend: the two are different failures with different remedies.
    pairing_failed: bool,
    /// The status the backend answered, when this failure came from a response.
    /// Retained, not interpreted: a status only acquires a meaning where a route
    /// gives it one.
    status: Option<u16>,
}

impl BackendError {
    fn new(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
            not_paired: false,
            mismatch: None,
            pairing_failed: false,
            status: None,
        }
    }

    /// The typed "this app and this backend do not speak the same wire
    /// contract" failure, naming which machine to update.
    pub fn protocol(mismatch: ProtocolMismatch) -> Self {
        Self {
            message: PROTOCOL_MISMATCH.to_string(),
            not_paired: false,
            mismatch: Some(mismatch),
            pairing_failed: false,
            status: None,
        }
    }

    /// Which side is behind, when this failure is a contract mismatch at all.
    /// `None` for every other failure — a backend that cannot be reached is not
    /// an incompatible one.
    pub fn protocol_mismatch(&self) -> Option<ProtocolMismatch> {
        self.mismatch
    }

    /// The typed "the app is not paired with this backend" failure: no token
    /// stored, or the backend answered 401.
    pub fn not_paired() -> Self {
        Self {
            message: NOT_PAIRED.to_string(),
            not_paired: true,
            mismatch: None,
            pairing_failed: false,
            status: None,
        }
    }

    /// Whether this failure means the app must pair (again) rather than that the
    /// backend is unreachable.
    pub fn is_not_paired(&self) -> bool {
        self.not_paired
    }

    /// The typed "the speaker refused the Bluetooth bond" failure: `pair()`
    /// failed on the backend's side.
    ///
    /// Deliberately **not** [`BackendError::not_paired`], which is about this app
    /// and its backend: this one is about the backend and a speaker.
    pub fn pairing_failed() -> Self {
        Self {
            message: BLUETOOTH_PAIRING_FAILED.to_string(),
            not_paired: false,
            mismatch: None,
            pairing_failed: true,
            status: None,
        }
    }

    /// Whether this failure is a refused Bluetooth pairing, in which case the
    /// speaker must not be marked unavailable — the user can put it into pairing
    /// mode and tap again.
    pub fn is_pairing_failed(&self) -> bool {
        self.pairing_failed
    }

    /// The HTTP status the backend answered, when this failure came from a
    /// response at all. `None` for the failures built without one (no backend
    /// configured, no token stored, a transport error).
    ///
    /// Retained rather than interpreted: the backend answers ten distinct
    /// failure statuses (enumerated as `BACKEND_STATUSES` in the tests, which is
    /// where that count stays checkable) and [`backend_error_for`] is the single
    /// mapping point for every route, so only a route may give a status a
    /// meaning.
    pub fn status(&self) -> Option<u16> {
        self.status
    }

    /// The connect route's own reading of a retained status: a `409` there — and
    /// only there — means the speaker refused the Bluetooth bond. Pure.
    ///
    /// Applied by `connect_device` alone, so no other route can set the flag: a
    /// `409` from `/spotify/play` while disconnected keeps meaning what it says.
    /// A [`BackendError::not_paired`] passes through untouched — a 401 on the
    /// connect route is still "pair the app with the backend".
    fn pairing_failure_if_conflict(self) -> Self {
        if self.not_paired || self.status() != Some(CONFLICT) {
            return self;
        }
        // A bodiless response reached here as the bare status line: "HTTP 409"
        // names no cause, so the constant takes over. A body is kept verbatim —
        // the BlueZ wording is what says at which step the bond was refused.
        if self.message == status_line(CONFLICT) {
            return Self {
                status: self.status,
                ..Self::pairing_failed()
            };
        }
        Self {
            pairing_failed: true,
            ..self
        }
    }
}

impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

/// Message carried by every call made while no backend is configured. The app
/// fails fast with it instead of guessing an address and timing out.
pub const NO_BACKEND_CONFIGURED: &str = "no backend configured";

/// Message carried by every call made while the active backend has no token, or
/// answered 401 (phase 6.4).
pub const NOT_PAIRED: &str = "not paired";

/// Fallback message for a refused Bluetooth pairing carried by a response with
/// no body. The screen shows the localised `device.pairing_failed` instead; this
/// is what the error carries for logs and for `Display`.
pub const BLUETOOTH_PAIRING_FAILED: &str = "bluetooth pairing failed";

/// The status the backend answers when a speaker refuses the Bluetooth bond —
/// and, on other routes, plenty of unrelated conflicts. Only
/// [`BackendError::pairing_failure_if_conflict`] reads it as the former.
const CONFLICT: u16 = 409;

/// What a failed response with no body shows: a bare status line, naming no
/// cause. Pure.
fn status_line(status: u16) -> String {
    format!("HTTP {status}")
}

/// Build the `{base}/pair` URL, tolerating a trailing slash on the base.
fn pair_url(base: &str) -> String {
    format!("{}/pair", base.trim_end_matches('/'))
}

/// The `Authorization` header value carrying `token`.
fn auth_header_value(token: &str) -> String {
    format!("Bearer {token}")
}

/// Map a failed backend response to a typed error, **retaining** its status.
/// Pure.
///
/// A 401 becomes [`BackendError::not_paired`], and that is the only meaning read
/// here: it is genuinely global, since any route can reject a stale token. Every
/// other status keeps the backend's own wording (which `error_for_status` would
/// throw away, leaving the phone showing a bare status line) and nothing else —
/// this is the single mapping point for every route, so anything interpreted
/// here becomes global, and the next status added server-side would re-break the
/// reader in silence. A route that wants a meaning applies its own rule, the way
/// `connect_device` applies [`BackendError::pairing_failure_if_conflict`].
fn backend_error_for(status: u16, body: &str) -> BackendError {
    let message = body.trim();
    let typed = if status == 401 {
        // Typed, not textual: the UI must be able to tell an unpaired app from
        // an unreachable one, and the backend's wording may change.
        BackendError::not_paired()
    } else if message.is_empty() {
        BackendError::new(status_line(status))
    } else {
        BackendError::new(message)
    };
    BackendError {
        status: Some(status),
        ..typed
    }
}

/// The active backend's base URL **and** token, or a typed failure. Pure.
///
/// Nothing configured fails with [`NO_BACKEND_CONFIGURED`]; an active backend
/// that was never paired fails with [`BackendError::not_paired`] — in both cases
/// before any request is built, so an unpaired app never waits on a timeout.
fn authed_base_from(settings: &AppSettings) -> Result<(String, String), BackendError> {
    let base = base_url_from(settings)?;
    let token = settings
        .active_token()
        .ok_or_else(BackendError::not_paired)?;
    Ok((base, token))
}

/// `POST {base}/pair` — exchange a short-lived pairing code for the backend's
/// long-lived API token. The one call that carries no bearer, since the app has
/// none yet.
///
/// It takes the [`CompatibleBackend`] that [`check_backend_protocol`] returns:
///
/// ```
/// # async fn demo() -> Result<(), blue2th_frontend::backend::BackendError> {
/// let backend = blue2th_frontend::backend::check_backend_protocol("http://pc:8080").await?;
/// let _token = blue2th_frontend::backend::pair(&backend, "K7M2QX").await?;
/// # Ok(())
/// # }
/// ```
///
/// Pairing with a backend whose wire contract was never checked does not
/// compile: a bare URL is refused.
///
/// ```compile_fail
/// # async fn demo() -> Result<(), blue2th_frontend::backend::BackendError> {
/// let _token = blue2th_frontend::backend::pair("http://pc:8080", "K7M2QX").await?;
/// # Ok(())
/// # }
/// ```
pub async fn pair(backend: &CompatibleBackend, code: &str) -> Result<String, BackendError> {
    let request = reqwest::Client::new()
        .post(pair_url(&backend.url))
        .timeout(SETTINGS_CALL_TIMEOUT)
        .json(&PairRequest {
            // Owned copy: `PairRequest` is a plain DTO built for serialization.
            code: code.to_string(),
        });
    let granted: PairResponse = send_json(request).await?;
    Ok(granted.token)
}

/// How long a settings-page call waits before giving up.
///
/// A mistyped LAN address is the normal case here: the host either refuses at
/// once or, when it silently drops packets, never answers at all. Without a
/// bound, `Test` would spin forever and the two best-effort steps of
/// [`activate_backend`] would leak a task per switch.
const SETTINGS_CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// An HTTP client carrying the active backend's bearer, plus its base URL.
///
/// The address is resolved at **runtime** from the app settings: there is no
/// compile-time address, no seeded default, not even a localhost fallback, so an
/// unconfigured app attempts no network call at all.
///
/// Every guarded call goes through this: the token is a *default header* on the
/// client rather than something each call site remembers to add, so a new call
/// cannot silently ship unauthenticated.
fn authed_client() -> Result<(reqwest::Client, String), BackendError> {
    let (base, token) = authed_base_from(&crate::settings::current())?;
    let mut headers = reqwest::header::HeaderMap::new();
    let value = reqwest::header::HeaderValue::from_str(&auth_header_value(&token))
        .map_err(|_| BackendError::not_paired())?;
    headers.insert(reqwest::header::AUTHORIZATION, value);
    let client = reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .map_err(|e| BackendError::new(describe(&e)))?;
    Ok((client, base))
}

/// Send a prepared request and decode its JSON payload.
///
/// Every call goes through this rather than `error_for_status`, which flattens a
/// refusal into a bare status line: a 401 must reach the UI as the typed
/// [`BackendError::not_paired`], or a revoked token would read as one more
/// network failure and the reconnect loops would retry it forever.
async fn send_json<T: serde::de::DeserializeOwned>(
    request: reqwest::RequestBuilder,
) -> Result<T, BackendError> {
    let response = request
        .send()
        .await
        .map_err(|e| BackendError::new(describe(&e)))?;
    backend_error_message(response)
        .await?
        .json::<T>()
        .await
        .map_err(|e| BackendError::new(describe(&e)))
}

/// Resolve the base URL from an explicit settings snapshot (pure, testable).
fn base_url_from(settings: &AppSettings) -> Result<String, BackendError> {
    settings
        .active_url()
        .ok_or_else(|| BackendError::new(NO_BACKEND_CONFIGURED))
}

/// Build the `{base}/config` URL, tolerating a trailing slash on the base.
fn config_url(base: &str) -> String {
    format!("{}/config", base.trim_end_matches('/'))
}

/// `POST {base}/config` against an explicit address — push a config change to
/// the backend, which adopts the name as its Spotify Connect device name.
///
/// Addressed explicitly rather than through `backend_base_url()`: the only caller
/// is [`sync_config`], which pushes to the entry whose change it confirms, even
/// if a concurrent switch has already moved the resolved address on.
///
/// The whole config travels as one `ConfigRequest` rather than as a growing list
/// of positional booleans: two adjacent `bool` parameters would silently swap at
/// a call site.
async fn set_config_at(
    base: &str,
    token: Option<&str>,
    config: ConfigRequest,
) -> Result<ServerConfig, BackendError> {
    let request = bearing(reqwest::Client::new().post(config_url(base)), token);
    // Serialized as it came in: rebuilding it field by field here is how a
    // setting added later reaches the wire everywhere but in this one call.
    send_json(request.timeout(SETTINGS_CALL_TIMEOUT).json(&config)).await
}

/// The config body describing a backend entry, as the app holds it.
///
/// One place builds it, so a new setting reaches `POST /config` from every
/// caller at once.
fn config_body(entry: &crate::settings::BackendEntry) -> ConfigRequest {
    ConfigRequest {
        // Owned copy: `ConfigRequest` is a plain DTO built for serialization.
        name: entry.name.clone(),
        restore_during_playback: entry.restore_during_playback,
        auto_reconnect: entry.auto_reconnect,
        spotify_volume_lock: None,
    }
}

/// Add `token` as the bearer, when there is one.
///
/// For the calls addressed to an **explicit** backend rather than the active
/// one: they cannot go through [`authed_client`], and the token they need is the
/// one stored with *that* entry — the active one may already be another backend
/// entirely (see [`activate_backend`], which quietens the backend it is leaving).
fn bearing(request: reqwest::RequestBuilder, token: Option<&str>) -> reqwest::RequestBuilder {
    match token {
        Some(token) => request.header(reqwest::header::AUTHORIZATION, auth_header_value(token)),
        None => request,
    }
}

/// `POST {base}/spotify/pause` against an explicit address — used to quieten the
/// backend being left behind, which is no longer the one the settings resolve to.
///
/// It carries *that* backend's token: the guard applies here like anywhere else,
/// and the active token now belongs to the backend being switched to.
async fn pause_at(base: &str, token: Option<&str>) -> Result<(), BackendError> {
    post_at(base, "spotify/pause", token).await
}

/// `POST {base}/{path}` against an explicit address, carrying *that* backend's
/// token. The body-less counterpart of [`set_config_at`], for the calls aimed at
/// a backend the app is leaving rather than the one it resolves to.
async fn post_at(base: &str, path: &str, token: Option<&str>) -> Result<(), BackendError> {
    let url = format!("{}/{path}", base.trim_end_matches('/'));
    let response = bearing(reqwest::Client::new().post(&url), token)
        .timeout(SETTINGS_CALL_TIMEOUT)
        .send()
        .await
        .map_err(|e| BackendError::new(describe(&e)))?;
    backend_error_message(response).await?;
    Ok(())
}

/// Hand a backend back: quieten whatever it is playing, then shut its Spotify
/// source down.
///
/// The order is the point. Pausing first stops the audio while `librespot` is
/// still alive to stop it cleanly; killing the subprocess first would leave the
/// speakers on the last buffer it pushed. Stopping the source last is what frees
/// the PC — a `librespot` left running keeps the Connect device advertised and
/// the speakers claimed, on a machine the app no longer even lists.
///
/// Every step is best-effort and independent: a backend that is already down
/// must not stop the app from letting go of the rest. The last failure is
/// returned so the page can say something, but none of them is worth undoing.
async fn release_at(base: &str, token: Option<&str>) -> Result<(), BackendError> {
    let mut failure = None;
    for path in ["spotify/pause", "stop", "spotify/stop"] {
        if let Err(e) = post_at(base, path, token).await {
            failure = Some(e);
        }
    }
    match failure {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Whether `base` is now unreferenced — no remaining entry points at it.
///
/// Two entries may carry the same address under different names (only names are
/// unique), so deleting one label must not silence a machine the app still
/// drives through the other. Pure.
fn is_last_reference(remaining: &[crate::settings::BackendEntry], base: &str) -> bool {
    !remaining.iter().any(|b| b.url == base)
}

/// Delete a backend and hand it back: forget it locally, then quieten and shut
/// down the machine it named, when nothing else still points at it.
///
/// The local removal always happens and is persisted first, exactly as
/// [`activate_backend`] switches first: a backend that is slow or dead must
/// never keep the user staring at an entry they have deleted. The remote release
/// is best-effort on top.
pub async fn remove_backend(settings: &mut AppSettings, index: usize) -> Result<(), BackendError> {
    // Captured before the removal: afterwards the entry is gone, and its own
    // token is the only one that backend will accept.
    let released = settings
        .backends
        .get(index)
        .map(|b| (b.url.clone(), b.token.clone()));

    settings
        .remove(index)
        .map_err(|e| BackendError::new(e.to_string()))?;
    // Owned copy: the cache keeps its own settings beyond this borrow.
    crate::settings::set_current(settings.clone());

    let Some((base, token)) = released else {
        return Ok(());
    };
    if !is_last_reference(&settings.backends, &base) {
        return Ok(());
    }
    release_at(&base, token.as_deref()).await
}

/// `GET {base}/config` — the configuration of the active backend of `settings`
/// (#160): the read half of [`sync_config`].
///
/// Resolved from the snapshot it is given rather than from the process-wide
/// cache, like [`set_config_at`]: the answer is adopted into that snapshot's
/// active entry, so it must come from that entry's backend.
pub async fn fetch_config(settings: &AppSettings) -> Result<ServerConfig, BackendError> {
    let (base, token) = authed_base_from(settings)?;
    let request = bearing(reqwest::Client::new().get(config_url(&base)), Some(&token));
    send_json(request.timeout(SETTINGS_CALL_TIMEOUT)).await
}

/// Sync the active backend's config, the same rule for every client (#160):
/// push a change the backend has not acknowledged (`POST /config`) and confirm
/// it, otherwise read `GET /config` and adopt it. Only `settings` is updated:
/// writing the process-wide cache — which is what gets persisted — is the
/// caller's, so it can keep an edit made while this call was out.
///
/// A change the backend refused stays pending, so it goes out at the next sync:
/// what the phone's re-push on every reconnection used to cover, without ever
/// overwriting what another client set since.
pub async fn sync_config(settings: &mut AppSettings) -> Result<(), BackendError> {
    match crate::settings::config_sync(settings) {
        ConfigSync::Push => {
            let (base, token) = authed_base_from(settings)?;
            let pushed = settings
                .active_backend()
                .map(config_body)
                .ok_or_else(|| BackendError::new(NO_BACKEND_CONFIGURED))?;
            // Owned copy: the body is consumed by the request, and the
            // confirmation compares the entry against what was sent.
            set_config_at(&base, Some(&token), pushed.clone()).await?;
            settings.confirm_config_push(&base, &pushed);
        },
        ConfigSync::Read => {
            let config = fetch_config(settings).await?;
            crate::settings::adopt_config(settings, &config);
        },
    }
    Ok(())
}

/// Everything the browser's presence post carries (#160), built on the host so
/// the wasm glue only maps it onto a `fetch` — `reqwest` has no `keepalive` on
/// wasm, and a post sent from `pagehide` without it dies with the page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresencePost {
    /// `{base}/client/presence`.
    pub url: String,
    /// The `Authorization` header value: `Bearer <token>`.
    pub authorization: String,
    /// The JSON body, a [`PresenceRequest`].
    pub body: String,
    /// Whether `fetch` is asked to outlive the page (`keepalive: true`).
    pub keepalive: bool,
}

/// The presence post for `presence` against the active backend of `settings`.
/// Pure. Fails like every guarded call: no backend configured, or not paired.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub fn browser_presence_post(
    settings: &AppSettings,
    presence: ClientPresence,
) -> Result<PresencePost, BackendError> {
    let (base, token) = authed_base_from(settings)?;
    let body = serde_json::to_string(&PresenceRequest { presence })
        .map_err(|e| BackendError::new(e.to_string()))?;
    Ok(PresencePost {
        url: format!("{}/client/presence", base.trim_end_matches('/')),
        authorization: auth_header_value(&token),
        body,
        keepalive: true,
    })
}

/// Whether the backend at `previous` is really being left behind by a switch to
/// `next`. Re-activating the backend already in use must quieten nothing:
/// pausing it is the opposite of what the user asked for. Pure.
fn is_left_behind(previous: &str, next: Option<&str>) -> bool {
    next != Some(previous)
}

/// Switch the active backend: pause the previous one (best-effort), repoint the
/// app, and sync the new backend's config (#160): push a change the app holds
/// unsent, otherwise read and adopt what the backend has.
///
/// The settings page and the status-encart quick switch must both go through
/// this, so the two ways to switch cannot drift apart. The local switch always
/// happens: a failure talking to either backend is surfaced, never blocking —
/// the app must never be stuck on a dead backend.
pub async fn activate_backend(
    settings: &mut AppSettings,
    index: usize,
) -> Result<(), BackendError> {
    // Captured before the switch: afterwards this address is no longer the one
    // the app resolves, and it is the one that must be quietened — with its own
    // token, which the switch is about to stop being the active one.
    let previous = settings
        .active_backend()
        .map(|b| (b.url.clone(), b.token.clone()));

    // Switch locally first, and persist: a slow or dead backend must never hold
    // the app on a target the user has left.
    settings
        .activate(index)
        .map_err(|e| BackendError::new(e.to_string()))?;
    // Owned copy: the cache keeps its own settings beyond this borrow.
    crate::settings::set_current(settings.clone());

    let arriving = settings.active_url();

    // Both remote steps are best-effort and independent; the last failure is
    // surfaced so the toast says something, but neither undoes the switch.
    let mut failure = None;
    if let Some((base, token)) = previous {
        if is_left_behind(&base, arriving.as_deref()) {
            if let Err(e) = pause_at(&base, token.as_deref()).await {
                failure = Some(e);
            }
        }
    }
    // Synced rather than pushed: switching to a backend the app holds nothing
    // unsent for must not re-impose a stale copy over another client's change.
    if arriving.is_some() {
        if let Err(e) = sync_config(settings).await {
            failure = Some(e);
        }
        // Owned copy: persisted, or a restart would push again a change the
        // backend has acknowledged, over whatever another client set since.
        crate::settings::set_current(settings.clone());
    }
    match failure {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// `GET {base}/health` against an explicit address — the settings page's `Test`
/// action, which pings a backend that is not (yet) the active one.
pub async fn test_backend(url: &str) -> Result<HealthStatus, BackendError> {
    // Bounded: a typo'd address that drops packets would otherwise leave the
    // `Test` button waiting forever with no answer either way.
    send_json(
        reqwest::Client::new()
            .get(health_url(url))
            .timeout(SETTINGS_CALL_TIMEOUT),
    )
    .await
}

/// Build the `/health` URL from a base, tolerating a trailing slash.
fn health_url(base: &str) -> String {
    format!("{}/health", base.trim_end_matches('/'))
}

/// Message carried by a wire-contract mismatch. The *side to update* travels
/// beside it as a [`ProtocolMismatch`], never inside this string — which is why
/// this one is not localised: the UI reads the typed variant, not this text.
pub const PROTOCOL_MISMATCH: &str = "incompatible backend";

/// A backend whose wire contract this app speaks, as [`check_backend_protocol`]
/// found it at `url`.
///
/// Only that check builds one — the field is private — so [`pair`] cannot run
/// against a backend nobody checked, nor against another URL than the checked one.
///
/// ```compile_fail
/// let _ = blue2th_frontend::backend::CompatibleBackend {
///     url: "http://pc:8080".to_string(),
/// };
/// ```
#[derive(Debug, PartialEq, Eq)]
pub struct CompatibleBackend {
    url: String,
}

/// Probe `GET {url}/health` and check the wire contract this app speaks against
/// the range the backend announces.
///
/// Runs before pairing: the phone and the PC are updated by hand at different
/// times, so a version gap is the normal state between two updates, and a token
/// minted against a backend the app cannot talk to would be useless.
pub async fn check_backend_protocol(url: &str) -> Result<CompatibleBackend, BackendError> {
    // An unreachable backend surfaces its transport error untouched: "cannot
    // reach" must never read as "incompatible".
    let health = test_backend(url).await?;
    blue2th_proto::check_protocol(&health, blue2th_proto::PROTOCOL_VERSION)
        .map_err(BackendError::protocol)?;
    Ok(CompatibleBackend {
        url: url.to_owned(),
    })
}

/// Flatten a `reqwest::Error` and its source chain into one string, so the
/// on-device UI shows the *underlying* cause (e.g. "Connection refused" vs
/// "CLEARTEXT communication not permitted") instead of just "error sending request".
fn describe(err: &reqwest::Error) -> String {
    use std::error::Error as _;
    let mut msg = err.to_string();
    let mut source = err.source();
    while let Some(e) = source {
        msg.push_str(" -> ");
        msg.push_str(&e.to_string());
        source = e.source();
    }
    msg
}

/// `GET {base}/health` and decode the backend's `HealthStatus`.
///
/// The one guard-free call besides [`pair`], and deliberately so: `/health` stays
/// open on the backend precisely so an app holding no (or a stale) token can
/// still tell "not paired" from "unreachable". Requiring a bearer here would
/// paint an alive-but-unpaired backend as offline — the exact confusion the open
/// probe exists to prevent. The bearer is still sent when there is one, so the
/// request is identical for a paired app.
pub async fn ping_backend() -> Result<HealthStatus, BackendError> {
    let settings = crate::settings::current();
    let base = base_url_from(&settings)?;
    let mut request = reqwest::Client::new().get(health_url(&base));
    if let Some(token) = settings.active_token() {
        request = request.header(reqwest::header::AUTHORIZATION, auth_header_value(&token));
    }
    send_json(request).await
}

/// Run a backend scan: consume the `/scan` SSE feed for `SCAN_WINDOW`, collecting
/// each discovered device (deduplicated by address).
pub async fn scan_devices() -> Result<Vec<DeviceInfo>, BackendError> {
    let (client, base) = authed_client()?;
    let url = format!("{}/scan", base.trim_end_matches('/'));
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| BackendError::new(describe(&e)))?;
    // Typed rather than `error_for_status`: the caller's reconnect loop must be
    // able to stop on a 401 instead of retrying a revoked token forever.
    let response = backend_error_message(response).await?;

    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut found: Vec<DeviceInfo> = Vec::new();

    let collect = async {
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| BackendError::new(describe(&e)))?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            // SSE events are separated by a blank line.
            while let Some(pos) = buffer.find("\n\n") {
                let block: String = buffer.drain(..pos + 2).collect();
                if let Some(json) = sse_device_payload(&block) {
                    if let Ok(device) = serde_json::from_str::<DeviceInfo>(&json) {
                        if !found.iter().any(|d| d.address == device.address) {
                            found.push(device);
                        }
                    }
                }
            }
        }
        Ok::<(), BackendError>(())
    };

    // Stop after the window even if the server keeps the stream open.
    let _ = crate::timer::timeout(SCAN_WINDOW, collect).await;
    Ok(found)
}

/// `POST {base}/devices/{address}/connect` — pair/trust/connect on the backend,
/// returning the device's updated state.
pub async fn connect_device(address: &str) -> Result<DeviceInfo, BackendError> {
    // The one place a 409 means "the speaker refused the bond": on every other
    // route it keeps saying what the backend meant by it.
    post_device_action(address, "connect")
        .await
        .map_err(BackendError::pairing_failure_if_conflict)
}

/// `POST {base}/devices/{address}/disconnect` — disconnect on the backend,
/// returning the device's updated state.
pub async fn disconnect_device(address: &str) -> Result<DeviceInfo, BackendError> {
    post_device_action(address, "disconnect").await
}

/// POST `{base}/devices/{address}/{action}` and decode the updated `DeviceInfo`.
async fn post_device_action(address: &str, action: &str) -> Result<DeviceInfo, BackendError> {
    let (client, base) = authed_client()?;
    let url = format!("{}/devices/{address}/{action}", base.trim_end_matches('/'));
    send_json(client.post(&url)).await
}

/// `GET {base}/devices` — the backend's paired devices and their current state.
/// Used by the periodic poll to refresh `connected`/`rssi` without re-scanning.
pub async fn fetch_devices() -> Result<Vec<DeviceInfo>, BackendError> {
    let (client, base) = authed_client()?;
    let url = format!("{}/devices", base.trim_end_matches('/'));
    send_json(client.get(&url)).await
}

/// `POST {base}/play` — start (or resume) playback on the backend, returning the
/// new playback state.
pub async fn play() -> Result<PlaybackState, BackendError> {
    post_transport("play").await
}

/// `POST {base}/pause` — pause playback, returning the new state.
pub async fn pause() -> Result<PlaybackState, BackendError> {
    post_transport("pause").await
}

/// `POST {base}/stop` — stop playback, returning the new state.
pub async fn stop() -> Result<PlaybackState, BackendError> {
    post_transport("stop").await
}

/// `GET {base}/playback` — the backend's current playback state.
pub async fn playback_state() -> Result<PlaybackState, BackendError> {
    let (client, base) = authed_client()?;
    let url = format!("{}/playback", base.trim_end_matches('/'));
    send_json(client.get(&url)).await
}

/// `POST {base}/volume` — set the connected speaker's PipeWire sink volume
/// (clamped server-side to `0.0..=1.0`), returning the new state.
pub async fn set_volume(level: f32) -> Result<PlaybackState, BackendError> {
    let (client, base) = authed_client()?;
    let url = format!("{}/volume", base.trim_end_matches('/'));
    send_json(client.post(&url).json(&VolumeRequest { level })).await
}

/// POST `{base}/{action}` (no body) and decode the updated `PlaybackState`.
async fn post_transport(action: &str) -> Result<PlaybackState, BackendError> {
    let (client, base) = authed_client()?;
    let url = format!("{}/{action}", base.trim_end_matches('/'));
    send_json(client.post(&url)).await
}

/// Build the `{base}/devices/{address}/{action}` URL for a target action
/// (`select`/`deselect`/`offset`), tolerating a trailing slash on the base.
fn device_action_url(base: &str, address: &str, action: &str) -> String {
    format!("{}/devices/{address}/{action}", base.trim_end_matches('/'))
}

/// Build the `{base}/targets` URL, tolerating a trailing slash on the base.
fn targets_url(base: &str) -> String {
    format!("{}/targets", base.trim_end_matches('/'))
}

/// `POST {base}/devices/{address}/select` — select a connected speaker as a
/// playback target, returning the updated selection state.
pub async fn select_target(address: &str) -> Result<TargetsState, BackendError> {
    let (client, base) = authed_client()?;
    let url = device_action_url(&base, address, "select");
    send_json(client.post(&url)).await
}

/// `POST {base}/devices/{address}/deselect` — drop a speaker from the playback
/// target selection, returning the updated selection state.
pub async fn deselect_target(address: &str) -> Result<TargetsState, BackendError> {
    let (client, base) = authed_client()?;
    let url = device_action_url(&base, address, "deselect");
    send_json(client.post(&url)).await
}

/// `POST {base}/devices/{address}/offset` — set a target speaker's latency offset
/// (clamped server-side to `0..=750` ms), returning the updated selection state.
pub async fn set_offset(address: &str, offset_ms: u32) -> Result<TargetsState, BackendError> {
    let (client, base) = authed_client()?;
    let url = device_action_url(&base, address, "offset");
    send_json(client.post(&url).json(&OffsetRequest { offset_ms })).await
}

/// `GET {base}/targets` — the backend's current playback-target selection,
/// per-speaker offsets and routing mode.
pub async fn fetch_targets() -> Result<TargetsState, BackendError> {
    let (client, base) = authed_client()?;
    let url = targets_url(&base);
    send_json(client.get(&url)).await
}

/// Build the `{base}/spotify/{action}` URL for a Spotify backend action
/// (`start`/`stop`/`status`), tolerating a trailing slash on the base.
fn spotify_url(base: &str, action: &str) -> String {
    format!("{}/spotify/{action}", base.trim_end_matches('/'))
}

/// `POST {base}/spotify/start` — activate the Spotify source backend (spawn the
/// `librespot` Connect device), returning its new state.
pub async fn start_spotify() -> Result<SpotifyState, BackendError> {
    post_spotify("start").await
}

/// `POST {base}/spotify/stop` — deactivate the Spotify source backend (kill the
/// subprocess), returning its new state.
pub async fn stop_spotify() -> Result<SpotifyState, BackendError> {
    post_spotify("stop").await
}

/// `GET {base}/spotify/status` — the Spotify backend's current state.
pub async fn spotify_status() -> Result<SpotifyState, BackendError> {
    let (client, base) = authed_client()?;
    let url = spotify_url(&base, "status");
    send_json(client.get(&url)).await
}

/// POST `{base}/spotify/{action}` (no body) and decode the updated `SpotifyState`.
async fn post_spotify(action: &str) -> Result<SpotifyState, BackendError> {
    let (client, base) = authed_client()?;
    let url = spotify_url(&base, action);
    send_json(client.post(&url)).await
}

/// Build the `{base}/spotify/now-playing` SSE URL, tolerating a trailing slash.
fn now_playing_url(base: &str) -> String {
    format!("{}/spotify/now-playing", base.trim_end_matches('/'))
}

/// Surface the backend's own message for a failed response. `AppError` replies
/// with a plain-text body ("Spotify client id not configured — …", "start the
/// Spotify backend first"), which `error_for_status` would throw away, leaving
/// the user with a bare "503 Service Unavailable" on the phone.
async fn backend_error_message(
    response: reqwest::Response,
) -> Result<reqwest::Response, BackendError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    Err(backend_error_for(status.as_u16(), &body))
}

/// `GET {base}/spotify/auth/url` — ask the backend for a Spotify authorize URL
/// (PKCE) and the CSRF `state` to echo back on callback.
pub async fn spotify_auth_url() -> Result<AuthUrlResponse, BackendError> {
    let (client, base) = authed_client()?;
    let url = spotify_url(&base, "auth/url");
    send_json(client.get(&url)).await
}

/// `POST {base}/spotify/auth/callback` — hand the backend the authorization
/// `code` (and CSRF `state`) captured from the custom-scheme redirect.
///
/// Invoked from the root deep-link poll in `App`, which consumes the
/// redirect through `deep_link::take_pending_deep_link`.
pub async fn spotify_auth_callback(
    code: &str,
    state: &str,
) -> Result<SpotifyAuthState, BackendError> {
    let (client, base) = authed_client()?;
    let url = spotify_url(&base, "auth/callback");
    send_json(client.post(&url).json(&AuthCallbackRequest {
        // Owned copies: `AuthCallbackRequest` is a plain DTO built for
        // serialization.
        code: code.to_string(),
        state: state.to_string(),
    }))
    .await
}

/// `GET {base}/spotify/auth/status` — the current auth state (Connected/Disconnected).
pub async fn spotify_auth_status() -> Result<SpotifyAuthState, BackendError> {
    let (client, base) = authed_client()?;
    let url = spotify_url(&base, "auth/status");
    send_json(client.get(&url)).await
}

/// `POST {base}/client/presence` — tell the backend whether the app is on screen,
/// backgrounded or closing.
///
/// The backend cannot infer this: Android freezes a backgrounded app, so its
/// dropped SSE feed looks exactly like a phone that is gone. Reporting keeps a
/// background listening session alive and pauses at once on a real exit.
#[cfg(target_os = "android")]
pub async fn report_presence(presence: ClientPresence) -> Result<(), BackendError> {
    let (client, base) = authed_client()?;
    let url = format!("{}/client/presence", base.trim_end_matches('/'));
    let response = client
        .post(&url)
        .json(&PresenceRequest { presence })
        .send()
        .await
        .map_err(|e| BackendError::new(describe(&e)))?;
    backend_error_message(response).await?;
    Ok(())
}

/// A Spotify transport action, so the UI can carry one in a prop instead of a
/// stringly-typed path.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SpotifyAction {
    /// Skip to the previous track.
    Previous,
    /// Resume playback.
    Play,
    /// Pause playback.
    Pause,
    /// Skip to the next track.
    Next,
}

impl SpotifyAction {
    /// The `/spotify/{…}` path segment this action posts to.
    fn path(self) -> &'static str {
        match self {
            SpotifyAction::Previous => "previous",
            SpotifyAction::Play => "play",
            SpotifyAction::Pause => "pause",
            SpotifyAction::Next => "next",
        }
    }
}

/// `POST {base}/spotify/{action}` — drive playback through the Web API, which the
/// server applies to the `blue2th-PC` Connect device.
pub async fn spotify_transport(action: SpotifyAction) -> Result<(), BackendError> {
    post_spotify_transport(action.path()).await
}

/// POST `{base}/spotify/{action}` (no body) for a transport action; the backend
/// replies 204 (no content) on success, so no body is decoded.
async fn post_spotify_transport(action: &str) -> Result<(), BackendError> {
    let (client, base) = authed_client()?;
    let url = spotify_url(&base, action);
    let response = client
        .post(&url)
        .send()
        .await
        .map_err(|e| BackendError::new(describe(&e)))?;
    backend_error_message(response).await?;
    Ok(())
}

/// Subscribe to the `{base}/spotify/now-playing` SSE feed, invoking `on_event`
/// for each `now-playing` snapshot until the stream ends or the caller drops the
/// future. Errors talking to the backend are surfaced to the caller.
pub async fn subscribe_now_playing<F>(mut on_event: F) -> Result<(), BackendError>
where
    F: FnMut(NowPlaying),
{
    let (client, base) = authed_client()?;
    let url = now_playing_url(&base);
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|e| BackendError::new(describe(&e)))?;
    // Typed rather than `error_for_status`: the caller's reconnect loop must be
    // able to stop on a 401 instead of retrying a revoked token forever.
    let response = backend_error_message(response).await?;

    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| BackendError::new(describe(&e)))?;
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        // SSE events are separated by a blank line.
        while let Some(pos) = buffer.find("\n\n") {
            let block: String = buffer.drain(..pos + 2).collect();
            if let Some(now_playing) = sse_now_playing_payload(&block) {
                on_event(now_playing);
            }
        }
    }
    Ok(())
}

/// Extract and parse the `NowPlaying` payload of a `now-playing` SSE event block,
/// ignoring keep-alive comments and non-`now-playing` events.
fn sse_now_playing_payload(block: &str) -> Option<NowPlaying> {
    let mut is_now_playing = false;
    let mut data: Option<String> = None;
    for line in block.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            is_now_playing = rest.trim() == "now-playing";
        } else if let Some(rest) = line.strip_prefix("data:") {
            data = Some(rest.trim().to_string());
        }
    }
    if is_now_playing {
        data.and_then(|json| serde_json::from_str::<NowPlaying>(&json).ok())
    } else {
        None
    }
}

/// Extract the JSON payload of a `device` SSE event block, ignoring keep-alive
/// comments and non-device events.
fn sse_device_payload(block: &str) -> Option<String> {
    let mut is_device = false;
    let mut data: Option<String> = None;
    for line in block.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            is_device = rest.trim() == "device";
        } else if let Some(rest) = line.strip_prefix("data:") {
            data = Some(rest.trim().to_string());
        }
    }
    if is_device {
        data
    } else {
        None
    }
}

#[cfg(test)]
mod tests;
