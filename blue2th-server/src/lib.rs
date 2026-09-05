// SPDX-License-Identifier: MIT OR Apache-2.0

//! blue2th PC backend library facade.
//!
//! Exposes the Axum router (`app()`) and the server entry point (`run()`) so
//! both the binary (`main.rs`) and integration tests can drive the same surface
//! in-process. Phase 0 only exposed `GET /health`; later phases add Bluetooth
//! (`bluer`) and audio (PipeWire) routes — see `docs/ROADMAP.md`.

use std::{
    convert::Infallible,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
    Json, Router,
};
use blue2th_proto::{
    AdapterInfo, AuthCallbackRequest, AuthUrlResponse, ClientPresence, ConfigRequest, DeviceInfo,
    HealthStatus, OffsetRequest, PairRequest, PairResponse, PlaybackState, PlaybackStatus,
    PresenceRequest, ServerConfig, SpeakerTarget, SpotifyAuthState, SpotifyState, SpotifyStatus,
    TargetsState, VolumeRequest,
};
use futures::{Stream, StreamExt};
use tokio::sync::Mutex;
use tracing_subscriber::EnvFilter;

pub mod audio;
pub mod auth;
mod bluetooth;
pub mod config;
pub mod identity;
pub mod reconnect;
pub mod spotify;
pub mod spotify_auth;
mod state_store;
pub mod targets;
pub mod watchdog;

use audio::{AudioEngine, AudioError, RodioOutput};
use auth::AuthStore;
use spotify::{SpotifyBackend, SpotifyError};
use spotify_auth::{SpotifyApiError, SpotifyAuth, Transport};
use targets::{SelectError, SpeakerTargets};

/// Shared application state injected through the Axum router (no globals).
#[derive(Clone)]
struct AppState {
    /// The audio engine, guarded for concurrent access.
    engine: Arc<Mutex<AudioEngine>>,
    /// The user's playback-target selection (0–2 speakers + offsets). `/play`
    /// derives its routing mode from this; an empty selection (`Idle`) is
    /// rejected with a 4xx so a stream never starts with nowhere to go.
    targets: Arc<Mutex<SpeakerTargets>>,
    /// Addresses of the currently connected speakers, kept in sync by
    /// `connect`/`disconnect`/`devices`/`scan`. Used to validate a `select`
    /// request and to drop disconnected speakers from the selection.
    connected: Arc<Mutex<Vec<String>>>,
    /// The Spotify source backend (a `librespot` subprocess), guarded for
    /// concurrent access by the `/spotify/*` handlers.
    spotify: Arc<Mutex<SpotifyBackend>>,
    /// The Spotify Web API auth driver (OAuth PKCE tokens + transport), guarded
    /// for concurrent access by the `/spotify/auth/*` and transport handlers.
    spotify_auth: Arc<Mutex<SpotifyAuth>>,
    /// Reader count for the now-playing SSE feed. The app holds that stream open
    /// for as long as it runs, so losing every reader means the phone is gone —
    /// the watchdog then pauses playback (see `watchdog`).
    sse_watch: Arc<watchdog::SseWatch>,
    /// The backend's own name, pushed by the app. It is the Spotify Connect
    /// device name `librespot` advertises *and* the name the Web API lookup
    /// matches on, so the two can never disagree.
    name: Arc<Mutex<config::ServerName>>,
    /// The API token and the armed pairing code (phase 6.4). Read by
    /// [`require_bearer`] on every guarded route and by the [`pair`] handler,
    /// which is the only one allowed to hand the token out.
    auth: Arc<Mutex<AuthStore>>,
    /// The auto-reconnect retry ladder (phase 6.5): which remembered speaker is
    /// due for a dial, and how long to wait after each failure. In memory only —
    /// a restart is deliberately a clean slate, so nothing stays given up on.
    reconnect: Arc<Mutex<reconnect::ReconnectTracker>>,
    /// The backend's claim that *it* paused both sources when the last speaker
    /// vanished (#67). It is what licenses the automatic resume on restoration:
    /// any explicit transport command from the app clears it, so a pause the user
    /// asked for is never undone by a speaker coming back.
    backend_paused_sources: Arc<AtomicBool>,
}

/// One route the backend serves, as a (method, path template) pair plus whether
/// it is reachable without a bearer token.
///
/// The point of naming the routes in data is the guard: [`ROUTES`] is the single
/// source of truth the router is built from *and* the list the authentication
/// tests iterate, so a route added later without the guard fails a test instead
/// of quietly shipping an open door.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteSpec {
    /// HTTP method, upper-case (`"GET"`, `"POST"`).
    pub method: &'static str,
    /// Axum path template, e.g. `/devices/{addr}/connect`.
    pub path: &'static str,
    /// Whether the route answers without `Authorization: Bearer <token>`.
    /// Only `GET /health` and `POST /pair` may be public.
    pub public: bool,
}

/// Every route the backend serves.
///
/// The router is built **from** this table, not alongside it: an axum `Router`
/// cannot be enumerated, so a route mounted by hand next to the table would
/// escape the guard tests entirely. Adding a route means adding a line here.
pub const ROUTES: &[RouteSpec] = &[
    RouteSpec {
        method: "GET",
        path: "/health",
        public: true,
    },
    RouteSpec {
        method: "POST",
        path: "/pair",
        public: true,
    },
    RouteSpec {
        method: "GET",
        path: "/adapters",
        public: false,
    },
    RouteSpec {
        method: "GET",
        path: "/devices",
        public: false,
    },
    RouteSpec {
        method: "POST",
        path: "/devices/{addr}/connect",
        public: false,
    },
    RouteSpec {
        method: "POST",
        path: "/devices/{addr}/disconnect",
        public: false,
    },
    RouteSpec {
        method: "POST",
        path: "/devices/{addr}/select",
        public: false,
    },
    RouteSpec {
        method: "POST",
        path: "/devices/{addr}/deselect",
        public: false,
    },
    RouteSpec {
        method: "POST",
        path: "/devices/{addr}/offset",
        public: false,
    },
    RouteSpec {
        method: "GET",
        path: "/targets",
        public: false,
    },
    RouteSpec {
        method: "GET",
        path: "/scan",
        public: false,
    },
    RouteSpec {
        method: "POST",
        path: "/play",
        public: false,
    },
    RouteSpec {
        method: "POST",
        path: "/pause",
        public: false,
    },
    RouteSpec {
        method: "POST",
        path: "/stop",
        public: false,
    },
    RouteSpec {
        method: "POST",
        path: "/volume",
        public: false,
    },
    RouteSpec {
        method: "GET",
        path: "/playback",
        public: false,
    },
    RouteSpec {
        method: "POST",
        path: "/spotify/start",
        public: false,
    },
    RouteSpec {
        method: "POST",
        path: "/spotify/stop",
        public: false,
    },
    RouteSpec {
        method: "GET",
        path: "/spotify/status",
        public: false,
    },
    RouteSpec {
        method: "GET",
        path: "/spotify/auth/url",
        public: false,
    },
    RouteSpec {
        method: "POST",
        path: "/spotify/auth/callback",
        public: false,
    },
    RouteSpec {
        method: "GET",
        path: "/spotify/auth/status",
        public: false,
    },
    RouteSpec {
        method: "POST",
        path: "/spotify/play",
        public: false,
    },
    RouteSpec {
        method: "POST",
        path: "/spotify/pause",
        public: false,
    },
    RouteSpec {
        method: "POST",
        path: "/spotify/next",
        public: false,
    },
    RouteSpec {
        method: "POST",
        path: "/spotify/previous",
        public: false,
    },
    RouteSpec {
        method: "GET",
        path: "/spotify/now-playing",
        public: false,
    },
    RouteSpec {
        method: "POST",
        path: "/client/presence",
        public: false,
    },
    RouteSpec {
        method: "GET",
        path: "/config",
        public: false,
    },
    RouteSpec {
        method: "POST",
        path: "/config",
        public: false,
    },
];

/// Hard cap on a single scan so a forgotten client cannot keep discovery running.
const SCAN_DURATION: Duration = Duration::from_secs(20);

/// Bind address of last resort: every interface, used only when no LAN address
/// can be resolved. Refusing to start would leave the operator with a backend
/// that works everywhere except on the box whose interfaces are still coming up.
const DEFAULT_BIND: &str = "0.0.0.0:4000";

/// TCP port the backend listens on.
const DEFAULT_PORT: u16 = 4000;

/// Env var overriding the bind address; it always wins over the automatic LAN
/// choice, so an operator can still ask for `0.0.0.0` (or a specific interface).
const BIND_ENV: &str = "BLUE2TH_BIND";

/// Run the backend: initialise tracing, bind the socket and serve the router.
pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();

    // A pairing window is opened whenever nobody *can* be paired — a first run,
    // but also a store that was missing, unreadable or malformed, since the token
    // minted in its place has just invalidated every paired client — and
    // otherwise only when the operator asks for one. Arming a code at every
    // restart would leave the one open door ajar for no reason.
    let pair_requested = std::env::args().any(|arg| arg == "--pair");

    let mut auth_store = AuthStore::with_store(auth::auth_store_path());
    let server_name = config::ServerName::with_store(config::name_store_path());
    let addr = lan_bind_address();

    if auth_store.minted_a_new_token() || pair_requested {
        let code = auth_store.arm_pairing(std::time::SystemTime::now());
        tracing::info!(
            "{}",
            pairing_banner(&advertised_url(&addr), server_name.name(), &code)
        );
    } else {
        tracing::info!("this backend is already paired; run with --pair to arm a new code");
    }

    // Published before the router takes ownership of the name, and held for the
    // whole run: dropping the daemon would withdraw the announcement.
    let identity = identity::BackendIdentity::with_store(identity::identity_store_path());
    let record = advertised_service_from(
        &addr,
        preferred_lan_ipv4(&host_ipv4_addresses()),
        identity.id(),
        server_name.name(),
    );
    let _mdns = advertise(&record);

    let router = app_with_auth_and_targets(
        SpotifyAuth::new(),
        SpeakerTargets::with_store(targets::offsets_store_path()),
        server_name,
        auth_store,
    );

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("blue2th-server listening on http://{addr}");

    axum::serve(listener, router).await?;
    Ok(())
}

/// Publish `record` as `_blue2th._tcp.local.`, returning the daemon that must be
/// kept alive for the announcement to stand.
///
/// Best effort by design: a host with no usable multicast interface still serves
/// its API perfectly well, and the pairing QR plus manual entry remain the way
/// in. Failing to announce must never stop the backend from starting.
fn advertise(record: &AdvertisedService) -> Option<mdns_sd::ServiceDaemon> {
    // Carried by the record rather than parsed back out of the assembled URL:
    // the host and the port are what built it in the first place.
    let Some((host, port)) = record.endpoint.as_ref() else {
        tracing::warn!(
            "no host/port to announce for {}; pair by QR or address instead",
            record.url
        );
        return None;
    };
    let port = *port;
    let properties: Vec<(&str, &str)> = record
        .txt
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    let announce = || -> Result<mdns_sd::ServiceDaemon, Box<dyn std::error::Error>> {
        let daemon = mdns_sd::ServiceDaemon::new()?;
        let info = mdns_sd::ServiceInfo::new(
            blue2th_proto::SERVICE_TYPE,
            // The instance name is cosmetic: the app matches on the TXT id, so a
            // rename never makes the machine look like a different one.
            "blue2th",
            &format!("{host}.local."),
            host.as_str(),
            port,
            &properties[..],
        )?;
        daemon.register(info)?;
        Ok(daemon)
    };

    match announce() {
        Ok(daemon) => {
            tracing::info!(
                "announcing {} on {}",
                record.url,
                blue2th_proto::SERVICE_TYPE
            );
            Some(daemon)
        },
        Err(e) => {
            tracing::warn!("mDNS announcement unavailable ({e}); pair by QR or address instead");
            None
        },
    }
}

/// The address to advertise in the pairing QR, resolving the host's LAN address
/// itself. See [`advertised_url_from`] for the rule.
fn advertised_url(bind_addr: &str) -> String {
    advertised_url_from(bind_addr, preferred_lan_ipv4(&host_ipv4_addresses()))
}

/// Pick the URL a pairing QR should carry for a backend bound to `bind_addr`.
/// Pure, so the wildcard cases are testable without the host's interfaces.
///
/// A backend bound to every interface has no address of its own to hand out, so
/// the detected LAN one is used: a QR carrying `0.0.0.0` would pair the phone
/// with nothing. With no LAN address detected either, the bind address is
/// advertised as-is — a QR that cannot work is still better than none, since the
/// operator can read the code off the same banner and type it.
fn advertised_url_from(bind_addr: &str, detected: Option<std::net::Ipv4Addr>) -> String {
    match advertised_endpoint_from(bind_addr, detected) {
        Some((host, port)) => format!("http://{host}:{port}"),
        // No host/port pair to resolve (no port at all, or one that is not a
        // number): the bind address is advertised verbatim, as before.
        None => format!("http://{bind_addr}"),
    }
}

/// The host and TCP port a backend bound to `bind_addr` should be reached at, or
/// `None` when the bind address carries no numeric port. Pure.
///
/// The single place the wildcard rule lives: both the pairing QR
/// ([`advertised_url_from`]) and the mDNS record ([`advertised_service_from`])
/// read it, so the announcement and the QR can never point at different hosts.
fn advertised_endpoint_from(
    bind_addr: &str,
    detected: Option<std::net::Ipv4Addr>,
) -> Option<(String, u16)> {
    let (host, port) = bind_addr.rsplit_once(':')?;
    let port = port.parse::<u16>().ok()?;
    // A backend bound to every interface has no address of its own to hand out,
    // so the detected LAN one takes its place: `0.0.0.0` is something no phone
    // could ever call. With none detected, the bind address stands as-is.
    match (host, detected) {
        ("0.0.0.0" | "[::]", Some(lan)) => Some((lan.to_string(), port)),
        _ => Some((host.to_string(), port)),
    }
}

/// What the backend publishes as `_blue2th._tcp.local.` (phase 6.6): the address
/// the phone must call and the TXT records identifying the machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvertisedService {
    /// Base URL the app will store, e.g. `http://192.168.1.107:4000`.
    pub url: String,
    /// The same address as host and port, which is what `ServiceInfo` needs.
    /// Carried here so publishing never parses them back out of `url`; `None`
    /// when the bind address has no numeric port, which is nothing an mDNS
    /// record could announce.
    pub endpoint: Option<(String, u16)>,
    /// TXT records, keyed by the shared `blue2th_proto` TXT keys.
    pub txt: Vec<(String, String)>,
}

/// Build the mDNS record for a backend bound to `bind_addr`. Pure, so the
/// wildcard-bind case is testable without the host's interfaces or a network.
///
/// The URL goes through [`advertised_url_from`] — the same rule the pairing QR
/// uses — so a wildcard bind announces the LAN address rather than `0.0.0.0`,
/// which no phone could ever call.
fn advertised_service_from(
    bind_addr: &str,
    detected: Option<std::net::Ipv4Addr>,
    id: &str,
    name: &str,
) -> AdvertisedService {
    AdvertisedService {
        url: advertised_url_from(bind_addr, detected),
        endpoint: advertised_endpoint_from(bind_addr, detected),
        txt: vec![
            (blue2th_proto::TXT_KEY_ID.to_string(), id.to_string()),
            (blue2th_proto::TXT_KEY_NAME.to_string(), name.to_string()),
        ],
    }
}

/// The address the backend binds to: `BLUE2TH_BIND` when set, otherwise the
/// host's LAN IPv4, otherwise every interface.
///
/// Binding to the LAN address is **defence in depth, not authentication**: it
/// stops the API being served on other interfaces (a VPN, a laptop's public
/// one). It does not restrict who on the LAN may connect — that is the bearer
/// token's job.
pub fn lan_bind_address() -> String {
    bind_address(
        std::env::var(BIND_ENV).ok().filter(|a| !a.is_empty()),
        preferred_lan_ipv4(&host_ipv4_addresses()),
    )
}

/// The host's IPv4 addresses, read from the interfaces the OS exposes.
///
/// Hardware/OS seam: which interfaces exist is host-dependent, so the *choice*
/// among them is a pure function ([`preferred_lan_ipv4`]) and only the gathering
/// lives here. A failure yields no candidate, which falls back to every
/// interface rather than refusing to start.
fn host_ipv4_addresses() -> Vec<std::net::Ipv4Addr> {
    let Ok(output) = std::process::Command::new("hostname").arg("-I").output() else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .filter_map(|token| token.parse().ok())
        .collect()
}

/// Pick the bind address from an explicit override and a detected LAN address.
/// Pure, so the precedence is testable without touching the environment or the
/// host's interfaces.
fn bind_address(override_addr: Option<String>, detected: Option<std::net::Ipv4Addr>) -> String {
    match (override_addr, detected) {
        // An explicit override always wins: the operator knows their network
        // better than a heuristic does.
        (Some(addr), _) => addr,
        (None, Some(lan)) => format!("{lan}:{DEFAULT_PORT}"),
        // No routable address (no interface up): every interface, with the
        // warning left to the caller. Refusing to start would be worse.
        (None, None) => DEFAULT_BIND.to_string(),
    }
}

/// The best LAN IPv4 among the host's addresses: a routable, non-loopback,
/// non-link-local one. `None` when there is no such address (no interface up),
/// in which case the caller falls back to every interface with a warning rather
/// than refusing to start. Pure.
pub fn preferred_lan_ipv4(candidates: &[std::net::Ipv4Addr]) -> Option<std::net::Ipv4Addr> {
    candidates
        .iter()
        .find(|addr| {
            // Loopback reaches nothing but this host; link-local (169.254/16)
            // means DHCP failed, so it is not a LAN address either.
            !addr.is_loopback() && !addr.is_link_local() && !addr.is_unspecified()
        })
        .copied()
}

/// The startup banner: the pairing code as text **and** as an ASCII QR of the
/// `blue2th://pair?…` deep link, so the operator can either type six characters
/// or point a phone camera at the terminal.
pub fn pairing_banner(url: &str, name: &str, code: &str) -> String {
    let link = blue2th_proto::pair_deep_link(url, name, code);
    format!(
        "\n{}\nPair this backend: scan the code above with the phone's camera, \
         or type this pairing code in blue2th → Settings: {code}\n\
         It is valid for {} minutes and works once.\n",
        pairing_qr(&link),
        auth::PAIRING_TTL.as_secs() / 60,
    )
}

/// Render `link` as a QR code in text a terminal can show.
pub fn pairing_qr(link: &str) -> String {
    match qrcode::QrCode::new(link.as_bytes()) {
        Ok(code) => code
            .render::<qrcode::render::unicode::Dense1x2>()
            .quiet_zone(true)
            .build(),
        // A QR that cannot be built must not cost the operator the code itself:
        // the banner still prints it as text, which is the other transport.
        Err(e) => {
            tracing::warn!("could not render the pairing QR: {e}");
            String::new()
        },
    }
}

/// Build the application router. Kept separate from `run` so tests can exercise
/// it in-process without binding a socket.
pub fn app() -> Router {
    // The env-built driver restores the persisted refresh token, so a restart
    // keeps the user logged in; the store-backed selection restores each
    // speaker's tuned offset the same way.
    app_with_auth_and_targets(
        SpotifyAuth::new(),
        SpeakerTargets::with_store(targets::offsets_store_path()),
        // Reloaded from disk so the Web API lookup keeps matching the running
        // librespot even before the app talks to us again.
        config::ServerName::with_store(config::name_store_path()),
        // The real, persisted API token: **no test may call `app()`**, since
        // minting or rotating this would unpair the operator's own phone.
        AuthStore::with_store(auth::auth_store_path()),
    )
}

/// Build the router around an explicit Spotify auth driver **and an explicit API
/// token** — the entry point every test uses, since it touches no store at all:
/// a test that let the server mint or reload the real token would unpair the
/// operator's phone (`AuthStore::with_token` keeps it in memory).
///
/// There is deliberately no variant that mints its own token: the caller could
/// not know it, so every guarded route would answer 401 and the test would look
/// broken for the wrong reason.
pub fn app_with_auth_store(spotify_auth: SpotifyAuth, auth: AuthStore) -> Router {
    app_with_auth_and_targets(
        spotify_auth,
        SpeakerTargets::new(),
        config::ServerName::new(),
        auth,
    )
}

/// Build the router around an explicit Spotify auth driver and an explicit
/// playback selection, so the on-disk seams stay in the caller's hands.
fn app_with_auth_and_targets(
    mut spotify_auth: SpotifyAuth,
    speaker_targets: SpeakerTargets,
    server_name: config::ServerName,
    auth: AuthStore,
) -> Router {
    // The auth driver and the subprocess must start out agreeing with the stored
    // name, or the very first transport call would look up a device nobody
    // advertises.
    spotify_auth.set_device_name(server_name.name());
    let spotify = SpotifyBackend::with_name(server_name.name());
    let state = AppState {
        // Real playback output (rodio → PipeWire); the device is opened lazily on
        // the first `/play`, so building the router stays cheap and CI-safe.
        engine: Arc::new(Mutex::new(AudioEngine::with_output(Box::new(
            RodioOutput::new(),
        )))),
        targets: Arc::new(Mutex::new(speaker_targets)),
        connected: Arc::new(Mutex::new(Vec::new())),
        spotify: Arc::new(Mutex::new(spotify)),
        spotify_auth: Arc::new(Mutex::new(spotify_auth)),
        sse_watch: Arc::new(watchdog::SseWatch::default()),
        name: Arc::new(Mutex::new(server_name)),
        auth: Arc::new(Mutex::new(auth)),
        reconnect: Arc::new(Mutex::new(reconnect::ReconnectTracker::new())),
        backend_paused_sources: Arc::new(AtomicBool::new(false)),
    };

    spawn_idle_watchdog(state.clone());
    spawn_auto_reconnect(state.clone());

    // Built from `ROUTES`, never alongside it: the guard is applied per entry,
    // so a route can only exist here by being listed — and by declaring whether
    // it is public.
    let mut router = Router::new();
    for spec in ROUTES {
        let handler = route_handler(spec);
        let handler = if spec.public {
            handler
        } else {
            handler.layer(axum::middleware::from_fn_with_state(
                state.clone(),
                require_bearer,
            ))
        };
        router = router.route(spec.path, handler);
    }
    // No CORS layer at all. The permissive one this replaces answered the
    // preflight for any web page the user happened to open, which made a LAN
    // service reachable from the internet by proxy. The app is not a browser.
    router.with_state(state)
}

/// The handler behind one table entry.
///
/// A `match` rather than a registry: it is exhaustive by construction — a spec
/// added to `ROUTES` without a handler fails to compile here, which is the point
/// of building the router from the table.
fn route_handler(spec: &RouteSpec) -> axum::routing::MethodRouter<AppState> {
    match (spec.method, spec.path) {
        ("GET", "/health") => get(health),
        ("POST", "/pair") => post(pair),
        ("GET", "/adapters") => get(adapters),
        ("GET", "/devices") => get(devices),
        ("POST", "/devices/{addr}/connect") => post(connect),
        ("POST", "/devices/{addr}/disconnect") => post(disconnect),
        ("POST", "/devices/{addr}/select") => post(select_target),
        ("POST", "/devices/{addr}/deselect") => post(deselect_target),
        ("POST", "/devices/{addr}/offset") => post(set_target_offset),
        ("GET", "/targets") => get(get_targets),
        ("GET", "/scan") => get(scan),
        ("POST", "/play") => post(play),
        ("POST", "/pause") => post(pause),
        ("POST", "/stop") => post(stop),
        ("POST", "/volume") => post(volume),
        ("GET", "/playback") => get(playback),
        ("POST", "/spotify/start") => post(spotify_start),
        ("POST", "/spotify/stop") => post(spotify_stop),
        ("GET", "/spotify/status") => get(spotify_status),
        ("GET", "/spotify/auth/url") => get(spotify_auth_url),
        ("POST", "/spotify/auth/callback") => post(spotify_auth_callback),
        ("GET", "/spotify/auth/status") => get(spotify_auth_status),
        ("POST", "/spotify/play") => post(spotify_play),
        ("POST", "/spotify/pause") => post(spotify_pause),
        ("POST", "/spotify/next") => post(spotify_next),
        ("POST", "/spotify/previous") => post(spotify_previous),
        ("GET", "/spotify/now-playing") => get(spotify_now_playing),
        ("POST", "/client/presence") => post(client_presence),
        ("GET", "/config") => get(get_config),
        ("POST", "/config") => post(set_config),
        // Unreachable in practice (the table above is the only source), but a
        // 404 is the safe answer: never serve something unguarded by accident.
        _ => get(unknown_route),
    }
}

/// Answers a table entry with no handler. Reached only if `ROUTES` gains a line
/// without one — a 404 rather than an unguarded surprise.
async fn unknown_route() -> StatusCode {
    StatusCode::NOT_FOUND
}

/// Reject a request whose `Authorization` header does not carry the API token.
///
/// Applied per route, to the guarded ones only: the public pair is `GET /health`
/// (so the app can tell "not paired" from "unreachable") and `POST /pair`.
async fn require_bearer(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let header = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    if !state.auth.lock().await.authorises(header) {
        return AppError {
            status: StatusCode::UNAUTHORIZED,
            message: "not paired — pair this app with the backend first".to_string(),
        }
        .into_response();
    }
    next.run(request).await
}

/// `POST /pair` — exchange an armed pairing code for the API token.
///
/// **The one open door.** Everything protecting the API rests on the code being
/// armed only briefly, one-shot and attempt-capped; every refusal answers the
/// same 401 with the same message, so a caller cannot tell an unknown code from
/// an expired or already-used one.
async fn pair(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<Json<PairResponse>, AppError> {
    // A malformed body is a client error, not a refused code: nothing was
    // guessed, so distinguishing the two leaks nothing an attacker could use.
    let submitted = serde_json::from_slice::<PairRequest>(&body)
        .map(|req| req.code)
        .map_err(|e| AppError::bad_request(format!("invalid pair body: {e}")))?;
    let mut auth = state.auth.lock().await;
    match auth.redeem(&submitted, std::time::SystemTime::now()) {
        Ok(token) => Ok(Json(PairResponse { token })),
        Err(e) => Err(AppError {
            status: StatusCode::UNAUTHORIZED,
            message: e.to_string(),
        }),
    }
}

/// Start the auto-reconnect pass (phase 6.5): dial the remembered speakers back
/// on a widening backoff until they answer or the ladder runs out.
///
/// The first pass runs immediately, which is the startup pass: a PC that boots
/// with its speaker already on has it back without anybody opening the app.
fn spawn_auto_reconnect(state: AppState) {
    // A router built outside an async context (a bare unit test) has no runtime
    // to spawn on — and a test must never dial the developer's own speakers.
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    tokio::spawn(async move {
        loop {
            auto_reconnect_pass(&state).await;
            tokio::time::sleep(reconnect::RECONNECT_TICK).await;
        }
    });
}

/// One reconnect pass: work out which remembered speakers are worth dialling
/// right now, dial them, and record what happened.
///
/// This runs forever on an idle backend, so the guards are ordered by cost and
/// every one of them returns **before** touching D-Bus. No lock is held across
/// an `await` on BlueZ either: a dial can block for seconds, and `/devices` must
/// not queue behind it.
///
/// It only connects. Re-selecting the speaker and rebuilding the routing belong
/// to `sync_connected` (phase 6.3), which picks it up on the next `/devices`
/// poll — doing both here would rebuild the PipeWire graph twice.
async fn auto_reconnect_pass(state: &AppState) {
    // Cheapest first: the setting the user can switch off.
    if !reconnect::should_auto_reconnect(state.name.lock().await.auto_reconnect()) {
        return;
    }
    let intended = state.targets.lock().await.intended();
    if intended.is_empty() {
        return;
    }
    // The connected cache answers "is anything even missing?" without a D-Bus
    // round-trip. It can be stale, so it only ever short-circuits the pass — the
    // live listing below is what the candidates are actually computed from.
    let cached = state.connected.lock().await.clone();
    let missing: Vec<String> = intended
        .iter()
        .filter(|addr| !cached.iter().any(|c| c == *addr))
        .cloned()
        .collect();
    if missing.is_empty() {
        return;
    }
    // Nothing due yet (every missing speaker is mid-backoff, dismissed or given
    // up on): the overwhelmingly common tick, and it ends here.
    if state
        .reconnect
        .lock()
        .await
        .due(&missing, std::time::Instant::now())
        .is_empty()
    {
        return;
    }

    let devices = match bluetooth::list_paired_devices().await {
        Ok(devices) => devices,
        // No adapter, no bluetoothd, no D-Bus: the server keeps serving and the
        // next tick tries again. Nothing here has a caller to fail.
        Err(e) => {
            tracing::warn!("auto-reconnect could not list the paired devices: {e}");
            return;
        },
    };
    let paired: Vec<String> = devices.iter().map(|d| d.address.clone()).collect();
    let connected: Vec<String> = devices
        .iter()
        .filter(|d| d.connected)
        .map(|d| d.address.clone())
        .collect();

    let candidates = reconnect::reconnect_candidates(&intended, &paired, &connected);
    let due = {
        let tracker = state.reconnect.lock().await;
        tracker.due(&candidates, std::time::Instant::now())
    };

    for addr in due {
        let Ok(parsed) = addr.parse::<bluer::Address>() else {
            // A hand-edited store can hold anything; it must not stop the pass.
            tracing::warn!("auto-reconnect skipping the malformed remembered address '{addr}'");
            continue;
        };
        // Paired-only: the pass connects, it never bonds. The candidate was
        // paired a moment ago, and BlueZ is asked to hold that.
        let failure = match bluetooth::connect_paired_device(parsed).await {
            Ok(device) if device.connected => {
                tracing::info!("auto-reconnect brought {} back", device.address);
                state.reconnect.lock().await.record_success(&device.address);
                // The cache the next tick short-circuits on, and the one
                // `/select` validates against: kept in step exactly as
                // `/connect` does, rather than waiting for a `/devices` poll
                // that only happens while the app is open.
                let mut conn = state.connected.lock().await;
                if !conn.iter().any(|a| a == &device.address) {
                    conn.push(device.address);
                }
                continue;
            },
            // BlueZ accepted the dial but the link is not up: counted as a
            // failure, or the ladder would reset on every such pass and dial
            // that speaker every tick for as long as it misbehaves.
            Ok(_) => "the link did not come up".to_string(),
            Err(e) => e.to_string(),
        };
        let mut tracker = state.reconnect.lock().await;
        tracker.record_failure(&addr, std::time::Instant::now());
        if tracker.given_up(&addr) {
            tracing::info!("auto-reconnect gave up on {addr}: no answer after the last attempt");
        } else {
            tracing::warn!("auto-reconnect could not reach {addr}: {failure}");
        }
    }
}

/// Start the idle watchdog: once the now-playing SSE feed has had no reader for
/// longer than the grace period its presence allows, pause Spotify.
///
/// The app reports `Gone` when it closes, which pauses immediately; this covers
/// what that report cannot — a crash, an OOM kill, a dropped network — where the
/// PC would otherwise keep streaming to nobody.
fn spawn_idle_watchdog(state: AppState) {
    // A router built outside an async context (a bare unit test) has no runtime to
    // spawn on; the watchdog is a safety net, never a requirement.
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(watchdog::WATCHDOG_TICK).await;
            // Only worth anything while our own Connect backend is up: with
            // librespot stopped there is nothing of ours playing to pause.
            let running = {
                let mut spotify = state.spotify.lock().await;
                spotify.poll_liveness().status == SpotifyStatus::Running
            };
            // A backgrounded app is frozen by Android, so its feed drops without
            // the user having left: the grace period follows what the app reported.
            let grace = watchdog::grace_for(state.sse_watch.presence());
            if !running || !state.sse_watch.claim_idle_pause(grace) {
                continue;
            }
            tracing::info!("no now-playing reader for {grace:?}: pausing Spotify");
            let mut auth = state.spotify_auth.lock().await;
            if let Err(e) = auth.transport(Transport::Pause).await {
                // Nothing playing, or no login: not worth more than a trace.
                tracing::warn!("idle watchdog could not pause Spotify: {e}");
            }
        }
    });
}

/// `POST /play` — start (or resume) playback of the embedded test file, routed
/// through the PipeWire combined sink spanning the current target selection. An
/// empty selection (`Idle`) is rejected (4xx).
async fn play(State(state): State<AppState>) -> Result<Json<PlaybackState>, AppError> {
    forget_backend_pause(&state);
    // Snapshot the selection and release the guard before the blocking PipeWire calls.
    let speakers = state.targets.lock().await.speakers();
    audio::route_for_targets(&speakers)?;
    let mut engine = state.engine.lock().await;
    Ok(Json(engine.play()?))
}

/// `POST /pause` — pause playback (idempotent while stopped).
async fn pause(State(state): State<AppState>) -> Result<Json<PlaybackState>, AppError> {
    forget_backend_pause(&state);
    let mut engine = state.engine.lock().await;
    Ok(Json(engine.pause()?))
}

/// `POST /stop` — stop playback (idempotent while stopped).
async fn stop(State(state): State<AppState>) -> Result<Json<PlaybackState>, AppError> {
    forget_backend_pause(&state);
    let mut engine = state.engine.lock().await;
    Ok(Json(engine.stop()?))
}

/// Drop the backend's claim that it paused the sources (#67).
///
/// Every explicit transport command from the app goes through here: from that
/// point on the playback state is the user's, so a speaker coming back must not
/// undo it.
fn forget_backend_pause(state: &AppState) {
    state.backend_paused_sources.store(false, Ordering::SeqCst);
}

/// `POST /volume` — set the selected speakers' PipeWire sink volume (clamped),
/// applied to every target sink so both stay in step and round-trip with
/// `/playback`. An empty selection is rejected (4xx).
async fn volume(
    State(state): State<AppState>,
    Json(req): Json<VolumeRequest>,
) -> Result<Json<PlaybackState>, AppError> {
    // Snapshot the selection and release the guard before the PipeWire calls.
    let speakers = state.targets.lock().await.speakers();
    if speakers.is_empty() {
        return Err(AudioError::NoSpeakerConnected.into());
    }
    for target in &speakers {
        audio::set_sink_volume(&target.address, req.level)?;
    }
    let mut engine = state.engine.lock().await;
    Ok(Json(engine.set_volume(req.level)?))
}

/// `GET /playback` — current playback state, reconciled so it returns to
/// `Stopped` once the tone ends on its own, and carrying a volume that is true
/// of *every* selected speaker: the live sink level when they all agree (so a
/// change made on a speaker itself is reflected), the commanded level otherwise
/// — see `audio::reported_volume`.
async fn playback(State(state): State<AppState>) -> Json<PlaybackState> {
    let mut snapshot = {
        let mut engine = state.engine.lock().await;
        engine.poll_state()
    };
    // Snapshot the selection and release the guard before the PipeWire reads.
    let speakers = state.targets.lock().await.speakers();
    let levels: Vec<Option<f32>> = speakers
        .iter()
        .map(|target| audio::sink_volume(&target.address))
        .collect();
    snapshot.volume = audio::reported_volume(&levels, snapshot.volume);
    Json(snapshot)
}

/// `POST /spotify/start` — activate the Spotify source backend: snapshot the
/// current target selection (like `/play`) and spawn the `librespot` Connect
/// device pointed at the matching sink. An empty selection is rejected (400).
async fn spotify_start(State(state): State<AppState>) -> Result<Json<SpotifyState>, AppError> {
    // Snapshot the selection and release the guard before touching the backend.
    let speakers = state.targets.lock().await.speakers();
    let mut spotify = state.spotify.lock().await;
    Ok(Json(spotify.start(&speakers)?))
}

/// `POST /spotify/stop` — deactivate the Spotify source backend (kill the
/// subprocess), returning its reconciled state.
async fn spotify_stop(State(state): State<AppState>) -> Result<Json<SpotifyState>, AppError> {
    let mut spotify = state.spotify.lock().await;
    Ok(Json(spotify.stop()?))
}

/// `GET /spotify/status` — the Spotify backend's current state, reconciled so a
/// subprocess that exited on its own is reported as `Stopped`.
async fn spotify_status(State(state): State<AppState>) -> Json<SpotifyState> {
    let mut spotify = state.spotify.lock().await;
    Json(spotify.poll_liveness())
}

/// `GET /spotify/auth/url` — mint a PKCE authorize URL and CSRF `state` for the
/// app to open in the system browser; the pending verifier/state is remembered
/// server-side until the callback. Returns 503 when no client id is configured.
async fn spotify_auth_url(
    State(state): State<AppState>,
) -> Result<Json<AuthUrlResponse>, AppError> {
    let (url, csrf) = state.spotify_auth.lock().await.authorize_url()?;
    Ok(Json(AuthUrlResponse { url, state: csrf }))
}

/// `POST /spotify/auth/callback` — exchange the authorization `code` (validated
/// against the pending CSRF `state`) for tokens. A body missing `code` is a
/// malformed callback and is rejected with 400; the token exchange is a manual
/// network seam.
async fn spotify_auth_callback(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<Json<SpotifyAuthState>, AppError> {
    // Parse leniently so a missing `code` yields a 400 (not Axum's default 422).
    let req: AuthCallbackRequest = serde_json::from_slice(&body)
        .map_err(|e| AppError::bad_request(format!("invalid callback body: {e}")))?;
    let mut auth = state.spotify_auth.lock().await;
    Ok(Json(auth.exchange_code(&req.code, &req.state).await?))
}

/// `GET /spotify/auth/status` — the coarse auth state (Connected/Disconnected).
async fn spotify_auth_status(State(state): State<AppState>) -> Json<SpotifyAuthState> {
    Json(state.spotify_auth.lock().await.auth_state())
}

/// `POST /spotify/play` — resume Web API playback (409 while Disconnected).
async fn spotify_play(State(state): State<AppState>) -> Result<StatusCode, AppError> {
    spotify_transport(&state, Transport::Play).await
}

/// `POST /spotify/pause` — pause Web API playback (409 while Disconnected).
async fn spotify_pause(State(state): State<AppState>) -> Result<StatusCode, AppError> {
    spotify_transport(&state, Transport::Pause).await
}

/// `POST /spotify/next` — skip to the next track (409 while Disconnected).
async fn spotify_next(State(state): State<AppState>) -> Result<StatusCode, AppError> {
    spotify_transport(&state, Transport::Next).await
}

/// `POST /spotify/previous` — skip to the previous track (409 while Disconnected).
async fn spotify_previous(State(state): State<AppState>) -> Result<StatusCode, AppError> {
    spotify_transport(&state, Transport::Previous).await
}

/// Drive a transport action on the Spotify Web API, returning 204 on success.
/// While Disconnected the auth driver rejects before any outbound call (→ 409).
async fn spotify_transport(state: &AppState, action: Transport) -> Result<StatusCode, AppError> {
    forget_backend_pause(state);
    let mut auth = state.spotify_auth.lock().await;
    auth.transport(action).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /spotify/now-playing` — Server-Sent Events stream of now-playing
/// snapshots polled from the Web API. Emits a `now-playing` event per tick; a
/// Disconnected server keeps the stream alive with keep-alive comments only.
async fn spotify_now_playing(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let auth = state.spotify_auth.clone();
    let guard = state.sse_watch.subscribe();
    let stream = async_stream::stream! {
        // Held for the stream's lifetime: whichever way the stream ends (client
        // gone, task cancelled), dropping it starts the idle clock.
        let _guard = guard;
        loop {
            let snapshot = {
                let mut guard = auth.lock().await;
                guard.now_playing().await
            };
            if let Ok(now_playing) = snapshot {
                let event = Event::default()
                    .event("now-playing")
                    .json_data(now_playing)
                    .unwrap_or_else(|_| Event::default().comment("serialization failed"));
                yield Ok(event);
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// `POST /client/presence` — the app reports whether it is on screen, backgrounded
/// or closing. The backend cannot tell a frozen app from a dead one on its own, so
/// this drives the watchdog's grace period; `Gone` pauses playback straight away.
async fn client_presence(
    State(state): State<AppState>,
    Json(req): Json<PresenceRequest>,
) -> StatusCode {
    // Logged: this is the only visible trace that the app's lifecycle hooks are
    // reaching the backend at all (Android does not guarantee `onDestroy`).
    tracing::info!("client presence: {:?}", req.presence);
    state.sse_watch.set_presence(req.presence);
    if req.presence == ClientPresence::Gone {
        // The outcome is only used to decide the backend's resume claim, which
        // a closing app makes no promise about: nothing here will resume it.
        let _ = pause_spotify_now(&state).await;
    }
    StatusCode::NO_CONTENT
}

/// Pause the Spotify Web API playback, best-effort — a *resumable* pause that
/// leaves the `librespot` subprocess alive and the Connect device visible.
///
/// This is the intent shared by the two situations where playback should stop
/// but may well come back: the app reporting `Gone`, and the last speaker
/// vanishing while the setting promises to re-select it. In both the Connect
/// endpoint must survive — killing it because a phone was swiped away, or
/// because a speaker blinked, would be wrong. Nothing here is worth failing the
/// caller over, and Spotify answers 409 when there is nothing to pause anyway.
///
/// It is not the way to silence a teardown: the call returns when Spotify's
/// servers answer, which says nothing about the audio thread. The
/// empty-selection branch of `apply_selection_change` stops the subprocess
/// instead.
///
/// Returns whether it really silenced something, which is what licenses the
/// backend to claim the pause (see [`targets::may_claim_pause`]). Spotify
/// answers a restriction when there is nothing to pause, so a successful
/// `transport` call means playback was actually running. A network or token
/// failure lands on the same `false`, and that direction is the safe one: no
/// claim means a restoration resumes nothing, so the music stays paused rather
/// than starting again behind the user's back.
async fn pause_spotify_now(state: &AppState) -> bool {
    let running = {
        let mut spotify = state.spotify.lock().await;
        spotify.poll_liveness().status == SpotifyStatus::Running
    };
    if !running {
        return false;
    }
    let mut auth = state.spotify_auth.lock().await;
    match auth.transport(Transport::Pause).await {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!("could not pause Spotify on client exit: {e}");
            false
        },
    }
}

/// `GET /config` — the backend's current name.
async fn get_config(State(state): State<AppState>) -> Json<ServerConfig> {
    let stored = state.name.lock().await;
    Json(ServerConfig {
        name: stored.name().to_string(),
        restore_during_playback: stored.restore_during_playback(),
        auto_reconnect: stored.auto_reconnect(),
    })
}

/// `POST /config` — set the backend's name, which becomes its Spotify Connect
/// device name.
///
/// The name is re-validated here rather than trusted: a bearer says the caller
/// is the paired app, not that what it sent is well formed.
async fn set_config(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<Json<ServerConfig>, AppError> {
    // Parsed leniently so a malformed body is a 400 rather than Axum's 422.
    let req: ConfigRequest = serde_json::from_slice(&body)
        .map_err(|e| AppError::bad_request(format!("invalid config body: {e}")))?;
    let (name, restore_during_playback, auto_reconnect, resumed) = {
        let mut stored = state.name.lock().await;
        let name = stored
            .set_name(&req.name)
            .map_err(|e| AppError::bad_request(e.to_string()))?;
        // Applied only once the name was accepted, so a rejected body changes
        // nothing at all.
        stored.set_restore_during_playback(req.restore_during_playback);
        // Only the off → on edge, never every push: the app re-pushes the whole
        // config on activation, and re-arming there would reset a running
        // backoff ladder on each one.
        let resumed = !stored.auto_reconnect() && req.auto_reconnect;
        stored.set_auto_reconnect(req.auto_reconnect);
        // Read back rather than echoed: the response reports what the backend
        // actually holds, exactly as it does for the (trimmed) name.
        (
            name,
            stored.restore_during_playback(),
            stored.auto_reconnect(),
            resumed,
        )
    };
    // Switching the setting back on is the user asking for their speakers now:
    // a ladder that ran out (or a hang-up recorded) while it was off must not
    // leave the toggle looking inert.
    if resumed {
        state.reconnect.lock().await.rearm_all();
    }

    // The Web API lookup must follow the advertised name, or transport would 412
    // while blaming the user for not starting the backend.
    state.spotify_auth.lock().await.set_device_name(&name);

    let speakers = state.targets.lock().await.speakers();
    let mut spotify = state.spotify.lock().await;
    // `--name` is fixed at spawn: a running backend has to be respawned to be
    // renamed, which briefly drops the Connect device.
    let restart = spotify::should_restart_for_rename(
        spotify.poll_liveness().status,
        spotify.device_name(),
        &name,
    );
    spotify.set_device_name(&name);
    if restart {
        // Both halves are logged rather than propagated: the name *is* stored, so
        // a rename must not report failure because the subprocess dance did.
        if let Err(e) = spotify.stop() {
            tracing::warn!("could not stop the Spotify backend before a rename: {e}");
        }
        if let Err(e) = spotify.start(&speakers) {
            tracing::warn!("could not restart the Spotify backend after a rename: {e}");
        }
    }

    Ok(Json(ServerConfig {
        name,
        restore_during_playback,
        auto_reconnect,
    }))
}

/// `GET /health` — liveness probe carrying the backend version.
async fn health() -> Json<HealthStatus> {
    // Deliberately public, and it says so: the app needs to tell "not paired"
    // from "unreachable", which a 401 on the probe itself would hide.
    Json(HealthStatus::ok(env!("CARGO_PKG_VERSION")).with_auth_required(true))
}

/// `GET /adapters` — Bluetooth adapters present on the host.
async fn adapters() -> Result<Json<Vec<AdapterInfo>>, AppError> {
    Ok(Json(bluetooth::list_adapters().await?))
}

/// `GET /devices` — paired devices on the default adapter. Re-syncs the connected
/// cache with the live state so a speaker connected (or disconnected) outside the
/// app is reflected, and drops any disconnected speaker from the target selection.
async fn devices(State(state): State<AppState>) -> Result<Json<Vec<DeviceInfo>>, AppError> {
    let devices = bluetooth::list_paired_devices().await?;
    sync_connected(&state, &devices).await;
    Ok(Json(devices))
}

/// Refresh the connected-address cache from a device list and drop any selected
/// target that is no longer connected (keeping the selection and routing honest).
async fn sync_connected(state: &AppState, devices: &[DeviceInfo]) {
    let connected: Vec<String> = devices
        .iter()
        .filter(|d| d.connected)
        .map(|d| d.address.clone())
        .collect();
    // Clone: the cache owns one copy while the selection below is validated
    // against the other.
    *state.connected.lock().await = connected.clone();

    // A remembered speaker that is connected again — whether the pass dialled it
    // or it came back on its own — clears its retry ladder, so a later loss
    // starts over from the first backoff step. Skipped outright when nothing is
    // connected: this runs on every `/devices` poll of every client.
    if !connected.is_empty() {
        let intended = state.targets.lock().await.intended();
        let mut tracker = state.reconnect.lock().await;
        for addr in intended.iter().filter(|a| connected.contains(a)) {
            tracker.record_success(addr);
        }
    }

    let (lost_last_target, anything_to_restore) = {
        let mut targets = state.targets.lock().await;
        let had_targets = !targets.speakers().is_empty();
        targets.retain_connected(&connected);
        (
            had_targets && targets.speakers().is_empty(),
            !targets.restorable(&connected).is_empty(),
        )
    };

    // The last selected device just dropped off. Pruning the selection does not
    // touch the audio graph: the routing still points at that device's sink, and
    // PipeWire re-attaches the sink when it comes back — so the stream would
    // resume on a device blue2th no longer considers selected.
    let action = targets::action_on_last_loss(
        lost_last_target,
        state.name.lock().await.restore_during_playback(),
    );
    match action {
        targets::LastLossAction::Nothing => {},
        targets::LastLossAction::PauseSources => pause_sources_until_restored(state).await,
        targets::LastLossAction::QuietenAndTeardown => apply_selection_change(state, &[]).await,
    }
    // The overwhelmingly common case: this runs on every `/devices` poll (a
    // couple of seconds apart, per client), so a poll where nobody came back
    // must end here — without polling the engine, the Spotify subprocess or the
    // routing.
    if !anything_to_restore {
        return;
    }

    // Whether restoring right now is allowed: mid-playback it is opt-in, since
    // moving the target sink respawns `librespot` and cuts the sound. "Playing"
    // covers both sources — the local tone and the Spotify backend — or the
    // setting would be defeated by whichever one it ignored.
    let playing = {
        let mut engine = state.engine.lock().await;
        engine.poll_state().status == PlaybackStatus::Playing
    } || {
        let mut spotify = state.spotify.lock().await;
        spotify.poll_liveness().status == SpotifyStatus::Running
    };
    if !targets::should_restore(playing, state.name.lock().await.restore_during_playback()) {
        return;
    }

    // `restore` reports whether the selection really moved; re-routing
    // unconditionally would tear the PipeWire graph down and rebuild it on every
    // poll. Every guard is released before `apply_selection_change`, which takes
    // the engine and Spotify ones again.
    let speakers = {
        let mut targets = state.targets.lock().await;
        if !targets.restore(&connected) {
            return;
        }
        targets.speakers()
    };
    apply_selection_change(state, &speakers).await;
    // Only a pause the backend performed may be undone by the backend: without
    // that claim, a speaker coming back would restart music the user had paused.
    if targets::should_resume_after_restore(state.backend_paused_sources.load(Ordering::SeqCst)) {
        resume_sources_after_restore(state).await;
    }
}

/// Pause both sources because the last selected speaker vanished, and claim that
/// pause — but only when a source really was silenced — so a restoration may
/// undo it (#67).
///
/// The routing is deliberately left standing: the setting promises the speaker
/// comes back, so nothing may be destroyed under a still-running stream — which
/// is also what makes a *resumable* pause safe here, the Web API returning before
/// `librespot`'s audio thread has stopped no longer orphaning anything.
async fn pause_sources_until_restored(state: &AppState) {
    let spotify_silenced = pause_spotify_now(state).await;
    let engine_silenced = {
        let mut engine = state.engine.lock().await;
        // The engine's pause is a no-op unless it was `Playing`, so the status
        // on either side of the call is what says whether anything stopped.
        let before = engine.poll_state().status;
        match engine.pause() {
            Ok(after) => before != after.status,
            Err(e) => {
                tracing::warn!("could not pause playback after the last speaker was lost: {e}");
                false
            },
        }
    };
    if targets::may_claim_pause(spotify_silenced, engine_silenced) {
        state.backend_paused_sources.store(true, Ordering::SeqCst);
    }
}

/// Resume the sources the backend paused when the speakers vanished, and drop
/// the claim: it has been spent, so a later restoration resumes nothing on its
/// own.
async fn resume_sources_after_restore(state: &AppState) {
    state.backend_paused_sources.store(false, Ordering::SeqCst);
    {
        let mut auth = state.spotify_auth.lock().await;
        if let Err(e) = auth.transport(Transport::Play).await {
            // Nothing to resume, or no login: never worth failing the poll over.
            tracing::warn!("could not resume Spotify after the speakers came back: {e}");
        }
    }
    let mut engine = state.engine.lock().await;
    // Only a paused engine is resumed: `play()` on a stopped one would start the
    // tone from scratch, which the backend never paused.
    if engine.poll_state().status == PlaybackStatus::Paused {
        if let Err(e) = engine.play() {
            tracing::warn!("could not resume playback after the speakers came back: {e}");
        }
    }
}

/// `POST /devices/{addr}/connect` — pair/trust/connect a device, returning its
/// updated state. The device joins the connected cache so it becomes selectable
/// as a playback target (selection itself is explicit, via `/select`).
async fn connect(
    State(state): State<AppState>,
    Path(addr): Path<String>,
) -> Result<Json<DeviceInfo>, AppError> {
    let device = bluetooth::connect_device(parse_addr(&addr)?).await?;
    {
        let mut conn = state.connected.lock().await;
        if !conn.iter().any(|a| a == &device.address) {
            conn.push(device.address.clone());
        }
    }
    // The user acted on this speaker: clear any dismissal or give-up, so the
    // pass dials it again the next time it drops off.
    state.reconnect.lock().await.rearm(&device.address);
    Ok(Json(device))
}

/// `POST /devices/{addr}/disconnect` — disconnect a device, returning its updated
/// state. Drops it from the connected cache and from the target selection.
async fn disconnect(
    State(state): State<AppState>,
    Path(addr): Path<String>,
) -> Result<Json<DeviceInfo>, AppError> {
    let device = bluetooth::disconnect_device(parse_addr(&addr)?).await?;
    let connected = {
        let mut conn = state.connected.lock().await;
        conn.retain(|a| a != &device.address);
        conn.clone()
    };
    state.targets.lock().await.retain_connected(&connected);
    // The user hung this speaker up from the app: the intent (and its remembered
    // offset) is kept, but auto-reconnect must not dial it straight back — the
    // backend does not fight the user.
    state.reconnect.lock().await.dismiss(&device.address);
    Ok(Json(device))
}

/// `POST /devices/{addr}/select` — select a connected speaker as a playback
/// target. Rejected (4xx) if it is not connected or the two-speaker cap is hit.
async fn select_target(
    State(state): State<AppState>,
    Path(addr): Path<String>,
) -> Result<Json<TargetsState>, AppError> {
    let connected = { state.connected.lock().await.clone() };
    let (speakers, updated) = {
        let mut targets = state.targets.lock().await;
        targets.select(&addr, &connected)?;
        (targets.speakers(), targets.state())
    };
    // Selecting is the user asking for this speaker: re-arm it, exactly as
    // `/connect` does.
    state.reconnect.lock().await.rearm(&addr);
    // Symmetric with deselect: a speaker added mid-playback must be brought into
    // the routing, not just into the stored selection.
    apply_selection_change(&state, &speakers).await;
    Ok(Json(updated))
}

/// `POST /devices/{addr}/deselect` — drop a speaker from the target selection.
async fn deselect_target(
    State(state): State<AppState>,
    Path(addr): Path<String>,
) -> Json<TargetsState> {
    let (speakers, updated) = {
        let mut targets = state.targets.lock().await;
        targets.deselect(&addr);
        (targets.speakers(), targets.state())
    };
    // Dropping a speaker must actually stop the audio reaching it, not just
    // update the selection.
    apply_selection_change(&state, &speakers).await;
    Json(updated)
}

/// `POST /devices/{addr}/offset` — set a target speaker's latency offset (clamped
/// server-side to `0..=750` ms). A no-op if the speaker is not selected.
async fn set_target_offset(
    State(state): State<AppState>,
    Path(addr): Path<String>,
    Json(req): Json<OffsetRequest>,
) -> Json<TargetsState> {
    let (speakers, updated) = {
        let mut targets = state.targets.lock().await;
        targets.set_offset(&addr, req.offset_ms);
        (targets.speakers(), targets.state())
    };
    apply_offset_live(&state, &addr, &speakers).await;
    Json(updated)
}

/// Make a just-changed offset audible without replaying: the offset only exists
/// as `module-loopback` latency, so it has to be pushed into the live PipeWire
/// graph. Best-effort — a failure here must not turn a slider drag into an error,
/// and the new value is applied anyway on the next `/play` or Spotify start.
async fn apply_offset_live(state: &AppState, addr: &str, speakers: &[SpeakerTarget]) {
    let Some(target) = speakers.iter().find(|s| s.address == addr) else {
        return;
    };
    let plan = audio::combine_sink_plan(speakers);

    // An offset change never moves the target sink, so this does not respawn; it
    // stays because the target would move if the sink name ever became dynamic.
    if resync_spotify_sink(state, speakers).await {
        return;
    }

    if audio::combined_sink_exists(&plan.sink_name) {
        let branch = audio::CombineBranch {
            sink: audio::bluez_sink_prefix(&target.address),
            latency_ms: target.offset_ms,
        };
        if let Err(e) = audio::retune_combined_branch(&plan.sink_name, &branch) {
            tracing::warn!("could not retune the speaker offset live: {e}");
        }
    }
}

/// Respawn `librespot` when the selection moves it to a different sink.
/// `--device` is fixed at spawn, so re-routing alone would leave it feeding the
/// sink it was started with. Returns whether it was restarted.
async fn resync_spotify_sink(state: &AppState, speakers: &[SpeakerTarget]) -> bool {
    let mut spotify = state.spotify.lock().await;
    if spotify.poll_liveness().status != SpotifyStatus::Running {
        return false;
    }
    let wanted = spotify::spotify_target_sink(speakers);
    if spotify.current_sink() == Some(wanted.as_str()) {
        return false;
    }
    let _ = spotify.stop();
    if let Err(e) = spotify.start(speakers) {
        tracing::warn!("could not restart the Spotify backend after a routing change: {e}");
    }
    true
}

/// Push a selection change into the live audio graph.
///
/// Selecting or deselecting a speaker used to only update the stored selection:
/// the PipeWire routing stayed exactly as it was, so a speaker dropped from the
/// selection kept receiving the stream and playing on.
async fn apply_selection_change(state: &AppState, speakers: &[SpeakerTarget]) {
    if speakers.is_empty() {
        // Nothing left to play to. Silence both sources, then tear the combined
        // sink down so no loopback keeps feeding a speaker nobody selected.
        //
        // Spotify is *stopped*, not paused through the Web API: a remote pause
        // returns when Spotify's servers answer, not when `librespot`'s audio
        // thread has, and a failed call only warns. Unloading the null sink
        // under a still-streaming child makes PipeWire relocate that stream onto
        // the fallback, so the music comes out of the PC's own speakers (#67).
        // `stop()` is local and synchronous, so once it returns there is no
        // stream left to orphan.
        {
            let mut spotify = state.spotify.lock().await;
            if let Err(e) = spotify.stop() {
                tracing::warn!(
                    "could not stop the Spotify backend after the last speaker was dropped: {e}"
                );
            }
        }
        {
            let mut engine = state.engine.lock().await;
            if let Err(e) = engine.pause() {
                tracing::warn!("could not pause playback after the last speaker was dropped: {e}");
            }
        }
        if let Err(e) = audio::teardown_combined(spotify::COMBINED_SINK_NAME) {
            tracing::warn!("could not tear the combined sink down: {e}");
        }
        return;
    }
    // Still a target: rebuild the routing so it spans exactly the current
    // selection (this is what stops feeding a speaker that was just dropped).
    if let Err(e) = audio::route_for_targets(speakers) {
        tracing::warn!("could not re-route after a selection change: {e}");
        return;
    }
    resync_spotify_sink(state, speakers).await;
}

/// `GET /targets` — the current selection, per-speaker offsets and routing mode.
async fn get_targets(State(state): State<AppState>) -> Json<TargetsState> {
    Json(state.targets.lock().await.state())
}

/// Parse a path MAC address, returning a 400-style error on malformed input.
fn parse_addr(addr: &str) -> Result<bluer::Address, AppError> {
    addr.parse::<bluer::Address>()
        .map_err(|e| AppError::bad_request(format!("invalid address '{addr}': {e}")))
}

/// `GET /scan` — Server-Sent Events stream of devices discovered by an active
/// scan. Emits a `device` event per discovery and an `error` event on failure;
/// the scan stops after `SCAN_DURATION` or when the client disconnects.
async fn scan(State(state): State<AppState>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let deadline = tokio::time::sleep(SCAN_DURATION);
    // Arc clone so the stream closure can add a discovered connected speaker to
    // the connected cache as soon as it is seen (no need to wait for a /devices poll).
    let connected = state.connected.clone();
    let stream = bluetooth::scan_events()
        .map(move |result| {
            if let Ok(device) = &result {
                if device.connected {
                    if let Ok(mut guard) = connected.try_lock() {
                        // Best-effort cache update; skipped if the lock is momentarily
                        // held elsewhere. Removal is handled by /devices + /disconnect.
                        if !guard.iter().any(|a| a == &device.address) {
                            guard.push(device.address.clone());
                        }
                    }
                }
            }
            let event = match result {
                Ok(device) => Event::default()
                    .event("device")
                    .json_data(device)
                    .unwrap_or_else(|_| Event::default().comment("serialization failed")),
                Err(e) => Event::default().event("error").data(e.to_string()),
            };
            Ok(event)
        })
        .take_until(deadline);

    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// Error type for handlers: renders a status code with the message. BlueZ
/// failures (no adapter, bluetoothd down) convert into it via
/// `From<bluer::Error>`; audio failures via `From<AudioError>`.
struct AppError {
    status: StatusCode,
    message: String,
}

impl AppError {
    /// A 500 error carrying the given message (the historical behaviour).
    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }

    /// A 400 error carrying the given message (bad client input / preconditions).
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    /// A 409 error carrying the given message: the request collided with the
    /// state of something the backend does not own — a speaker refusing the
    /// bond — rather than with a bug on this side.
    fn conflict(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: message.into(),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        tracing::warn!("request failed: {}", self.message);
        (self.status, self.message).into_response()
    }
}

impl From<bluer::Error> for AppError {
    fn from(err: bluer::Error) -> Self {
        AppError::internal(err.to_string())
    }
}

impl From<bluetooth::ConnectError> for AppError {
    fn from(err: bluetooth::ConnectError) -> Self {
        // A refused bond is a conflict with the speaker's own state (409), any
        // other BlueZ failure a server fault (500). Both keep the BlueZ message
        // for the logs. Nothing on the app side reads these statuses globally —
        // the connect route alone gives the 409 its meaning — so sharing it with
        // `SpotifyApiError::NotConnected` is harmless.
        let message = err.to_string();
        match err {
            bluetooth::ConnectError::Pairing(_) => AppError::conflict(message),
            bluetooth::ConnectError::Bluetooth(_) => AppError::internal(message),
        }
    }
}

impl From<AudioError> for AppError {
    fn from(err: AudioError) -> Self {
        match err {
            // No connected speaker is a precondition failure, not a server bug.
            AudioError::NoSpeakerConnected => AppError::bad_request(err.to_string()),
            AudioError::Decode(_) | AudioError::PipeWire(_) => AppError::internal(err.to_string()),
        }
    }
}

impl From<SpotifyError> for AppError {
    fn from(err: SpotifyError) -> Self {
        match err {
            // No selected speaker is a precondition failure, not a server bug.
            SpotifyError::NoSpeakerSelected => AppError::bad_request(err.to_string()),
            // A missing binary or failed spawn is a backend/server-side fault.
            SpotifyError::BackendMissing | SpotifyError::Spawn(_) => {
                AppError::internal(err.to_string())
            },
        }
    }
}

impl From<SpotifyApiError> for AppError {
    fn from(err: SpotifyApiError) -> Self {
        let status = match err {
            // Server misconfiguration (no client id): nothing the app can fix.
            SpotifyApiError::NotConfigured => StatusCode::SERVICE_UNAVAILABLE,
            // Transport while Disconnected: a precondition conflict, not a bug.
            SpotifyApiError::NotConnected => StatusCode::CONFLICT,
            // The librespot backend must be started before targeting blue2th-PC.
            SpotifyApiError::BackendNotRunning => StatusCode::PRECONDITION_FAILED,
            // Access token rejected: the app must reauth (log in again).
            SpotifyApiError::Unauthorized => StatusCode::UNAUTHORIZED,
            // Non-Premium account: transport is forbidden by Spotify.
            SpotifyApiError::PremiumRequired => StatusCode::FORBIDDEN,
            // No active device to target.
            SpotifyApiError::NoActiveDevice => StatusCode::NOT_FOUND,
            // Rate limited by the Web API.
            SpotifyApiError::RateLimited(_) => StatusCode::TOO_MANY_REQUESTS,
            // Token exchange/refresh or any upstream HTTP failure: a bad gateway.
            SpotifyApiError::Exchange(_) | SpotifyApiError::Http(_) => StatusCode::BAD_GATEWAY,
        };
        AppError {
            status,
            message: err.to_string(),
        }
    }
}

impl From<SelectError> for AppError {
    fn from(err: SelectError) -> Self {
        // Both are bad client requests (not connected / cap exceeded), not server bugs.
        match err {
            SelectError::NotConnected => {
                AppError::bad_request("speaker is not connected".to_string())
            },
            SelectError::CapExceeded => {
                AppError::bad_request("at most two speakers can be selected".to_string())
            },
        }
    }
}

/// Initialise tracing from `RUST_LOG`, defaulting to `info`.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

#[cfg(test)]
mod tests {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use blue2th_proto::RoutingMode;
    use tower::ServiceExt;

    use super::*; // for `oneshot`

    /// The API token these tests pair with. Held in memory only: after phase 6.4
    /// no test may build the router through `app()`, which reloads (and, on a
    /// malformed store, rotates) the operator's real token.
    const TOKEN: &str = "test-api-token";

    /// A store-free router with a known API token.
    fn build_app() -> Router {
        app_with_auth_store(
            spotify_auth::SpotifyAuth::with_config(None, "blue2th://spotify-callback".to_string()),
            AuthStore::with_token(TOKEN),
        )
    }

    /// Add the bearer every guarded route requires.
    fn authorized(builder: axum::http::request::Builder) -> axum::http::request::Builder {
        builder.header("authorization", format!("Bearer {TOKEN}"))
    }

    #[tokio::test]
    async fn test_health_endpoint_returns_ok_status_and_version() {
        let request = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .expect("build request");

        let response = build_app().oneshot(request).await.expect("router response");
        assert_eq!(response.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let parsed: HealthStatus = serde_json::from_slice(&bytes).expect("parse HealthStatus");

        assert_eq!(parsed.status, "ok");
        assert_eq!(parsed.version, env!("CARGO_PKG_VERSION"));
    }

    // Criterion: server — `GET /health` announces `protocol` and `protocol_min`
    // equal to the proto constants, so the app can compare the wire contract
    // rather than guessing from the release version (#33).
    #[tokio::test]
    async fn test_health_endpoint_announces_the_protocol_range() {
        let request = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .expect("build request");

        let response = build_app().oneshot(request).await.expect("router response");
        assert_eq!(response.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let parsed: HealthStatus = serde_json::from_slice(&bytes).expect("parse HealthStatus");

        assert_eq!(
            parsed.protocol,
            blue2th_proto::PROTOCOL_VERSION,
            "the backend must announce the newest contract it speaks"
        );
        assert_eq!(
            parsed.protocol_min,
            blue2th_proto::MIN_SUPPORTED_PROTOCOL_VERSION,
            "the backend must announce the oldest client it still serves"
        );
    }

    // Criterion (phase 6.1, re-pointed in 6.4): the router still serves
    // `/targets`, and only the offsets are ever persisted — never the selection,
    // so a freshly built router reports nothing selected. Built store-free: the
    // offsets store itself is unit-tested in `targets.rs` against a temp path.
    #[tokio::test]
    async fn test_app_builds_with_the_offsets_store_and_restores_no_selection() {
        let request = authorized(Request::builder().uri("/targets"))
            .body(Body::empty())
            .expect("build request");

        let response = build_app().oneshot(request).await.expect("router response");
        assert_eq!(response.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let state: TargetsState = serde_json::from_slice(&bytes).expect("parse TargetsState");
        assert!(
            state.speakers.is_empty(),
            "the selection itself must never be restored, got {:?}",
            state.speakers
        );
        assert_eq!(state.routing, RoutingMode::Idle);
    }

    // Criterion: a `NoSpeakerSelected` error maps to a 400 (precondition failure),
    // so `POST /spotify/start` with no target rejects the client.
    #[test]
    fn test_spotify_no_speaker_selected_maps_to_bad_request() {
        let err: AppError = SpotifyError::NoSpeakerSelected.into();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    // Criterion: a `NotFound` spawn error (`BackendMissing`) maps to a 500 with a
    // clear message ("Spotify backend unavailable").
    #[test]
    fn test_spotify_backend_missing_maps_to_internal_error() {
        let err: AppError = SpotifyError::BackendMissing.into();
        assert_eq!(err.status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    // Criterion: any other spawn failure maps to a 500 carrying the OS message.
    #[test]
    fn test_spotify_spawn_error_maps_to_internal_error() {
        let err: AppError = SpotifyError::Spawn("permission denied".to_string()).into();
        assert_eq!(err.status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    // Criterion (phase 5.2): a transport call while Disconnected maps to 409.
    #[test]
    fn test_spotify_api_not_connected_maps_to_conflict() {
        let err: AppError = SpotifyApiError::NotConnected.into();
        assert_eq!(err.status, StatusCode::CONFLICT);
    }

    // Criterion (phase 5.2): a token exchange failure maps to 502 (Bad Gateway).
    #[test]
    fn test_spotify_api_exchange_failure_maps_to_bad_gateway() {
        let err: AppError = SpotifyApiError::Exchange("invalid code".to_string()).into();
        assert_eq!(err.status, StatusCode::BAD_GATEWAY);
    }

    // Criterion (phase 5.2): a Premium-required rejection maps to 403 (Forbidden).
    #[test]
    fn test_spotify_api_premium_required_maps_to_forbidden() {
        let err: AppError = SpotifyApiError::PremiumRequired.into();
        assert_eq!(err.status, StatusCode::FORBIDDEN);
    }

    // ---- #52: a Bluetooth pairing failure is not a server fault ----

    /// A BlueZ failure as `bluer` reports one, with a message the user can read.
    fn bluez_error(kind: bluer::ErrorKind, message: &str) -> bluer::Error {
        bluer::Error {
            kind,
            message: message.to_string(),
        }
    }

    // Criterion: a pairing failure maps to HTTP 409 — the speaker refused or
    // timed out, which is a conflict with the speaker's own state, not a bug in
    // the backend. The app types that status and leaves the row clickable
    // instead of greying it.
    //
    // Deliberately **not** 502: `SpotifyApiError::Exchange | ::Http` already map
    // there, so sharing the code would have the app read a failed Spotify token
    // exchange as a refused speaker.
    #[test]
    fn test_connect_error_pairing_maps_to_conflict() {
        let err: AppError = bluetooth::ConnectError::Pairing(bluez_error(
            bluer::ErrorKind::AuthenticationTimeout,
            "Authentication Timeout",
        ))
        .into();

        assert_eq!(err.status, StatusCode::CONFLICT);
    }

    // Criterion: 502 stays the Spotify upstream failure alone — a pairing
    // failure must never answer it again, or the two meanings collapse back
    // together.
    #[test]
    fn test_connect_error_pairing_is_never_bad_gateway() {
        for err in [
            bluetooth::ConnectError::Pairing(bluez_error(
                bluer::ErrorKind::AuthenticationFailed,
                "Authentication Failed",
            )),
            bluetooth::ConnectError::Pairing(bluez_error(
                bluer::ErrorKind::AuthenticationTimeout,
                "Authentication Timeout",
            )),
        ] {
            let mapped: AppError = err.into();
            assert_ne!(
                mapped.status,
                StatusCode::BAD_GATEWAY,
                "502 is the Spotify upstream failure, got {:?}",
                mapped.message
            );
        }
    }

    // Criterion: every other BlueZ failure keeps HTTP 500, so the app still
    // greys out a paired speaker that cannot be connected (powered off).
    #[test]
    fn test_connect_error_bluetooth_maps_to_internal_error() {
        let err: AppError = bluetooth::ConnectError::Bluetooth(bluez_error(
            bluer::ErrorKind::ConnectionAttemptFailed,
            "br-connection-page-timeout",
        ))
        .into();

        assert_eq!(err.status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    // Criterion: both arms keep the BlueZ message — it is the only thing telling
    // the operator (and the logs) what the adapter actually answered.
    #[test]
    fn test_connect_error_keeps_the_bluez_message_on_both_arms() {
        let pairing: AppError = bluetooth::ConnectError::Pairing(bluez_error(
            bluer::ErrorKind::AuthenticationFailed,
            "Authentication Failed",
        ))
        .into();
        assert!(
            pairing.message.contains("Authentication Failed"),
            "the pairing failure must carry the BlueZ message, got {:?}",
            pairing.message
        );

        let other: AppError = bluetooth::ConnectError::Bluetooth(bluez_error(
            bluer::ErrorKind::Failed,
            "br-connection-page-timeout",
        ))
        .into();
        assert!(
            other.message.contains("br-connection-page-timeout"),
            "any other failure must carry the BlueZ message, got {:?}",
            other.message
        );
    }

    // ---- phase 6.4: LAN bind address and the pairing banner ----

    // Criterion: `lan_bind_address()` yields to `BLUE2TH_BIND` — an operator who
    // asked for a specific address (or for every interface) always wins.
    #[test]
    fn test_bind_address_prefers_the_env_override() {
        assert_eq!(
            bind_address(
                Some("0.0.0.0:4000".to_string()),
                Some(std::net::Ipv4Addr::new(192, 168, 1, 107))
            ),
            "0.0.0.0:4000"
        );
    }

    // Criterion: without an override the backend binds its LAN address on the
    // standard port, rather than every interface.
    #[test]
    fn test_bind_address_uses_the_detected_lan_address() {
        assert_eq!(
            bind_address(None, Some(std::net::Ipv4Addr::new(192, 168, 1, 107))),
            format!("192.168.1.107:{DEFAULT_PORT}")
        );
    }

    // Criterion (non-nominal): with no LAN address resolvable (no interface up)
    // the server falls back to `0.0.0.0` rather than refusing to start.
    #[test]
    fn test_bind_address_falls_back_to_every_interface() {
        assert_eq!(bind_address(None, None), DEFAULT_BIND);
    }

    // Criterion: `lan_bind_address()` prefers a non-loopback IPv4.
    #[test]
    fn test_preferred_lan_ipv4_skips_loopback_and_link_local() {
        let candidates = [
            std::net::Ipv4Addr::new(127, 0, 0, 1),
            std::net::Ipv4Addr::new(169, 254, 3, 4),
            std::net::Ipv4Addr::new(192, 168, 1, 107),
        ];
        assert_eq!(
            preferred_lan_ipv4(&candidates),
            Some(std::net::Ipv4Addr::new(192, 168, 1, 107))
        );
    }

    // Criterion (non-nominal): loopback alone is no LAN address at all, so the
    // caller falls back to every interface.
    #[test]
    fn test_preferred_lan_ipv4_without_a_routable_address_is_none() {
        assert_eq!(
            preferred_lan_ipv4(&[std::net::Ipv4Addr::new(127, 0, 0, 1)]),
            None
        );
        assert_eq!(preferred_lan_ipv4(&[]), None);
    }

    // Criterion: `BLUE2TH_BIND` wins over the automatic choice, end to end.
    // The single env-mutating test of this binary, like `targets`' XDG one.
    #[test]
    fn test_lan_bind_address_yields_to_the_bind_env_var() {
        std::env::set_var(BIND_ENV, "10.1.2.3:4321");
        let chosen = lan_bind_address();
        std::env::remove_var(BIND_ENV);
        assert_eq!(chosen, "10.1.2.3:4321");
    }

    // Criterion: the QR is rendered as text the terminal can show — a square
    // block of lines, not the URL itself.
    #[test]
    fn test_pairing_qr_renders_a_text_block() {
        let link =
            blue2th_proto::pair_deep_link("http://192.168.1.107:4000", "blue2th-PC", "K7M2QX");
        let rendered = pairing_qr(&link);
        let lines: Vec<&str> = rendered.lines().filter(|l| !l.is_empty()).collect();
        assert!(
            lines.len() >= 21,
            "a QR is at least 21 modules across, got {} lines",
            lines.len()
        );
        assert!(
            lines
                .windows(2)
                .all(|w| w[0].chars().count() == w[1].chars().count()),
            "every QR row must be the same width"
        );
        assert!(
            !rendered.contains(&link),
            "the QR must encode the link, not print it"
        );
    }

    // Criterion: the QR encodes *that* URL — the render is a function of the
    // link and nothing else. Without a decoder here, the property is pinned the
    // way it can break: two links must not render the same block, and one link
    // must always render the same one.
    #[test]
    fn test_pairing_qr_encodes_the_link_it_is_given() {
        let link =
            blue2th_proto::pair_deep_link("http://192.168.1.107:4000", "blue2th-PC", "K7M2QX");
        let other =
            blue2th_proto::pair_deep_link("http://192.168.1.107:4000", "blue2th-PC", "AAAAAA");
        assert_eq!(
            pairing_qr(&link),
            pairing_qr(&link),
            "the same link must always render the same QR"
        );
        assert_ne!(
            pairing_qr(&link),
            pairing_qr(&other),
            "a different pairing code must produce a different QR, or it encodes something else"
        );
    }

    // Criterion: the QR carries the address the phone must call, so a backend
    // bound to every interface advertises its LAN address instead of `0.0.0.0`.
    #[test]
    fn test_advertised_url_replaces_the_wildcard_with_the_lan_address() {
        let lan = Some(std::net::Ipv4Addr::new(192, 168, 1, 107));
        assert_eq!(
            advertised_url_from(DEFAULT_BIND, lan),
            "http://192.168.1.107:4000"
        );
        assert_eq!(
            advertised_url_from("[::]:4000", lan),
            "http://192.168.1.107:4000"
        );
    }

    // Criterion: an address the operator chose is advertised as-is — resolving
    // it again could hand out an interface they deliberately avoided.
    #[test]
    fn test_advertised_url_keeps_an_explicit_bind_address() {
        assert_eq!(
            advertised_url_from(
                "10.1.2.3:4321",
                Some(std::net::Ipv4Addr::new(192, 168, 1, 107))
            ),
            "http://10.1.2.3:4321"
        );
    }

    // Criterion (non-nominal): with no LAN address to substitute, the wildcard
    // is advertised as-is rather than crashing the banner — the printed code is
    // the transport that still works.
    #[test]
    fn test_advertised_url_without_a_lan_address_keeps_the_bind_address() {
        assert_eq!(
            advertised_url_from(DEFAULT_BIND, None),
            "http://0.0.0.0:4000"
        );
        assert_eq!(
            advertised_url_from("no-port-here", None),
            "http://no-port-here"
        );
    }

    // Criterion (phase 6.6): the advertised record is built from the bound
    // address via `advertised_url_from`, so the wildcard-bind case resolves to
    // the LAN address, not `0.0.0.0` — a record no phone could ever call.
    #[test]
    fn test_advertised_service_resolves_the_wildcard_bind_to_the_lan_address() {
        let lan = Some(std::net::Ipv4Addr::new(192, 168, 1, 107));
        let record = advertised_service_from(DEFAULT_BIND, lan, "backend-id-42", "blue2th-PC");
        assert_eq!(record.url, "http://192.168.1.107:4000");
        assert_eq!(
            record.url,
            advertised_url_from(DEFAULT_BIND, lan),
            "the mDNS record and the pairing QR must advertise the same address"
        );
    }

    // Criterion (phase 6.6): the record carries the very host and port
    // `ServiceInfo` needs, so publishing never parses them back out of the URL
    // it just assembled — and they agree with that URL.
    #[test]
    fn test_advertised_service_carries_the_host_and_port_it_announces() {
        let lan = Some(std::net::Ipv4Addr::new(192, 168, 1, 107));
        let record = advertised_service_from(DEFAULT_BIND, lan, "backend-id-42", "blue2th-PC");
        assert_eq!(
            record.endpoint,
            Some(("192.168.1.107".to_string(), 4000)),
            "a wildcard bind announces the detected LAN host, not 0.0.0.0"
        );
        let (host, port) = record.endpoint.clone().expect("just asserted");
        assert_eq!(record.url, format!("http://{host}:{port}"));

        let explicit = advertised_service_from("10.1.2.3:4321", lan, "backend-id-42", "Salon");
        assert_eq!(
            explicit.endpoint,
            Some(("10.1.2.3".to_string(), 4321)),
            "an explicit bind address is announced as-is"
        );
    }

    // Criterion (non-nominal, phase 6.6): a bind address with no port — or one
    // that is not a number — leaves nothing an mDNS record could announce, so
    // the endpoint is absent and `advertise` declines instead of guessing.
    #[test]
    fn test_advertised_service_without_a_usable_port_has_no_endpoint() {
        for bind in ["no-port-here", "0.0.0.0:not-a-port"] {
            let record = advertised_service_from(bind, None, "backend-id-42", "blue2th-PC");
            assert_eq!(record.endpoint, None, "{bind} carries no port to announce");
            assert_eq!(
                record.url,
                format!("http://{bind}"),
                "{bind} is still shown verbatim in the banner"
            );
        }
    }

    // Criterion (phase 6.6): the record carries `id=<stable id>` and
    // `name=<backend name>` under the TXT keys declared once in proto.
    #[test]
    fn test_advertised_service_carries_the_id_and_the_name_txt_records() {
        let record = advertised_service_from(
            "10.1.2.3:4321",
            Some(std::net::Ipv4Addr::new(192, 168, 1, 107)),
            "backend-id-42",
            "Salon",
        );
        assert_eq!(
            record.url, "http://10.1.2.3:4321",
            "an explicit bind address is advertised as-is"
        );
        assert!(
            record.txt.contains(&(
                blue2th_proto::TXT_KEY_ID.to_string(),
                "backend-id-42".to_string()
            )),
            "the id must be published, or the app cannot repair an address: {:?}",
            record.txt
        );
        assert!(
            record
                .txt
                .contains(&(blue2th_proto::TXT_KEY_NAME.to_string(), "Salon".to_string())),
            "the configured name must be published: {:?}",
            record.txt
        );
    }

    // Criterion (phase 6.6): the published record round-trips through the shared
    // proto helper — what the server announces is what the app reads back.
    #[test]
    fn test_advertised_service_round_trips_through_discovered_from_txt() {
        let lan = Some(std::net::Ipv4Addr::new(192, 168, 1, 107));
        let record = advertised_service_from(DEFAULT_BIND, lan, "backend-id-42", "Salon");
        let txt: Vec<(&str, &str)> = record
            .txt
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(
            blue2th_proto::discovered_from_txt(&record.url, &txt),
            blue2th_proto::DiscoveredBackend {
                id: Some("backend-id-42".to_string()),
                name: "Salon".to_string(),
                url: "http://192.168.1.107:4000".to_string(),
            }
        );
    }

    // Criterion: the startup banner shows the code as text *and* the deep link
    // as a QR, so typing six characters and scanning are the same mechanism.
    #[test]
    fn test_pairing_banner_shows_the_code_and_the_qr() {
        let banner = pairing_banner("http://192.168.1.107:4000", "blue2th-PC", "K7M2QX");
        assert!(
            banner.contains("K7M2QX"),
            "the operator must be able to read the code, got {banner}"
        );
        assert!(
            banner.lines().count() >= 21,
            "the banner must carry the QR block, got {banner}"
        );
    }

    // Criterion (security): the QR carries the **code**, never the token — the
    // link travels through Android's intent system, which another app declaring
    // the `blue2th` scheme could listen to.
    #[test]
    fn test_pairing_banner_never_prints_the_api_token() {
        let mut store = AuthStore::with_token("super-secret-api-token");
        let code = store.arm_pairing(std::time::SystemTime::now());
        let banner = pairing_banner("http://192.168.1.107:4000", "blue2th-PC", &code);
        assert!(
            !banner.contains(store.token()),
            "the banner must never show the API token"
        );
    }
}
