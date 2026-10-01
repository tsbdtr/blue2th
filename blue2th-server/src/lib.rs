// SPDX-License-Identifier: MIT OR Apache-2.0

//! blue2th PC backend library facade.
//!
//! Exposes the Axum router (`app()`) and the server entry point (`run()`) so
//! both the binary (`main.rs`) and integration tests can drive the same surface
//! in-process. The route table and what each area owns are described in
//! `docs/ARCHITECTURE.md`.

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
    AdapterInfo, AudioGraphStatus, AuthCallbackRequest, AuthUrlResponse, ClientPresence,
    ConfigRequest, DeviceInfo, HealthStatus, OffsetRequest, PairRequest, PairResponse,
    PlaybackState, PlaybackStatus, PresenceRequest, ServerConfig, SpeakerTarget, SpotifyAuthState,
    SpotifyState, SpotifyStatus, SpotifyVolumeRequest, TargetsState, VolumeRequest,
};
use futures::{Stream, StreamExt};
use tokio::sync::Mutex;
use tracing_subscriber::EnvFilter;

pub mod audio;
pub mod auth;
mod bluetooth;
pub mod config;
pub mod graph;
pub mod graph_pw;
pub mod identity;
pub mod reconnect;
mod router_actor;
pub mod router_handle;
pub mod spotify;
pub mod spotify_auth;
pub mod spotify_volume;
mod state_store;
pub mod targets;
pub mod tone;
pub mod watchdog;

use audio::{AudioEngine, AudioError, AudioRouter};
use auth::AuthStore;
use router_handle::{RouterError, RouterHandle};
use spotify::{SpotifyBackend, SpotifyError};
use spotify_auth::{SpotifyApiError, SpotifyAuth, Transport};
use targets::{SelectError, SpeakerTargets};

/// Shared application state injected through the Axum router (no globals).
#[derive(Clone)]
pub struct AppState {
    /// The audio engine, guarded for concurrent access.
    engine: Arc<Mutex<AudioEngine>>,
    /// The routing logic over the audio graph, with the history its
    /// reconciliation carries from one pass to the next. A caller already
    /// holding `spotify` may take it, never the other way round; the routing
    /// applier reads `targets` while holding it, so nothing waits for it while
    /// holding `targets`.
    router: RouterHandle,
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
    /// The Spotify Connect volume policy (#58): the level the user chose, and
    /// whether a `librespot` respawn has reset it since. The now-playing poll
    /// runs it and performs the write it asks for.
    spotify_volume: Arc<Mutex<spotify_volume::Policy>>,
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
        path: "/spotify/volume",
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

    // The registry's speaker sink events drive the branch repair (#80).
    let (events, graph_events) = tokio::sync::mpsc::unbounded_channel();
    let mut graph = graph_pw::PipeWireGraph::spawn();
    graph.watch(events);
    let (router, state) = app_with_auth_and_targets(
        SpotifyAuth::new(),
        SpeakerTargets::with_store(targets::offsets_store_path()),
        server_name,
        auth_store,
        Box::new(graph),
    );
    spawn_event_repair(state.clone(), graph_events);
    spawn_confirmation_timer(state.clone());

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("blue2th-server listening on http://{addr}");

    // No graceful drain: the SSE streams never end, so waiting on the open
    // connections would wait forever. The serve future is simply dropped once
    // the signal wins, and the sources are stopped before returning.
    tokio::select! {
        served = axum::serve(listener, router) => served?,
        () = shutdown_signal() => {
            tracing::info!("shutting down: stopping the Spotify source");
            stop_sources_for_shutdown(&state).await;
        }
    }
    Ok(())
}

/// Resolves once the process is asked to stop: SIGINT (Ctrl-C) or SIGTERM
/// (`kill`, systemd). Handlers are registered on the first poll; a failure to
/// register one leaves that signal to its default action, which still stops
/// the process — only the `librespot` stop is lost, and the parent-death
/// signal covers that case.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sigterm) => {
                let _ = sigterm.recv().await;
            },
            Err(_) => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}

/// Stop every source the server owns before it exits (#122): the Spotify
/// backend through the same `stop()` `POST /spotify/stop` uses. Returns the
/// reconciled state. Idempotent: on a stopped backend it reports `Stopped`
/// again. The PipeWire graph is left in place — it is what keeps the speakers
/// routed across a restart.
pub async fn stop_sources_for_shutdown(state: &AppState) -> SpotifyState {
    let mut spotify = state.spotify.lock().await;
    // The process is exiting either way, so a failed stop has no error path
    // worth taking: the reconciled state is still the right thing to report.
    spotify.stop().unwrap_or_else(|_| spotify.poll_liveness())
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
        Box::new(graph_pw::PipeWireGraph::spawn()),
    )
    .0
}

/// Build the router around an explicit Spotify auth driver **and an explicit API
/// token** — the entry point every test uses, since it touches no store at all:
/// a test that let the server mint or reload the real token would unpair the
/// operator's phone (`AuthStore::with_token` keeps it in memory).
///
/// There is deliberately no variant that mints its own token: the caller could
/// not know it, so every guarded route would answer 401 and the test would look
/// broken for the wrong reason.
///
/// For the same reason it touches no audio graph either: its graph is
/// [`graph_pw::PipeWireGraph::detached`], which errs on every call the way a host
/// without PipeWire does. A graph over the session's daemon would let a test that
/// reaches `AudioRouter::teardown` — deselecting the last speaker does — destroy
/// the operator's live `blue2th_combined`.
pub fn app_with_auth_store(spotify_auth: SpotifyAuth, auth: AuthStore) -> Router {
    app_with_auth_and_targets(
        spotify_auth,
        SpeakerTargets::new(),
        config::ServerName::new(),
        auth,
        Box::new(graph_pw::PipeWireGraph::detached()),
    )
    .0
}

/// [`app_with_auth_store`], also handing back the [`AppState`] the router was
/// built around, so a test can drive the shutdown path against the very same
/// `SpotifyBackend` the routes hold (#122). Detached from any audio graph, like
/// [`app_with_auth_store`].
pub fn app_with_auth_store_and_state(
    spotify_auth: SpotifyAuth,
    auth: AuthStore,
) -> (Router, AppState) {
    app_with_auth_and_targets(
        spotify_auth,
        SpeakerTargets::new(),
        config::ServerName::new(),
        auth,
        Box::new(graph_pw::PipeWireGraph::detached()),
    )
}

/// Build the router around an explicit Spotify auth driver and an explicit
/// playback selection, so the on-disk seams stay in the caller's hands.
fn app_with_auth_and_targets(
    mut spotify_auth: SpotifyAuth,
    speaker_targets: SpeakerTargets,
    server_name: config::ServerName,
    auth: AuthStore,
    graph: Box<dyn graph::Graph>,
) -> (Router, AppState) {
    // The auth driver and the subprocess must start out agreeing with the stored
    // name, or the very first transport call would look up a device nobody
    // advertises.
    spotify_auth.set_device_name(server_name.name());
    let spotify = SpotifyBackend::with_name(server_name.name());
    // The persisted lock must guard from the first poll on, not from the next
    // `POST /config`.
    let mut volume_policy = spotify_volume::Policy::new();
    volume_policy.set_lock(server_name.spotify_volume_lock());
    let state = AppState {
        // Real playback output: a PipeWire stream pinned to the combined sink
        // (#66). It connects lazily on the first `/play`, so building the router
        // stays cheap and CI-safe.
        engine: Arc::new(Mutex::new(AudioEngine::with_output(Box::new(
            tone::PipeWireToneOutput::new(spotify::COMBINED_SINK_NAME),
        )))),
        router: RouterHandle::new(AudioRouter::new(graph)),
        targets: Arc::new(Mutex::new(speaker_targets)),
        connected: Arc::new(Mutex::new(Vec::new())),
        spotify: Arc::new(Mutex::new(spotify)),
        spotify_auth: Arc::new(Mutex::new(spotify_auth)),
        sse_watch: Arc::new(watchdog::SseWatch::default()),
        name: Arc::new(Mutex::new(server_name)),
        auth: Arc::new(Mutex::new(auth)),
        reconnect: Arc::new(Mutex::new(reconnect::ReconnectTracker::new())),
        backend_paused_sources: Arc::new(AtomicBool::new(false)),
        spotify_volume: Arc::new(Mutex::new(volume_policy)),
    };

    spawn_idle_watchdog(state.clone());
    spawn_auto_reconnect(state.clone());
    spawn_branch_repair(state.clone());
    spawn_routing_applier(state.clone());

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
    // Cheap: every field is an `Arc`, and the shutdown path needs the same
    // handles the routes hold.
    (router.with_state(state.clone()), state)
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
        ("POST", "/spotify/volume") => post(spotify_volume),
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

/// Start the safety-net repair pass (#75, #80): re-run the routing
/// reconciliation every [`audio::SAFETY_NET_TICK`], for what no registry event
/// reports — the combined sink destroyed by hand, a branch ruled dead. The
/// speakers' sinks appearing and vanishing are handled by
/// [`spawn_event_repair`] as they happen.
///
/// A tick of its own rather than work on the `/devices` poll, which runs per
/// client: the cost would otherwise multiply by the number of connected apps.
fn spawn_branch_repair(state: AppState) {
    // A router built outside an async context (a bare unit test) has no runtime
    // to spawn on, and a test must never drive the developer's own PipeWire graph.
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    tokio::spawn(async move {
        loop {
            // Sleeps first, unlike `spawn_auto_reconnect`: at startup the graph is
            // whatever the previous run left, nothing is playing yet, and the
            // selection is restored by its own pass. A repair on the first
            // instant would only reconcile against a selection nobody asked for.
            tokio::time::sleep(audio::SAFETY_NET_TICK).await;
            branch_repair_pass(&state, audio::PassReason::SafetyNet).await;
        }
    });
}

/// Start the background routing applier (#145): the single task a selection
/// change hands its routing to, so a change made while the graph does not
/// answer is applied once it does, rather than dropped at the request's bound.
///
/// Each pass takes the router with the unbounded wait and only then reads the
/// selection: requests made while it waited fold into that one pass, which
/// applies the latest selection rather than any snapshot taken on the way.
fn spawn_routing_applier(state: AppState) {
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    let mut requests = state.router.routing_requests();
    tokio::spawn(async move {
        while requests.changed().await.is_ok() {
            let speakers = {
                let mut router = state.router.lock_unbounded().await;
                requests.borrow_and_update();
                let speakers = state.targets.lock().await.speakers();
                if speakers.is_empty() {
                    if let Err(e) = router.teardown(spotify::COMBINED_SINK_NAME) {
                        tracing::warn!("could not tear the combined sink down: {e}");
                    }
                    continue;
                }
                if let Err(e) = router.route_for_targets(&speakers) {
                    tracing::warn!("could not re-route after a selection change: {e}");
                    continue;
                }
                speakers
            };
            // After the router is released: a Spotify respawn takes the
            // `spotify` guard first, then the router.
            resync_spotify_sink(&state, &speakers).await;
        }
    });
}

/// Start the event consumer (#80): drain the graph's events, and run one repair
/// pass per drain when at least one of them wakes one for the selection (see
/// [`audio::wake_for_burst`]). The task ends once every sender is gone and the
/// queue is drained.
fn spawn_event_repair(
    state: AppState,
    mut events: tokio::sync::mpsc::UnboundedReceiver<graph_pw::GraphEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(first) = events.recv().await {
            let speakers = state.targets.lock().await.speakers();
            if let Some(reason) = drain_burst_reason(first, &mut events, &speakers) {
                branch_repair_pass(&state, reason).await;
            }
        }
    })
}

/// Drain the burst `first` opens — `first`, then every event already queued
/// behind it in `events` — and answer the reason the one pass it wakes for
/// `speakers` carries, if it wakes one (#139).
fn drain_burst_reason(
    first: graph_pw::GraphEvent,
    events: &mut tokio::sync::mpsc::UnboundedReceiver<graph_pw::GraphEvent>,
    speakers: &[blue2th_proto::SpeakerTarget],
) -> Option<audio::PassReason> {
    let mut burst = Vec::new();
    let mut next = Some(first);
    while let Some(event) = next {
        tracing::debug!("graph event: {event:?}");
        burst.push(event);
        next = events.try_recv().ok();
    }
    audio::wake_for_burst(&burst, speakers)
}

/// Start the confirmation timer (#80): sleep until the router's earliest
/// confirming reload falls due, then run one pass for it. After every pass it
/// reads the due time again; with nothing armed it waits for the router to arm
/// a reload.
fn spawn_confirmation_timer(state: AppState) {
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    tokio::spawn(async move {
        let armed = state.router.lock_unbounded().await.confirmation_armed();
        loop {
            let due = state.router.lock_unbounded().await.next_confirmation_due();
            let Some(due) = due else {
                armed.notified().await;
                continue;
            };
            tokio::time::sleep_until(tokio::time::Instant::from_std(due)).await;
            branch_repair_pass(&state, audio::PassReason::ConfirmationDue).await;
            // A pass that could not take the reload — nothing playing, a graph
            // that cannot be read — leaves it due: wait for a new arming or a
            // gap, rather than spinning on a time already past.
            if state.router.lock_unbounded().await.next_confirmation_due() == Some(due) {
                tokio::select! {
                    () = armed.notified() => {},
                    () = tokio::time::sleep(audio::CONFIRM_GAP) => {},
                }
            }
        }
    });
}

/// One repair pass: reconcile the combined sink against the current selection.
///
/// A graph that already matches the plan is left untouched, so the common case
/// costs nothing and the audio runs on; a branch that failed to load — the race
/// where PipeWire had not created the `bluez_output.*` node yet — or one ruled
/// dead is loaded again alone, and the speakers already playing are not touched
/// (see `audio::reconcile_branches`). A failure stays a warning: the next
/// event, confirmation or safety-net tick tries again.
///
/// `reason` is what woke the pass. It is logged only when the pass changed the
/// graph, so a pass that found everything in place leaves no line.
///
/// Answers whether the pass fell back to pausing Spotify (#139), which only a
/// pass woken by the combined sink's removal ever does, and only when it could
/// not rebuild the sink or re-target the streams onto it.
async fn branch_repair_pass(state: &AppState, reason: audio::PassReason) -> bool {
    let speakers = state.targets.lock().await.speakers();
    // "Playing" covers both sources — the local tone and the Spotify backend —
    // exactly as the restore pass reads it: a branch that carries nothing only
    // matters while something is flowing towards it.
    let anything_playing = {
        let mut engine = state.engine.lock().await;
        engine.poll_state().status == PlaybackStatus::Playing
    } || {
        let mut spotify = state.spotify.lock().await;
        spotify.poll_liveness().status == SpotifyStatus::Running
    };
    // The single guard, and it runs before the graph is touched: an idle backend — no
    // selection, or nothing playing — asks PipeWire nothing at all.
    if !audio::should_repair_branches(&speakers, anything_playing) {
        return false;
    }
    let (routed, changed, retargeted_ok) = {
        let mut router = state.router.lock_unbounded().await;
        let before = router.graph_changes();
        let routed = router.route_for_targets(&speakers);
        (
            routed,
            router.graph_changes() != before,
            !router.last_retarget_failed(),
        )
    };
    if let Some(line) = repair_pass_line(&reason, changed, std::time::Instant::now()) {
        tracing::info!("{line}");
    }
    let routed_ok = routed.is_ok();
    if let Err(e) = routed {
        tracing::warn!("repair pass (woken by: {reason}) could not re-route: {e}");
    }
    if !audio::fallback_pause_due(&reason, routed_ok, retargeted_ok) {
        return false;
    }
    // Spotify only (#139): the fallback exists for `librespot`'s stream, which
    // the session manager moves onto the PC's own speakers once the sink it
    // asked for is gone.
    tracing::warn!(
        "repair pass (woken by: {reason}) could not bring the streams back: pausing Spotify"
    );
    let spotify_silenced = pause_spotify_now(state).await;
    claim_fallback_pause(state, spotify_silenced);
    true
}

/// Claim the repair pass's fallback pause (#139) when it silenced a playing
/// `librespot`. A pause that silenced nothing leaves the claim as it was: a
/// claim an earlier pause made is still owed its resume.
fn claim_fallback_pause(state: &AppState, spotify_silenced: bool) {
    // The fallback never touches the engine, so it has no half to report.
    if targets::may_claim_pause(spotify_silenced, false) {
        state.backend_paused_sources.store(true, Ordering::SeqCst);
    }
}

/// The line a repair pass woken by `reason` logs once it is done, at `now`;
/// `None` when it did not change the graph — the safety net runs every 30 s,
/// and a pass that found everything in place leaves no line.
///
/// For a sink that appeared, the line carries the time from the event to `now`:
/// from the sink's arrival to its branch being loaded, the gap #75's race is
/// measured by.
fn repair_pass_line(
    reason: &audio::PassReason,
    changed: bool,
    now: std::time::Instant,
) -> Option<String> {
    if !changed {
        return None;
    }
    Some(match reason {
        audio::PassReason::SinkAppeared { at, .. } => format!(
            "repair pass (woken by: {reason}) changed the graph {} ms after the event",
            now.saturating_duration_since(*at).as_millis()
        ),
        _ => format!("repair pass (woken by: {reason}) changed the graph"),
    })
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
            if !running {
                continue;
            }
            // A backgrounded app is frozen by Android, so its feed drops without
            // the user having left: the grace period follows what the app reported.
            // Read once, so the log names the presence the grace was derived from.
            let presence = state.sse_watch.presence();
            let grace = watchdog::grace_for(presence);
            let Some(idle) = state
                .sse_watch
                .claim_idle_pause(grace, std::time::Instant::now())
            else {
                continue;
            };
            let idle = idle.as_secs();
            let grace = grace.as_secs();
            tracing::info!(
                "no now-playing reader for {idle}s under {presence:?} (grace {grace}s): pausing Spotify"
            );
            let mut auth = state.spotify_auth.lock().await;
            if let Err(e) = auth.transport(Transport::Pause).await {
                // Nothing playing, or no login: not worth more than a trace.
                tracing::warn!("idle watchdog could not pause Spotify: {e}");
            }
        }
    });
}

/// `POST /play` — start (or resume) playback of the test tone, routed
/// through the PipeWire combined sink spanning the current target selection. An
/// empty selection (`Idle`) is rejected (4xx).
async fn play(State(state): State<AppState>) -> Result<Json<PlaybackState>, AppError> {
    forget_backend_pause(&state);
    // Snapshot the selection and release the guard before the blocking PipeWire calls.
    let speakers = state.targets.lock().await.speakers();
    state.router.route(&speakers).await?;
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
    let macs: Vec<String> = speakers.into_iter().map(|target| target.address).collect();
    state.router.set_sink_volumes(&macs, req.level).await?;
    let mut engine = state.engine.lock().await;
    Ok(Json(engine.set_volume(req.level)?))
}

/// `GET /playback` — current playback state, reconciled so it returns to
/// `Stopped` once the tone ends on its own, and carrying a volume that is true
/// of *every* selected speaker: the live sink level when they all agree (so a
/// change made on a speaker itself is reflected), the commanded level otherwise
/// — see `audio::reported_volume`.
///
/// A router not obtained in time, a sink list that cannot be read (#145), or a
/// speaker's level read that fails (#148) is not a failed poll: the reply
/// carries the commanded level and says the graph is unresponsive. A listed
/// sink with no level is not a failure.
async fn playback(State(state): State<AppState>) -> Json<PlaybackState> {
    let mut snapshot = {
        let mut engine = state.engine.lock().await;
        engine.poll_state()
    };
    // Snapshot the selection and release the guard before the PipeWire reads.
    let macs: Vec<String> = state
        .targets
        .lock()
        .await
        .speakers()
        .into_iter()
        .map(|target| target.address)
        .collect();
    // Nothing selected, nothing to read: the graph is not asked.
    if macs.is_empty() {
        return Json(snapshot);
    }
    match state.router.sink_volumes(&macs).await {
        Ok(levels) => {
            snapshot.volume = audio::reported_volume(&levels, snapshot.volume);
            snapshot.audio_graph = AudioGraphStatus::Responsive;
        },
        Err(e) => {
            tracing::warn!("could not read the speakers' volume: {e}");
            snapshot.audio_graph = AudioGraphStatus::Unresponsive;
        },
    }
    Json(snapshot)
}

/// `POST /spotify/start` — activate the Spotify source backend: snapshot the
/// current target selection (like `/play`) and spawn the `librespot` Connect
/// device pointed at the matching sink. An empty selection is rejected (400).
async fn spotify_start(State(state): State<AppState>) -> Result<Json<SpotifyState>, AppError> {
    // Snapshot the selection and release the guard before touching the backend.
    let speakers = state.targets.lock().await.speakers();
    let mut spotify = state.spotify.lock().await;
    Ok(Json(start_spotify(&state, &mut spotify, &speakers).await?))
}

/// Spawn `librespot` through the caller's guard and mark the respawn for the
/// volume policy (#58): every start is `--initial-volume 100`, so the level the
/// user chose is gone until the next poll writes it back. Every start site goes
/// through here, or one path would silently leave the Connect level at full
/// scale.
///
/// The router wait is bounded: a request answers 503 rather than queue behind
/// a graph that does not answer.
async fn start_spotify(
    state: &AppState,
    spotify: &mut SpotifyBackend,
    speakers: &[SpeakerTarget],
) -> Result<SpotifyState, AppError> {
    let started = state.router.start_spotify(spotify, speakers).await??;
    state.spotify_volume.lock().await.mark_respawned();
    Ok(started)
}

/// [`start_spotify`] for the background routing applier, whose router wait is
/// unbounded: a respawn delayed is better than a `librespot` left stopped.
async fn start_spotify_in_background(
    state: &AppState,
    spotify: &mut SpotifyBackend,
    speakers: &[SpeakerTarget],
) -> Result<SpotifyState, SpotifyError> {
    let started = {
        let mut router = state.router.lock_unbounded().await;
        spotify.start(&mut router, speakers)?
    };
    state.spotify_volume.lock().await.mark_respawned();
    Ok(started)
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

/// `POST /spotify/volume` — set the Spotify Connect level (#58), 204 on success.
///
/// The checks answer in this order: the body (400), then the lock (409, naming
/// it — a Disconnected backend is a 409 too, and the app must tell them apart),
/// then the Web API, whose errors map exactly as the transport routes' do.
async fn spotify_volume(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> Result<StatusCode, AppError> {
    // Parsed leniently so a malformed body is a 400 rather than Axum's 422.
    let req: SpotifyVolumeRequest = serde_json::from_slice(&body)
        .map_err(|e| AppError::bad_request(format!("invalid volume body: {e}")))?;
    if req.percent > 100 {
        return Err(AppError::bad_request(format!(
            "volume {} % is above 100",
            req.percent
        )));
    }
    if state.name.lock().await.spotify_volume_lock() {
        return Err(AppError::conflict(
            "the Spotify volume is pinned to 100 by spotify_volume_lock",
        ));
    }
    state
        .spotify_auth
        .lock()
        .await
        .set_volume(req.percent)
        .await?;
    state.spotify_volume.lock().await.on_user_set(req.percent);
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /spotify/now-playing` — Server-Sent Events stream of now-playing
/// snapshots polled from the Web API. Emits a `now-playing` event per tick; a
/// Disconnected server keeps the stream alive with keep-alive comments only.
async fn spotify_now_playing(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let auth = state.spotify_auth.clone();
    // Shared with the stream, which outlives this handler's borrow of `state`.
    let policy = state.spotify_volume.clone();
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
                // The policy lock is never held across the Web API call: the
                // route and the start sites take it too.
                let action = policy.lock().await.on_observed(now_playing.volume_percent);
                if let Some(percent) = action {
                    let written = auth.lock().await.set_volume(percent).await;
                    match written {
                        Ok(()) => policy.lock().await.written(),
                        // The mark stays: the write is retried at the next poll.
                        Err(e) => tracing::warn!(
                            "could not restore the Spotify Connect level to {percent} %: {e}"
                        ),
                    }
                }
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
    state
        .sse_watch
        .set_presence(req.presence, std::time::Instant::now());
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
/// caller over.
///
/// It is not the way to silence a teardown: the call returns when Spotify's
/// servers answer, which says nothing about the audio thread. The
/// empty-selection branch of `apply_selection_change` stops the subprocess
/// instead.
///
/// Returns whether it really silenced something, which is what licenses the
/// backend to claim the pause (see [`targets::may_claim_pause`]). That answer
/// comes from a `now_playing()` snapshot taken *before* the pause, because the
/// pause call cannot give it: Spotify answers 2xx to a pause on an
/// already-paused player, so `transport` returning `Ok` proves nothing (see
/// [`targets::spotify_was_playing`]). A snapshot that cannot be taken at all —
/// network, token — lands on `false`, and that direction is the safe one: no
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
    let was_playing = match auth.now_playing().await {
        Ok(snapshot) => targets::spotify_was_playing(snapshot.state),
        Err(e) => {
            tracing::warn!("could not read the Spotify playback state before pausing: {e}");
            false
        },
    };
    if let Err(e) = auth.transport(Transport::Pause).await {
        tracing::warn!("could not pause Spotify: {e}");
        return false;
    }
    was_playing
}

/// `GET /config` — the backend's current name.
async fn get_config(State(state): State<AppState>) -> Json<ServerConfig> {
    let stored = state.name.lock().await;
    Json(ServerConfig {
        name: stored.name().to_string(),
        restore_during_playback: stored.restore_during_playback(),
        auto_reconnect: stored.auto_reconnect(),
        spotify_volume_lock: stored.spotify_volume_lock(),
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
    let (name, restore_during_playback, auto_reconnect, spotify_volume_lock, resumed) = {
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
        // Absent means "not mentioned", never "off": the app re-pushes its whole
        // config on activation, and a client without the field must not undo
        // the guard each time (#58).
        if let Some(lock) = req.spotify_volume_lock {
            stored.set_spotify_volume_lock(lock);
        }
        // Read back rather than echoed: the response reports what the backend
        // actually holds, exactly as it does for the (trimmed) name.
        (
            name,
            stored.restore_during_playback(),
            stored.auto_reconnect(),
            stored.spotify_volume_lock(),
            resumed,
        )
    };
    // The policy is what the poll runs: the stored flag alone would not pin
    // anything until a restart.
    state
        .spotify_volume
        .lock()
        .await
        .set_lock(spotify_volume_lock);
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
        if let Err(e) = start_spotify(&state, &mut spotify, &speakers).await {
            tracing::warn!(
                "could not restart the Spotify backend after a rename: {}",
                e.message
            );
        }
    }

    Ok(Json(ServerConfig {
        name,
        restore_during_playback,
        auto_reconnect,
        spotify_volume_lock,
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

    // Pruning the selection does not touch the audio graph: the routing still
    // points at the sink that just went, and PipeWire re-attaches it when the
    // device comes back — so a stream left running would play on a speaker
    // blue2th no longer considers selected. Which of the three answers that
    // calls for depends on whether anything promises to re-select it, so the
    // decision is asked for on every pass and is `Nothing` on almost all of them.
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
        // Spelled out as the one transition `pause` can perform rather than as
        // `before != after`: both calls reconcile, so a tone that ended between
        // them also moves the status (Playing → Stopped) without this having
        // silenced anything, and a claim made there would resume Spotify behind
        // the user's back.
        let before = engine.poll_state().status;
        match engine.pause() {
            Ok(after) => {
                before == PlaybackStatus::Playing && after.status == PlaybackStatus::Paused
            },
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
/// as the delay of the speaker's branch, so it has to be pushed into the live
/// PipeWire graph, where it is set on the delay node in place. Best-effort — a
/// failure here must not turn a slider drag into an error, and the next
/// reconciliation (the repair tick, `/play`, a Spotify start) retunes the branch
/// anyway.
///
/// A router not obtained in time hands the change to the routing applier
/// (#145): its pass reconciles every branch to the offsets stored by then,
/// so the latest one is applied once the graph answers, and a Spotify sink
/// that moved is resynced there too.
async fn apply_offset_live(state: &AppState, addr: &str, speakers: &[SpeakerTarget]) {
    let Some(target) = speakers.iter().find(|s| s.address == addr) else {
        return;
    };
    let plan = audio::combine_sink_plan(speakers);
    let branch = audio::CombineBranch {
        sink: audio::bluez_sink_prefix(&target.address),
        latency_ms: target.offset_ms,
    };
    match state.router.retune(&plan.sink_name, &branch).await {
        Ok(()) => {},
        Err(RouterError::TimedOut) => state.router.request_routing(),
        Err(RouterError::Audio(e)) => {
            tracing::warn!("could not retune the speaker offset live: {e}");
        },
        Err(e @ (RouterError::Superseded | RouterError::Outdated)) => {
            tracing::warn!("could not retune the speaker offset live: {e}");
        },
    }
}

/// Respawn `librespot` when the selection moves it to a different sink.
/// `--device` is fixed at spawn, so re-routing alone would leave it feeding the
/// sink it was started with.
async fn resync_spotify_sink(state: &AppState, speakers: &[SpeakerTarget]) {
    let mut spotify = state.spotify.lock().await;
    if spotify.poll_liveness().status != SpotifyStatus::Running {
        return;
    }
    let wanted = spotify::spotify_target_sink(speakers);
    if spotify.current_sink() == Some(wanted.as_str()) {
        return;
    }
    let _ = spotify.stop();
    if let Err(e) = start_spotify_in_background(state, &mut spotify, speakers).await {
        tracing::warn!("could not restart the Spotify backend after a routing change: {e}");
    }
}

/// Push a selection change into the live audio graph.
///
/// Selecting or deselecting a speaker used to only update the stored selection:
/// the PipeWire routing stayed exactly as it was, so a speaker dropped from the
/// selection kept receiving the stream and playing on.
///
/// The routing itself goes to the background applier (#145), which applies
/// the selection current once it holds the router: a change made while the
/// graph does not answer is neither lost nor delays the reply.
async fn apply_selection_change(state: &AppState, speakers: &[SpeakerTarget]) {
    if speakers.is_empty() {
        // Nothing left to play to. Silence both sources, then tear the combined
        // sink down so no branch keeps feeding a speaker nobody selected.
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
    }
    // The applier tears the combined sink down on an empty selection, and
    // otherwise rebuilds the routing so it spans exactly the current one
    // (this is what stops feeding a speaker that was just dropped).
    state.router.request_routing();
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

    /// A 503 error carrying the given message (#145): the backend is up, but
    /// something it depends on did not answer in time.
    fn service_unavailable(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: message.into(),
        }
    }

    /// The 503 of an audio graph that did nothing for the request: the router
    /// could not be had in time (#145), or the graph thread took the command
    /// out of its queue too late to start it (#146). One answer for both — the
    /// client cannot tell them apart, and has nothing different to do.
    fn graph_not_answering() -> Self {
        Self::service_unavailable("the audio graph is not answering")
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
            AudioError::PipeWire(_) => AppError::internal(err.to_string()),
            // Nothing was done to the graph: unavailable, not a server fault.
            AudioError::Expired => AppError::graph_not_answering(),
        }
    }
}

impl From<RouterError> for AppError {
    fn from(err: RouterError) -> Self {
        match err {
            RouterError::TimedOut => AppError::graph_not_answering(),
            RouterError::Audio(e) => e.into(),
            // Stub (#147): `POST /volume` owes a superseded set a 200.
            RouterError::Superseded | RouterError::Outdated => AppError::internal(err.to_string()),
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
    use blue2th_proto::{AudioGraphStatus, RoutingMode};
    use tower::ServiceExt;

    use super::*; // for `oneshot`

    /// The API token these tests pair with. Held in memory only: after phase 6.4
    /// no test may build the router through `app()`, which reloads (and, on a
    /// malformed store, rotates) the operator's real token.
    const TOKEN: &str = "test-api-token";

    /// A store-free router with a known API token.
    fn build_app() -> Router {
        app_with_auth_and_targets(
            spotify_auth::SpotifyAuth::with_config(None, "blue2th://spotify-callback".to_string()),
            SpeakerTargets::new(),
            config::ServerName::new(),
            AuthStore::with_token(TOKEN),
            // In memory: a route test must never drive the developer's own
            // PipeWire graph.
            Box::new(graph::fake::FakeGraph::new()),
        )
        .0
    }

    /// An `AppState` with every seam kept off the network and off the hardware:
    /// a `NullOutput` engine, a `SpotifyBackend` holding no child, and a
    /// Disconnected `SpotifyAuth` — whose `now_playing`/`transport` fail on the
    /// missing token before any outbound call. That is what lets the claim's
    /// lifecycle (`backend_paused_sources`) be driven end to end in a test:
    /// nothing below it reaches PipeWire, `librespot` or the Web API.
    ///
    /// Built by hand rather than through `app_with_auth_and_targets`, which
    /// returns a `Router` and hides the state these tests have to read back.
    fn test_state() -> AppState {
        test_state_with_engine(AudioEngine::new())
    }

    /// The same fixture around an explicit engine, so a test can supply an
    /// [`audio::AudioOutput`] that behaves differently from `NullOutput`.
    fn test_state_with_engine(engine: AudioEngine) -> AppState {
        test_state_on(engine, &graph::fake::FakeGraph::new())
    }

    /// The same fixture over an explicit in-memory graph, so a test can seed it
    /// and read back the calls the routes made.
    fn test_state_on(engine: AudioEngine, fake: &graph::fake::FakeGraph) -> AppState {
        AppState {
            engine: Arc::new(Mutex::new(engine)),
            // The fake actor's router owns a handle onto the same graph state
            // the test keeps.
            router: RouterHandle::over_fake(fake),
            targets: Arc::new(Mutex::new(SpeakerTargets::new())),
            connected: Arc::new(Mutex::new(Vec::new())),
            spotify: Arc::new(Mutex::new(SpotifyBackend::new())),
            spotify_auth: Arc::new(Mutex::new(SpotifyAuth::with_config(
                None,
                "blue2th://spotify-callback".to_string(),
            ))),
            sse_watch: Arc::new(watchdog::SseWatch::default()),
            name: Arc::new(Mutex::new(config::ServerName::new())),
            auth: Arc::new(Mutex::new(AuthStore::with_token(TOKEN))),
            reconnect: Arc::new(Mutex::new(reconnect::ReconnectTracker::new())),
            backend_paused_sources: Arc::new(AtomicBool::new(false)),
            spotify_volume: Arc::new(Mutex::new(spotify_volume::Policy::new())),
        }
    }

    /// Add the bearer every guarded route requires.
    fn authorized(builder: axum::http::request::Builder) -> axum::http::request::Builder {
        builder.header("authorization", format!("Bearer {TOKEN}"))
    }

    // Criterion (#122): the shutdown stop goes through `SpotifyBackend::stop`
    // — the child the backend holds is killed and reaped, and the reconciled
    // state reads `Stopped`. Driven against a real subprocess (`sleep 30`)
    // adopted by the backend: without it, a shutdown path that merely reported
    // the state, or dropped the handle without killing, stayed green.
    #[tokio::test]
    async fn test_stop_sources_for_shutdown_kills_the_running_child() {
        let state = test_state();
        let child =
            spotify::spawn_bound_to_this_thread("sleep", &["30".to_string()]).expect("spawn sleep");
        let pid = nix::unistd::Pid::from_raw(i32::try_from(child.id()).expect("pid fits an i32"));
        state.spotify.lock().await.adopt_child_for_test(child);
        assert_eq!(
            state.spotify.lock().await.status().status,
            blue2th_proto::SpotifyStatus::Running,
            "the fixture must start from a Running backend"
        );

        let reconciled = stop_sources_for_shutdown(&state).await;

        assert_eq!(reconciled.status, blue2th_proto::SpotifyStatus::Stopped);
        // Signal 0 probes without delivering: `ESRCH` means the child was
        // killed *and* reaped, not left as a zombie or still sleeping.
        assert_eq!(
            nix::sys::signal::kill(pid, None),
            Err(nix::errno::Errno::ESRCH),
            "the child must be gone after the shutdown stop"
        );
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

    // ---- #67: the claim's lifecycle, end to end ----
    //
    // The pure decisions in `targets.rs` are pinned there; what these cover is
    // the wiring around them, which mutation testing found unpinned: making
    // `pause_sources_until_restored` a no-op, storing the claim unconditionally
    // instead of asking `may_claim_pause`, making `resume_sources_after_restore`
    // a no-op, and dropping `forget_backend_pause` from a transport handler all
    // left the suite green.

    // Criterion: the backend may claim the pause only when it actually silenced
    // a source that was playing — with a stopped engine and no `librespot`,
    // nothing is silenced, so nothing is claimed. This is the row that undid a
    // user's pause in manual testing: the loss path used to re-claim a pause it
    // had not performed, and a returning speaker then resumed music the user had
    // stopped.
    #[tokio::test]
    async fn test_pausing_after_the_last_loss_claims_nothing_when_nothing_played() {
        let state = test_state();
        // Name what "nothing playing" is worth here, so the assertion below
        // cannot pass because the fixture happened to be in some other state.
        assert_eq!(
            state.engine.lock().await.poll_state().status,
            PlaybackStatus::Stopped,
            "the fixture must start with a stopped engine"
        );
        assert_eq!(
            state.spotify.lock().await.poll_liveness().status,
            SpotifyStatus::Stopped,
            "the fixture must start with no librespot subprocess"
        );

        pause_sources_until_restored(&state).await;

        assert!(
            !state.backend_paused_sources.load(Ordering::SeqCst),
            "neither source was silenced: a restoration must resume nothing"
        );
    }

    // Criterion: losing the last speaker pauses the sources, and the engine half
    // of the claim comes from the status on either side of `AudioEngine::pause`
    // — a playing engine really stops, and that is worth claiming.
    #[tokio::test]
    async fn test_pausing_after_the_last_loss_pauses_a_playing_engine_and_claims_it() {
        let state = test_state();
        let started = state.engine.lock().await.play().expect("engine plays");
        assert_eq!(
            started.status,
            PlaybackStatus::Playing,
            "the engine must really be playing before the loss path runs"
        );

        pause_sources_until_restored(&state).await;

        assert_eq!(
            state.engine.lock().await.poll_state().status,
            PlaybackStatus::Paused,
            "the last speaker went: the engine must be paused, not left running"
        );
        assert!(
            state.backend_paused_sources.load(Ordering::SeqCst),
            "the backend silenced a playing engine, so it may claim the pause"
        );
    }

    // Criterion: a restoration resumes the sources the backend paused, and spends
    // the claim — so a second restoration, which paused nothing, resumes nothing.
    #[tokio::test]
    async fn test_resuming_after_a_restore_plays_the_engine_and_spends_the_claim() {
        let state = test_state();
        state.engine.lock().await.play().expect("engine plays");
        pause_sources_until_restored(&state).await;
        assert!(
            state.backend_paused_sources.load(Ordering::SeqCst),
            "the claim must be set before a restoration can spend it"
        );

        resume_sources_after_restore(&state).await;

        assert_eq!(
            state.engine.lock().await.poll_state().status,
            PlaybackStatus::Playing,
            "the speaker came back: the engine the backend paused must play again"
        );
        assert!(
            !state.backend_paused_sources.load(Ordering::SeqCst),
            "the claim is spent: a later restoration must resume nothing on its own"
        );
    }

    // Criterion: a restoration never resumes an engine the backend did not pause
    // — `resume_sources_after_restore` only ever un-pauses, it does not start the
    // tone from scratch.
    #[tokio::test]
    async fn test_resuming_after_a_restore_never_starts_a_stopped_engine() {
        let state = test_state();
        assert_eq!(
            state.engine.lock().await.poll_state().status,
            PlaybackStatus::Stopped,
            "the fixture must start with a stopped engine"
        );

        resume_sources_after_restore(&state).await;

        assert_eq!(
            state.engine.lock().await.poll_state().status,
            PlaybackStatus::Stopped,
            "nothing was paused: a restoration must not start the tone from scratch"
        );
    }

    /// An output whose tone ends on its own **after** the first `is_finished`
    /// question, which is what makes the reconcile inside `AudioEngine::pause`
    /// disagree with the one just before it.
    ///
    /// `NullOutput` never finishes, so with it the two reconciles always agree
    /// and `before != after.status` cannot be told apart from the rule it stands
    /// for. This is the output that tells them apart.
    #[derive(Default)]
    struct FinishesOnTheSecondPoll {
        polls: std::sync::atomic::AtomicUsize,
    }

    impl audio::AudioOutput for FinishesOnTheSecondPoll {
        fn start(&mut self) -> Result<(), AudioError> {
            Ok(())
        }
        fn resume(&mut self) -> Result<(), AudioError> {
            Ok(())
        }
        fn pause(&mut self) -> Result<(), AudioError> {
            Ok(())
        }
        fn stop(&mut self) -> Result<(), AudioError> {
            Ok(())
        }
        fn is_finished(&self) -> bool {
            self.polls.fetch_add(1, Ordering::SeqCst) > 0
        }
    }

    // Criterion: the backend may claim the pause only when it actually silenced
    // a source that was playing — a tone that reached its own end between the
    // two reconciles silenced itself, and claiming it would make a returning
    // speaker resume Spotify behind the user's back. The status *moves* here
    // (Playing → Stopped), so a rule written as "the status changed" claims it;
    // only the rule naming the transition `pause` can perform does not.
    #[tokio::test]
    async fn test_a_tone_that_ended_on_its_own_is_not_a_pause_the_backend_may_claim() {
        let state = test_state_with_engine(AudioEngine::with_output(
            Box::<FinishesOnTheSecondPoll>::default(),
        ));
        let started = state.engine.lock().await.play().expect("engine plays");
        assert_eq!(
            started.status,
            PlaybackStatus::Playing,
            "the engine must really be playing before the loss path runs"
        );

        pause_sources_until_restored(&state).await;

        assert_eq!(
            state.engine.lock().await.poll_state().status,
            PlaybackStatus::Stopped,
            "the tone ended on its own: the engine is stopped, not paused"
        );
        assert!(
            !state.backend_paused_sources.load(Ordering::SeqCst),
            "nothing was silenced by the backend: a restoration must resume nothing"
        );
    }

    // Criterion: an explicit transport command from the app clears the backend's
    // claim, so a pause the user asked for is never undone by a speaker coming
    // back. `POST /pause` is the command that caused the regression.
    #[tokio::test]
    async fn test_an_explicit_pause_forgets_the_backend_claim() {
        let state = test_state();
        state.backend_paused_sources.store(true, Ordering::SeqCst);

        // Cloned because the handler takes its state by value, as Axum hands
        // it over; the `Arc`s inside are what the assertion below reads back.
        let response = pause(State(state.clone())).await;
        assert!(
            response.is_ok(),
            "pausing a stopped engine is a no-op, not an error"
        );

        assert!(
            !state.backend_paused_sources.load(Ordering::SeqCst),
            "the playback state is the user's now: a restoration must not undo it"
        );
    }

    // Criterion: the same holds for `POST /stop`.
    #[tokio::test]
    async fn test_an_explicit_stop_forgets_the_backend_claim() {
        let state = test_state();
        state.backend_paused_sources.store(true, Ordering::SeqCst);

        let response = stop(State(state.clone())).await;
        assert!(
            response.is_ok(),
            "stopping a stopped engine is a no-op, not an error"
        );

        assert!(!state.backend_paused_sources.load(Ordering::SeqCst));
    }

    // Criterion: and for a Spotify transport command — including one that fails.
    // The claim is dropped *before* the call, because what makes the state the
    // user's is that they asked, not that Spotify obliged: a 409 from a
    // Disconnected driver must still leave the pause theirs.
    #[tokio::test]
    async fn test_a_failed_spotify_transport_still_forgets_the_backend_claim() {
        let state = test_state();
        state.backend_paused_sources.store(true, Ordering::SeqCst);

        let response = spotify_transport(&state, Transport::Pause).await;
        assert!(
            response.is_err(),
            "the fixture is Disconnected, so the transport call must be rejected"
        );

        assert!(
            !state.backend_paused_sources.load(Ordering::SeqCst),
            "the user asked for this: the claim goes whether or not Spotify answered"
        );
    }

    // Criterion: `POST /client/presence` records the report and stamps it with
    // `Instant::now()`. The route pins in `tests/presence.rs` only see the 204,
    // which a handler that never touched the watch would still return; this one
    // reads the watch back. `t1` is taken strictly after the departure at `t0`, so
    // a handler that left the clock at `t0` would report a non-zero idle time.
    #[tokio::test]
    async fn test_client_presence_records_the_report_and_restarts_the_idle_clock() {
        let state = test_state();
        let watch = Arc::clone(&state.sse_watch);
        assert_eq!(watch.presence(), ClientPresence::Foreground);

        let t0 = std::time::Instant::now();
        watch.subscribe().release_at(t0);
        let t1 = loop {
            let now = std::time::Instant::now();
            if now > t0 {
                break now;
            }
        };

        let status = client_presence(
            State(state),
            Json(PresenceRequest {
                presence: ClientPresence::Background,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        assert_eq!(watch.presence(), ClientPresence::Background);
        // The handler stamped `Instant::now()`, at or after `t1`: read at `t1`, the
        // idle time saturates to zero. Left at `t0`, it would be `t1 - t0 > 0`.
        assert_eq!(
            watch.claim_idle_pause(Duration::ZERO, t1),
            Some(Duration::ZERO),
            "the handler must restart the idle clock at the report"
        );
    }

    // Criterion (#79): `/play` with a selection routes through the state's
    // `AudioRouter` — the in-memory graph receives the build calls, in order,
    // and the engine plays.
    #[tokio::test]
    async fn test_play_with_a_selection_builds_the_combined_sink_on_the_graph() {
        use graph::fake::{FakeGraph, GraphCall};

        let mac = "AA:BB:CC:DD:EE:01";
        let sink = "bluez_output.AA_BB_CC_DD_EE_01.1";
        let fake = FakeGraph::with_sinks(&[sink]);
        let state = test_state_on(AudioEngine::new(), &fake);
        state
            .targets
            .lock()
            .await
            .select(mac, &[mac.to_string()])
            .expect("a connected speaker can be selected");

        let played = play(State(state)).await;

        assert!(
            matches!(&played, Ok(Json(p)) if p.status == PlaybackStatus::Playing),
            "play failed: {:?}",
            played.map(|Json(p)| p.status).map_err(|_| "error response")
        );
        assert_eq!(
            fake.routing_calls(),
            vec![
                GraphCall::ClearStaleDefaultSink {
                    sink_name: "blue2th_combined".to_string()
                },
                GraphCall::Teardown {
                    sink_name: "blue2th_combined".to_string()
                },
                GraphCall::CreateCombinedSink {
                    sink_name: "blue2th_combined".to_string()
                },
                GraphCall::LoadBranch {
                    sink_name: "blue2th_combined".to_string(),
                    real_sink: sink.to_string(),
                    latency_ms: 0
                },
            ]
        );
    }

    // Criterion (#81): `POST /devices/{addr}/offset` on a playing speaker
    // retunes its branch in place through `apply_offset_live` — one
    // `set_branch_delay` on that speaker's branch, at exactly the offset with
    // no base on top, and no unload or load. The other speaker is untouched.
    #[tokio::test]
    async fn test_offset_change_retunes_the_speaker_branch_in_place() {
        use graph::fake::{FakeGraph, GraphCall};

        let mac_a = "AA:BB:CC:DD:EE:01";
        let mac_b = "AA:BB:CC:DD:EE:02";
        let sink_a = "bluez_output.AA_BB_CC_DD_EE_01.1";
        let sink_b = "bluez_output.AA_BB_CC_DD_EE_02.1";
        let fake = FakeGraph::with_sinks(&[sink_a, sink_b, "blue2th_combined"]);
        let a = fake.seed_branch("blue2th_combined", sink_a, 0, Some(true));
        let b = fake.seed_branch("blue2th_combined", sink_b, 0, Some(true));
        let state = test_state_on(AudioEngine::new(), &fake);
        state
            .targets
            .lock()
            .await
            .select(mac_a, &[mac_a.to_string(), mac_b.to_string()])
            .expect("a connected speaker can be selected");
        state
            .targets
            .lock()
            .await
            .select(mac_b, &[mac_a.to_string(), mac_b.to_string()])
            .expect("a connected speaker can be selected");

        let _ = set_target_offset(
            State(state),
            Path(mac_b.to_string()),
            Json(OffsetRequest { offset_ms: 120 }),
        )
        .await;

        assert_eq!(
            fake.calls(),
            vec![GraphCall::SetBranchDelay {
                id: b,
                delay_ms: 120
            }]
        );
        let delays: Vec<(u32, u32)> = fake
            .loaded("blue2th_combined")
            .iter()
            .map(|l| (l.id, l.branch.latency_ms))
            .collect();
        assert_eq!(delays, vec![(a, 0), (b, 120)]);
    }

    // ─── #80: the event consumer and the confirmation timer ──────────────────
    //
    // Driven with the in-memory graph and a real `unbounded_channel`: nothing
    // here reaches a PipeWire daemon.

    /// The JBL Xtreme 3 and its sink, as #81's manual verification and a live
    /// `pw-dump` (2026-09-26/27) named them; the WH-1000XM5's sink.
    const JBL: &str = "2C:FD:B4:D3:AC:21";
    const JBL_SINK: &str = "bluez_output.2C_FD_B4_D3_AC_21.1";
    const SONY_SINK: &str = "bluez_output.80_99_E7_63_50_29.1";
    const COMBINED_SINK: &str = "blue2th_combined";

    fn sink_appeared(name: &str) -> graph_pw::GraphEvent {
        graph_pw::GraphEvent::SinkAppeared {
            name: name.to_string(),
            at: std::time::Instant::now(),
        }
    }

    fn sink_vanished(name: &str) -> graph_pw::GraphEvent {
        graph_pw::GraphEvent::SinkVanished {
            name: name.to_string(),
            at: std::time::Instant::now(),
        }
    }

    /// A state with the JBL selected over a steady graph — the combined sink
    /// up and the JBL's branch live, nothing armed — so every repair pass
    /// reads the branches exactly once and changes nothing. Playing when
    /// `playing` is set. The calls made while setting it up are forgotten.
    async fn steady_state(fake: &graph::fake::FakeGraph, playing: bool) -> AppState {
        fake.add_sink(JBL_SINK);
        fake.add_sink(COMBINED_SINK);
        fake.seed_branch(COMBINED_SINK, JBL_SINK, 0, Some(true));
        let state = test_state_on(AudioEngine::new(), fake);
        state
            .targets
            .lock()
            .await
            .select(JBL, &[JBL.to_string()])
            .expect("a connected speaker can be selected");
        if playing {
            state
                .engine
                .lock()
                .await
                .play()
                .expect("the null output plays");
        }
        fake.clear_calls();
        state
    }

    /// How many repair passes reached the graph: each one reads the combined
    /// sink's branches once on a steady graph.
    fn passes(fake: &graph::fake::FakeGraph) -> usize {
        fake.all_calls()
            .iter()
            .filter(|call| matches!(call, graph::fake::GraphCall::Branches { .. }))
            .count()
    }

    /// Queue `events`, close the channel, and run the consumer until it has
    /// drained them all and ended.
    async fn consume(state: &AppState, events: Vec<graph_pw::GraphEvent>) {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        for event in events {
            sender.send(event).expect("the receiver is alive");
        }
        drop(sender);
        // Cheap: every field of the state is an `Arc`.
        let task = spawn_event_repair(state.clone(), receiver);
        let ended = tokio::time::timeout(Duration::from_secs(5), task).await;
        assert!(
            matches!(ended, Ok(Ok(()))),
            "the consumer ends once its channel is closed and drained: {ended:?}"
        );
    }

    // Criterion (guard, one pass per burst): three events queued together —
    // the speaker's sink vanishing and coming back, and a reconnection, each
    // of which wakes on its own — are drained together and run exactly one
    // pass, not three.
    #[tokio::test]
    async fn test_event_consumer_runs_one_pass_for_a_burst_of_events() {
        let fake = graph::fake::FakeGraph::new();
        let state = steady_state(&fake, true).await;

        consume(
            &state,
            vec![
                sink_vanished(JBL_SINK),
                sink_appeared(JBL_SINK),
                graph_pw::GraphEvent::Reconnected,
            ],
        )
        .await;

        assert_eq!(passes(&fake), 1, "calls: {:?}", fake.all_calls());
    }

    // Criterion (non-nominal): an event for a sink no selected speaker names
    // — the unselected Sony, a longer address that only starts like the
    // JBL's — runs no pass: the graph is not even read. The control, on the
    // same state: the JBL's own sink runs one.
    #[tokio::test]
    async fn test_event_consumer_ignores_an_event_for_an_unselected_sink() {
        let fake = graph::fake::FakeGraph::new();
        let state = steady_state(&fake, true).await;

        consume(
            &state,
            vec![
                sink_appeared(SONY_SINK),
                sink_vanished(SONY_SINK),
                sink_appeared("bluez_output.2C_FD_B4_D3_AC_21_02.1"),
            ],
        )
        .await;
        assert!(fake.all_calls().is_empty(), "calls: {:?}", fake.all_calls());

        consume(&state, vec![sink_appeared(JBL_SINK)]).await;
        assert_eq!(passes(&fake), 1, "control: the JBL's sink wakes a pass");
    }

    // Criterion (non-nominal): while nothing plays, an event for a selected
    // speaker runs no pass — `should_repair_branches` still guards it, and the
    // graph is built at the next play. The control: once playing, the same
    // events run one.
    #[tokio::test]
    async fn test_event_consumer_runs_no_pass_while_nothing_plays() {
        let fake = graph::fake::FakeGraph::new();
        let state = steady_state(&fake, false).await;

        consume(
            &state,
            vec![sink_appeared(JBL_SINK), graph_pw::GraphEvent::Reconnected],
        )
        .await;
        assert!(fake.all_calls().is_empty(), "calls: {:?}", fake.all_calls());

        state
            .engine
            .lock()
            .await
            .play()
            .expect("the null output plays");
        consume(
            &state,
            vec![sink_appeared(JBL_SINK), graph_pw::GraphEvent::Reconnected],
        )
        .await;
        assert_eq!(
            passes(&fake),
            1,
            "control: the same events wake a pass once playing"
        );
    }

    /// A state whose actor's router reads tokio's paused clock, with the JBL selected
    /// and the null output playing, over `fake`.
    async fn timed_state(fake: &graph::fake::FakeGraph) -> AppState {
        let mut state = test_state_on(AudioEngine::new(), fake);
        state.router = RouterHandle::over_fake_with_clock(
            fake,
            Arc::new(|| tokio::time::Instant::now().into_std()),
        );
        state
            .targets
            .lock()
            .await
            .select(JBL, &[JBL.to_string()])
            .expect("a connected speaker can be selected");
        state
            .engine
            .lock()
            .await
            .play()
            .expect("the null output plays");
        state
    }

    /// The branch load into the JBL's sink.
    fn jbl_load() -> graph::fake::GraphCall {
        graph::fake::GraphCall::LoadBranch {
            sink_name: COMBINED_SINK.to_string(),
            real_sink: JBL_SINK.to_string(),
            latency_ms: 0,
        }
    }

    /// Load the JBL's branch through the pass its sink's arrival wakes, which
    /// arms its confirming reload; the calls are forgotten. Returns the id of
    /// the one branch loaded.
    async fn load_the_jbl(state: &AppState, fake: &graph::fake::FakeGraph) -> u32 {
        branch_repair_pass(
            state,
            audio::PassReason::SinkAppeared {
                name: JBL_SINK.to_string(),
                at: std::time::Instant::now(),
            },
        )
        .await;
        assert_eq!(fake.calls(), vec![jbl_load()]);
        let loaded: Vec<u32> = fake.loaded(COMBINED_SINK).iter().map(|b| b.id).collect();
        assert_eq!(loaded.len(), 1);
        fake.clear_calls();
        loaded[0]
    }

    // Criterion: a pass that changed nothing logs nothing, whatever woke it —
    // the safety net runs every 30 s and a steady graph must leave no line.
    // The control: each reason, once the pass changed the graph, has a line.
    #[test]
    fn test_repair_pass_line_of_a_pass_that_changed_nothing_is_none() {
        let now = std::time::Instant::now();
        let reasons = [
            audio::PassReason::SinkAppeared {
                name: JBL_SINK.to_string(),
                at: now,
            },
            audio::PassReason::SinkVanished {
                name: JBL_SINK.to_string(),
            },
            audio::PassReason::ConfirmationDue,
            audio::PassReason::SafetyNet,
            audio::PassReason::Reconnected,
        ];

        for reason in &reasons {
            assert_eq!(repair_pass_line(reason, false, now), None, "{reason:?}");
            assert!(
                repair_pass_line(reason, true, now).is_some(),
                "control: {reason:?} logs once the graph changed"
            );
        }
    }

    // Criterion: the line names what woke the pass, set off from the rest of
    // the sentence, and for a sink that appeared it carries the milliseconds
    // from the event to the end of the pass — #75's race was 149 ms.
    #[test]
    fn test_repair_pass_line_names_the_reason_and_the_ms_since_the_sink_appeared() {
        let at = std::time::Instant::now();
        let appeared = audio::PassReason::SinkAppeared {
            name: JBL_SINK.to_string(),
            at,
        };

        assert_eq!(
            repair_pass_line(&appeared, true, at + Duration::from_millis(149)).as_deref(),
            Some(
                "repair pass (woken by: sink bluez_output.2C_FD_B4_D3_AC_21.1 appeared) \
                 changed the graph 149 ms after the event"
            )
        );
        assert_eq!(
            repair_pass_line(&audio::PassReason::SafetyNet, true, at).as_deref(),
            Some("repair pass (woken by: the safety net) changed the graph")
        );
    }

    // ─── #139: the combined sink removed from outside ────────────────────────

    fn combined_sink_vanished() -> graph_pw::GraphEvent {
        graph_pw::GraphEvent::CombinedSinkVanished {
            name: COMBINED_SINK.to_string(),
            at: std::time::Instant::now(),
        }
    }

    fn combined_reason() -> audio::PassReason {
        audio::PassReason::CombinedSinkVanished {
            name: COMBINED_SINK.to_string(),
        }
    }

    /// Every reason but the combined sink's removal.
    fn other_reasons() -> Vec<audio::PassReason> {
        vec![
            audio::PassReason::SafetyNet,
            audio::PassReason::SinkVanished {
                name: JBL_SINK.to_string(),
            },
            audio::PassReason::ConfirmationDue,
            audio::PassReason::Reconnected,
            audio::PassReason::SinkAppeared {
                name: JBL_SINK.to_string(),
                at: std::time::Instant::now(),
            },
        ]
    }

    /// A state with the JBL selected and the tone playing — which is what gets
    /// a pass past its guard without a `librespot` — over `fake`, whose
    /// combined sink is gone: destroyed from outside. The JBL's sink is there
    /// when `speaker_up` is set.
    async fn vanished_state(fake: &graph::fake::FakeGraph, speaker_up: bool) -> AppState {
        if speaker_up {
            fake.add_sink(JBL_SINK);
        }
        let state = test_state_on(AudioEngine::new(), fake);
        state
            .targets
            .lock()
            .await
            .select(JBL, &[JBL.to_string()])
            .expect("a connected speaker can be selected");
        state
            .engine
            .lock()
            .await
            .play()
            .expect("the null output plays");
        state
    }

    /// How many times `fake` was asked to create the combined sink: one per
    /// pass that rebuilt it.
    fn rebuilds(fake: &graph::fake::FakeGraph) -> usize {
        fake.calls()
            .iter()
            .filter(|call| matches!(call, graph::fake::GraphCall::CreateCombinedSink { .. }))
            .count()
    }

    /// How many times `fake` was asked to re-target the streams.
    fn retargets(fake: &graph::fake::FakeGraph) -> usize {
        fake.calls()
            .iter()
            .filter(|call| matches!(call, graph::fake::GraphCall::RetargetStreams { .. }))
            .count()
    }

    /// The fallback pauses Spotify only: the tone, playing throughout, is
    /// never paused, and nothing is claimed while no `librespot` ran. Reusing
    /// `pause_sources_until_restored`, which pauses the engine too and claims
    /// its pause, fails here.
    async fn assert_tone_untouched_and_nothing_claimed(state: &AppState) {
        assert_eq!(
            state.engine.lock().await.poll_state().status,
            PlaybackStatus::Playing,
            "the fallback pauses Spotify, never the tone"
        );
        assert!(
            !state.backend_paused_sources.load(Ordering::SeqCst),
            "nothing was silenced, so nothing is claimed"
        );
    }

    // Criterion (#139): the pass line covers the combined sink's removal — it
    // names the reason when the pass changed the graph, and there is no line
    // when it did not.
    #[test]
    fn test_repair_pass_line_names_the_combined_sink_s_removal() {
        let now = std::time::Instant::now();

        let line = repair_pass_line(&combined_reason(), true, now);

        assert!(
            line.as_deref()
                .is_some_and(|line| line.starts_with("repair pass (woken by: ")
                    && line.contains(COMBINED_SINK)
                    && line.ends_with(") changed the graph")),
            "got {line:?}"
        );
        assert_eq!(repair_pass_line(&combined_reason(), false, now), None);
    }

    // Criterion (#139): a pass woken by the combined sink's removal that cannot
    // rebuild it — no selected speaker has a sink any more — falls back to
    // pausing Spotify. With no `librespot` running, the pause silenced
    // nothing: no claim, and the tone is not paused.
    #[tokio::test]
    async fn test_repair_pass_after_the_combined_sink_s_removal_falls_back_when_the_rebuild_fails()
    {
        let fake = graph::fake::FakeGraph::new();
        let state = vanished_state(&fake, false).await;

        let fell_back = branch_repair_pass(&state, combined_reason()).await;

        assert!(rebuilds(&fake) >= 1, "the pass tried: {:?}", fake.calls());
        assert!(fell_back, "the failed rebuild falls back to the pause");
        assert_tone_untouched_and_nothing_claimed(&state).await;
    }

    // Criterion (#139): a pass woken by the combined sink's removal that
    // rebuilt it but could not re-target the streams falls back to the pause
    // too, while the speakers' branches stay loaded.
    #[tokio::test]
    async fn test_repair_pass_after_the_combined_sink_s_removal_falls_back_when_the_retarget_fails()
    {
        let fake = graph::fake::FakeGraph::new();
        fake.fail(graph::fake::GraphOp::RetargetStreams);
        let state = vanished_state(&fake, true).await;

        let fell_back = branch_repair_pass(&state, combined_reason()).await;

        assert_eq!(retargets(&fake), 1, "calls: {:?}", fake.calls());
        assert_eq!(
            fake.loaded(COMBINED_SINK)
                .into_iter()
                .map(|b| b.branch.sink)
                .collect::<Vec<_>>(),
            vec![JBL_SINK.to_string()],
            "the route itself went through"
        );
        assert!(fell_back, "the failed re-target falls back to the pause");
        assert_tone_untouched_and_nothing_claimed(&state).await;
    }

    // Criterion (#139, guard, only on failure): a pass woken by the combined
    // sink's removal that rebuilt the sink and re-targeted the streams never
    // falls back. The near miss: the same reason, the same missing sink —
    // only the successful rebuild and re-target spare it the pause.
    #[tokio::test]
    async fn test_repair_pass_after_the_combined_sink_s_removal_that_rebuilt_never_falls_back() {
        let fake = graph::fake::FakeGraph::new();
        let state = vanished_state(&fake, true).await;

        let fell_back = branch_repair_pass(&state, combined_reason()).await;

        assert_eq!(rebuilds(&fake), 1, "calls: {:?}", fake.calls());
        assert_eq!(retargets(&fake), 1, "calls: {:?}", fake.calls());
        assert!(!fell_back, "a successful rebuild never pauses");
        assert_tone_untouched_and_nothing_claimed(&state).await;
    }

    // Criterion (#139, guard, only this reason pauses): a pass woken by any
    // other reason never falls back, even when its route fails or its
    // re-targeting does. The control: the combined sink's removal on the
    // same failing graph does.
    #[tokio::test]
    async fn test_repair_pass_woken_by_another_reason_never_falls_back() {
        let fake = graph::fake::FakeGraph::new();
        let state = vanished_state(&fake, false).await;
        assert!(
            branch_repair_pass(&state, combined_reason()).await,
            "control: the combined sink's removal falls back on this graph"
        );

        for reason in other_reasons() {
            // The route fails: no speaker sink.
            let fake = graph::fake::FakeGraph::new();
            let state = vanished_state(&fake, false).await;
            let label = format!("{reason:?}");
            assert!(
                !branch_repair_pass(&state, reason.clone()).await,
                "{label} with a failed route"
            );
            assert!(rebuilds(&fake) >= 1, "{label}: the pass ran");
            assert_tone_untouched_and_nothing_claimed(&state).await;

            // The route goes through, the re-targeting fails.
            let fake = graph::fake::FakeGraph::new();
            fake.fail(graph::fake::GraphOp::RetargetStreams);
            let state = vanished_state(&fake, true).await;
            assert!(
                !branch_repair_pass(&state, reason).await,
                "{label} with a failed re-target"
            );
            assert_tone_untouched_and_nothing_claimed(&state).await;
        }
    }

    // Criterion (#139): a fallback pause that silenced nothing claims nothing.
    // Here `librespot` runs — a real subprocess stands in for it, which is also
    // what gets the pass past its guard with the tone stopped — and the pause
    // request fails, the Web API being out of reach without a login. The pass
    // falls back, the request fails, and no claim is left for a restore path
    // to resume on.
    #[tokio::test]
    async fn test_fallback_pause_that_silenced_nothing_claims_nothing() {
        let fake = graph::fake::FakeGraph::new();
        let state = test_state_on(AudioEngine::new(), &fake);
        state
            .targets
            .lock()
            .await
            .select(JBL, &[JBL.to_string()])
            .expect("a connected speaker can be selected");
        let child =
            spotify::spawn_bound_to_this_thread("sleep", &["30".to_string()]).expect("spawn sleep");
        state.spotify.lock().await.adopt_child_for_test(child);
        assert_eq!(
            state.spotify.lock().await.status().status,
            blue2th_proto::SpotifyStatus::Running,
            "the fixture must start from a running librespot"
        );

        let fell_back = branch_repair_pass(&state, combined_reason()).await;

        let stopped = state.spotify.lock().await.stop();
        assert!(stopped.is_ok(), "the stand-in is stopped: {stopped:?}");
        assert!(fell_back, "the failed rebuild falls back to the pause");
        assert!(
            !state.backend_paused_sources.load(Ordering::SeqCst),
            "a pause that failed claims nothing"
        );
    }

    // Criterion (#139): the event consumer wakes a pass for the combined
    // sink's removal, and that pass rebuilds the sink and re-targets the
    // streams at once, without waiting for the safety net.
    #[tokio::test]
    async fn test_event_consumer_rebuilds_after_the_combined_sink_s_removal() {
        let fake = graph::fake::FakeGraph::new();
        let state = vanished_state(&fake, true).await;

        consume(&state, vec![combined_sink_vanished()]).await;

        assert_eq!(rebuilds(&fake), 1, "calls: {:?}", fake.calls());
        assert_eq!(retargets(&fake), 1, "calls: {:?}", fake.calls());
    }

    // Criterion (#139): a burst carrying a selected speaker's sink appearing
    // first and the combined sink's removal after it runs one pass, not two.
    // Counting rebuilds alone cannot tell: a second pass finds the sink
    // standing and only reconciles. So the whole call log, reads included, is
    // the log of one pass run on its own over the same graph — and that one
    // pass rebuilt once. The reason it carries is pinned by
    // `test_drain_burst_reason_names_the_combined_sink_s_removal_and_drains_the_queue`.
    #[tokio::test]
    async fn test_event_consumer_runs_one_pass_for_a_burst_with_the_combined_sink_s_removal() {
        let fake = graph::fake::FakeGraph::new();
        let state = vanished_state(&fake, true).await;
        let one_pass = graph::fake::FakeGraph::new();
        let alone = vanished_state(&one_pass, true).await;
        branch_repair_pass(&alone, combined_reason()).await;
        assert_eq!(rebuilds(&one_pass), 1, "calls: {:?}", one_pass.calls());

        consume(
            &state,
            vec![sink_appeared(JBL_SINK), combined_sink_vanished()],
        )
        .await;

        assert_eq!(fake.all_calls(), one_pass.all_calls());
        assert_eq!(retargets(&fake), 1, "calls: {:?}", fake.calls());
    }

    // Criterion (#139, guard, the reason is not lost in a burst): the drain
    // the event consumer runs names its one pass after the combined sink's
    // removal even when a selected speaker's sink appearing was queued first —
    // the first-wins fold it replaced would name that one — and takes every
    // event already queued, so no second pass follows for the same burst.
    #[test]
    fn test_drain_burst_reason_names_the_combined_sink_s_removal_and_drains_the_queue() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        sender
            .send(combined_sink_vanished())
            .expect("the receiver is alive");
        sender
            .send(sink_appeared(JBL_SINK))
            .expect("the receiver is alive");
        let speakers = [blue2th_proto::SpeakerTarget {
            address: JBL.to_string(),
            offset_ms: 0,
        }];

        let reason = drain_burst_reason(sink_appeared(JBL_SINK), &mut receiver, &speakers);

        assert_eq!(reason, Some(combined_reason()));
        assert!(receiver.try_recv().is_err(), "the burst was drained whole");
    }

    // Criterion (#139): a fallback pause that silenced a playing `librespot`
    // is claimed, so a restore path may resume it; one that silenced nothing
    // claims nothing — and leaves standing a claim an earlier pause made,
    // which is still owed its resume.
    #[tokio::test]
    async fn test_claim_fallback_pause_claims_only_what_it_silenced_and_clears_nothing() {
        let fake = graph::fake::FakeGraph::new();
        let state = test_state_on(AudioEngine::new(), &fake);

        claim_fallback_pause(&state, false);
        assert!(
            !state.backend_paused_sources.load(Ordering::SeqCst),
            "a pause that silenced nothing claims nothing"
        );

        claim_fallback_pause(&state, true);
        assert!(
            state.backend_paused_sources.load(Ordering::SeqCst),
            "a pause that silenced librespot is claimed"
        );

        claim_fallback_pause(&state, false);
        assert!(
            state.backend_paused_sources.load(Ordering::SeqCst),
            "an earlier claim survives a pause that silenced nothing"
        );
    }

    // Criterion (#139): with nothing selected, the combined sink's removal
    // wakes nothing — the graph is not even read. The control: once the JBL
    // is selected, the same event rebuilds.
    #[tokio::test]
    async fn test_event_consumer_ignores_the_combined_sink_s_removal_with_nothing_selected() {
        let fake = graph::fake::FakeGraph::with_sinks(&[JBL_SINK]);
        let state = test_state_on(AudioEngine::new(), &fake);
        state
            .engine
            .lock()
            .await
            .play()
            .expect("the null output plays");

        consume(&state, vec![combined_sink_vanished()]).await;
        assert!(fake.all_calls().is_empty(), "calls: {:?}", fake.all_calls());

        state
            .targets
            .lock()
            .await
            .select(JBL, &[JBL.to_string()])
            .expect("a connected speaker can be selected");
        consume(&state, vec![combined_sink_vanished()]).await;
        assert_eq!(rebuilds(&fake), 1, "control: a selection rebuilds");
    }

    /// Let every task that is ready run, without moving the paused clock.
    async fn settle() {
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
    }

    // Criterion: the confirmation timer sleeps until the router's earliest
    // confirming reload falls due, then runs one pass that reloads that
    // branch alone — one unload, one load — without waiting for the 30 s
    // safety net. Not a moment before the gap, and only once. Halfway through
    // the gap no pass has even read the graph: the timer sleeps rather than
    // polling, which the register alone would hide, since an early pass
    // reloads nothing. Driven on tokio's paused clock, which the router's
    // clock follows here.
    #[tokio::test(start_paused = true)]
    async fn test_confirmation_timer_wakes_a_pass_when_the_confirmation_falls_due() {
        use graph::fake::{FakeGraph, GraphCall};

        let fake = FakeGraph::with_sinks(&[JBL_SINK, COMBINED_SINK]);
        let state = timed_state(&fake).await;
        let load = jbl_load();

        let loaded = load_the_jbl(&state, &fake).await;

        spawn_confirmation_timer(state.clone());
        settle().await;
        tokio::time::advance(audio::CONFIRM_GAP / 2).await;
        settle().await;
        assert_eq!(
            fake.all_calls(),
            Vec::<GraphCall>::new(),
            "the timer sleeps: no pass reads the graph halfway through the gap"
        );
        tokio::time::advance(audio::CONFIRM_GAP / 2 - Duration::from_millis(1)).await;
        settle().await;
        assert_eq!(
            fake.calls(),
            Vec::<GraphCall>::new(),
            "no reload before the gap"
        );

        tokio::time::advance(Duration::from_millis(1)).await;
        settle().await;
        assert_eq!(
            fake.calls(),
            vec![GraphCall::UnloadBranch { id: loaded }, load],
            "the confirming reload, of that branch alone"
        );

        fake.clear_calls();
        tokio::time::advance(audio::CONFIRM_GAP * 2).await;
        settle().await;
        assert_eq!(
            fake.calls(),
            Vec::<GraphCall>::new(),
            "only once: the reload does not arm itself"
        );
    }

    // Criterion: a timer with nothing armed waits for an arming rather than
    // for the safety net. Here it is started first, on an empty register; the
    // load arms a reload while it waits, and that reload runs one gap later,
    // not a moment before. Without the router's arming notification the timer
    // would sleep on until the process ends.
    #[tokio::test(start_paused = true)]
    async fn test_confirmation_timer_wakes_for_a_reload_armed_while_it_waits() {
        use graph::fake::{FakeGraph, GraphCall};

        let fake = FakeGraph::with_sinks(&[JBL_SINK, COMBINED_SINK]);
        let state = timed_state(&fake).await;
        spawn_confirmation_timer(state.clone());
        settle().await;

        let loaded = load_the_jbl(&state, &fake).await;
        settle().await;
        tokio::time::advance(audio::CONFIRM_GAP - Duration::from_millis(1)).await;
        settle().await;
        assert_eq!(
            fake.all_calls(),
            Vec::<GraphCall>::new(),
            "not before the gap"
        );

        tokio::time::advance(Duration::from_millis(1)).await;
        settle().await;
        assert_eq!(
            fake.calls(),
            vec![GraphCall::UnloadBranch { id: loaded }, jbl_load()]
        );
    }

    // Criterion: a reload that falls due while nothing plays is neither
    // dropped nor retried in a loop. The pass it wakes is guarded out, so the
    // reload stays armed; the timer then waits one more gap. Playback resumes
    // a second after the due time: a timer spinning on the past due time
    // would reload at once, this one reloads when the gap is over. The due
    // time is read off what the actor published (#147), never asked for.
    #[tokio::test(start_paused = true)]
    async fn test_confirmation_timer_keeps_a_reload_due_while_nothing_plays_without_spinning() {
        use graph::fake::{FakeGraph, GraphCall};

        let fake = FakeGraph::with_sinks(&[JBL_SINK, COMBINED_SINK]);
        let state = timed_state(&fake).await;
        let loaded = load_the_jbl(&state, &fake).await;
        let published = state.router.confirmation_due();
        let due = *published.borrow();
        assert!(due.is_some(), "the load armed a reload");
        spawn_confirmation_timer(state.clone());
        settle().await;
        state
            .engine
            .lock()
            .await
            .stop()
            .expect("the null output stops");

        tokio::time::advance(audio::CONFIRM_GAP).await;
        settle().await;
        assert_eq!(fake.all_calls(), Vec::<GraphCall>::new(), "nothing plays");
        assert_eq!(*published.borrow(), due, "the reload is still armed");

        state
            .engine
            .lock()
            .await
            .play()
            .expect("the null output plays");
        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(
            fake.all_calls(),
            Vec::<GraphCall>::new(),
            "the timer waits for the gap, it does not spin"
        );
        // A poll in the middle of the wait is a message like any other: the
        // actor publishes the same due time after it, and a due time left in
        // place is not a new arming — the timer runs no pass for it.
        let Json(polled) = playback(State(state.clone())).await;
        settle().await;
        assert_eq!(polled.audio_graph, AudioGraphStatus::Responsive);
        assert_eq!(passes(&fake), 0, "calls: {:?}", fake.all_calls());
        assert_eq!(fake.calls(), Vec::<GraphCall>::new());
        fake.clear_calls();

        tokio::time::advance(audio::CONFIRM_GAP - Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(
            fake.calls(),
            vec![GraphCall::UnloadBranch { id: loaded }, jbl_load()],
            "the reload left due runs one gap later"
        );
        assert_eq!(*published.borrow(), None);
    }

    // ─── #145, #147: bounded waits for the actor, and a stalled graph ───────
    //
    // A graph thread that is stuck is simulated by the test holding the fake
    // actor (`RouterHandle::hold_actor`): it starts no message meanwhile, and
    // what the handlers send queues behind the hold. The paused clock lets
    // the bounds elapse instantly. A request "answers at once" when it has
    // finished after `settle()` with the clock not moved.

    /// The WH-1000XM5 whose sink is `SONY_SINK`.
    const SONY: &str = "80:99:E7:63:50:29";

    /// The level the engine was last told. Distinct from every live sink
    /// level these tests seed, so a reply tells which one it carries.
    const COMMANDED: f32 = 0.35;

    /// The message a graph that did nothing for a request answers with, as
    /// the spec quotes it.
    const GRAPH_NOT_ANSWERING: &str = "the audio graph is not answering";

    /// A state over `fake` with `connected` connected and `selected` selected,
    /// in order, and the engine's commanded level at [`COMMANDED`]. The calls
    /// made while setting it up are forgotten.
    async fn selected_state(
        fake: &graph::fake::FakeGraph,
        connected: &[&str],
        selected: &[&str],
    ) -> AppState {
        let state = test_state_on(AudioEngine::new(), fake);
        let connected: Vec<String> = connected.iter().map(|mac| mac.to_string()).collect();
        // Cloned: the cache owns one copy, the selection is validated against
        // the other.
        *state.connected.lock().await = connected.clone();
        for mac in selected {
            state
                .targets
                .lock()
                .await
                .select(mac, &connected)
                .expect("a connected speaker can be selected");
        }
        state
            .engine
            .lock()
            .await
            .set_volume(COMMANDED)
            .expect("the null output takes a level");
        fake.clear_calls();
        state
    }

    /// Start the background routing applier on `state` and forget whatever
    /// it did on its own before any change was asked of it.
    async fn start_applier(state: &AppState, fake: &graph::fake::FakeGraph) {
        spawn_routing_applier(state.clone());
        settle().await;
        fake.clear_calls();
    }

    /// The status and message of a handler's failure; `None` for a success.
    fn failure<T>(answer: &Result<T, AppError>) -> Option<(StatusCode, String)> {
        answer
            .as_ref()
            .err()
            // Cloned out of the borrowed error, so the test owns what it reads.
            .map(|e| (e.status, e.message.clone()))
    }

    /// The selected addresses a `TargetsState` reply carries, in order.
    fn addresses(targets: &TargetsState) -> Vec<String> {
        targets.speakers.iter().map(|s| s.address.clone()).collect()
    }

    /// The offset a `TargetsState` reply carries for `mac`.
    fn offset_of(targets: &TargetsState, mac: &str) -> Option<u32> {
        targets
            .speakers
            .iter()
            .find(|s| s.address == mac)
            .map(|s| s.offset_ms)
    }

    /// How many times `fake` was asked for its sink list.
    fn sink_list_reads(fake: &graph::fake::FakeGraph) -> usize {
        fake.all_calls()
            .iter()
            .filter(|call| matches!(call, graph::fake::GraphCall::Sinks))
            .count()
    }

    /// The messages a scripted router was sent, in order: each one described,
    /// with the `start_by` it carried.
    type Received = Arc<std::sync::Mutex<Vec<(String, Option<std::time::Instant>)>>>;

    /// A router that records every message it is sent and never answers one:
    /// a graph thread that is there and stuck.
    fn recording_router() -> (RouterHandle, Received) {
        let received: Received = Arc::default();
        let log = Arc::clone(&received);
        let mut unanswered = Vec::new();
        let router = RouterHandle::over(
            Box::new(move |envelope: router_actor::Envelope| {
                log.lock().unwrap().push((
                    router_actor::testing::describe(&envelope.message),
                    envelope.start_by,
                ));
                // Kept alive, unanswered: its caller goes on waiting.
                unanswered.push(envelope);
                Ok(())
            }),
            router_actor::Shared::new(),
        );
        (router, received)
    }

    /// A router that records every message it is sent and answers the first
    /// repair with `outcome`. Any other message is dropped unanswered.
    fn router_answering_a_repair_with(
        outcome: router_actor::RepairOutcome,
    ) -> (RouterHandle, Received) {
        let received: Received = Arc::default();
        let log = Arc::clone(&received);
        let mut outcome = Some(outcome);
        let router = RouterHandle::over(
            Box::new(move |envelope: router_actor::Envelope| {
                log.lock().unwrap().push((
                    router_actor::testing::describe(&envelope.message),
                    envelope.start_by,
                ));
                if let router_actor::Message::Repair { reply, .. } = envelope.message {
                    if let Some(outcome) = outcome.take() {
                        let _ = reply.send(Ok(outcome));
                    }
                }
                Ok(())
            }),
            router_actor::Shared::new(),
        );
        (router, received)
    }

    // Criterion (#145): `AppError` has a 503 constructor, carrying its
    // message as given.
    #[test]
    fn test_app_error_service_unavailable_answers_503_with_its_message() {
        let error = AppError::service_unavailable("the graph is frozen");

        assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(error.message, "the graph is frozen");
    }

    // Criterion (#145): the router timeout maps to 503 naming the audio graph;
    // a graph failure a message ran into keeps mapping as an `AudioError`
    // does (500, its message kept) — the two must not merge.
    #[test]
    fn test_router_timeout_maps_to_503_and_a_graph_failure_stays_500() {
        let timed_out = AppError::from(RouterError::TimedOut);
        let failed = AppError::from(RouterError::Audio(AudioError::PipeWire(
            "sinks unreadable".to_string(),
        )));

        assert_eq!(timed_out.status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            timed_out.message.contains(GRAPH_NOT_ANSWERING),
            "got {:?}",
            timed_out.message
        );
        assert_eq!(failed.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            failed.message.contains("sinks unreadable"),
            "got {:?}",
            failed.message
        );
    }

    // Criterion (#146): a message that expired maps to 503 with the router
    // timeout's own message, whether it reaches the handler bare or through
    // `RouterError::Audio` — nothing was done to the graph in either case.
    // The message is compared whole: the near miss is the 503 carrying the
    // error's own `Display`, which also opens with "the audio graph".
    #[test]
    fn test_an_expired_message_maps_to_503_with_the_router_timeout_s_message() {
        let bare = AppError::from(AudioError::Expired);
        let through_router = AppError::from(RouterError::Audio(AudioError::Expired));

        assert_eq!(bare.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(bare.message, "the audio graph is not answering");
        assert_eq!(through_router.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(through_router.message, "the audio graph is not answering");
        assert_eq!(
            bare.message,
            AppError::from(RouterError::TimedOut).message,
            "the same answer as a router that did not answer in time"
        );
    }

    // Criterion (#146, guard): only an expiry is a 503 — a graph failure that
    // reaches the handler bare stays a 500 carrying its own message. The near
    // miss is this very `PipeWire` error: an `Expired` arm widened to every
    // `AudioError` the graph raises would answer it 503 too.
    #[test]
    fn test_a_bare_graph_failure_stays_500_beside_an_expired_message() {
        let failed = AppError::from(AudioError::PipeWire("sinks unreadable".to_string()));
        let expired = AppError::from(AudioError::Expired);

        assert_eq!(failed.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            failed.message.contains("sinks unreadable"),
            "got {:?}",
            failed.message
        );
        assert_eq!(
            expired.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "control: the expiry beside it is the 503"
        );
    }

    // Criteria (#145, #147): a request-path wait expires after
    // `REQUEST_BOUND` — not a millisecond before — and `POST /volume` then
    // answers 503 naming the audio graph, with nothing sent to the graph and
    // the level not applied. Guard (sends nothing on a 503): the graph log
    // is read *after* the actor is released. The near miss is a message
    // that outlives its caller — sent, then run once the actor frees —
    // which answers the same 503 and then writes the level.
    #[tokio::test(start_paused = true)]
    async fn test_volume_answers_503_when_the_actor_is_held_past_the_request_bound() {
        use graph::fake::{FakeGraph, GraphCall};

        let fake = FakeGraph::with_sinks(&[JBL_SINK]);
        let state = selected_state(&fake, &[JBL], &[JBL]).await;
        let held = state.router.hold_actor();

        let request = tokio::spawn(volume(
            State(state.clone()),
            Json(VolumeRequest { level: 0.8 }),
        ));
        settle().await;
        tokio::time::advance(router_handle::REQUEST_BOUND - Duration::from_millis(1)).await;
        settle().await;
        assert!(!request.is_finished(), "the request waits the whole bound");
        tokio::time::advance(Duration::from_millis(1)).await;
        settle().await;
        assert!(request.is_finished(), "the request gives up at the bound");
        let answer = request.await.expect("the handler task ends");
        drop(held);
        settle().await;

        let failed = failure(&answer);
        assert_eq!(
            failed.as_ref().map(|(status, _)| *status),
            Some(StatusCode::SERVICE_UNAVAILABLE)
        );
        assert!(
            failed
                .as_ref()
                .is_some_and(|(_, message)| message.contains(GRAPH_NOT_ANSWERING)),
            "got {failed:?}"
        );
        assert_eq!(
            fake.all_calls(),
            Vec::<GraphCall>::new(),
            "nothing reached the graph, before or after the release"
        );
        assert_eq!(state.engine.lock().await.poll_state().volume, COMMANDED);
    }

    // Criterion (#146, kept): an actor freed within the start budget — here
    // at the very instant it runs out, 300 ms after the request — starts the
    // message, and the request proceeds: no 503, the level written to the
    // speaker's sink and into the engine.
    #[tokio::test(start_paused = true)]
    async fn test_volume_proceeds_when_the_actor_frees_within_the_start_budget() {
        use graph::fake::{FakeGraph, GraphCall};

        let fake = FakeGraph::with_sinks(&[JBL_SINK]);
        let state = selected_state(&fake, &[JBL], &[JBL]).await;
        let held = state.router.hold_actor();

        let request = tokio::spawn(volume(
            State(state.clone()),
            Json(VolumeRequest { level: 0.8 }),
        ));
        settle().await;
        tokio::time::advance(Duration::from_millis(300)).await;
        settle().await;
        assert!(!request.is_finished(), "the request waits behind the hold");
        drop(held);
        settle().await;
        assert!(request.is_finished(), "the freed actor runs it at once");
        let answer = request.await.expect("the handler task ends");

        assert_eq!(failure(&answer).map(|(status, _)| status), None);
        assert_eq!(
            fake.calls(),
            vec![GraphCall::SetSinkVolume {
                sink: JBL_SINK.to_string(),
                level: 0.8
            }]
        );
        assert_eq!(state.engine.lock().await.poll_state().volume, 0.8);
    }

    // Criterion (#146, kept; non-nominal): the graph thread is busy longer
    // than the start budget when the request's message reaches the head of
    // the queue — the actor frees 301 ms after the request. The message is
    // not run, and the request answers 503 right then, not at the 2 s
    // bound; the graph receives nothing and the level is not applied.
    #[tokio::test(start_paused = true)]
    async fn test_volume_answers_503_at_once_when_the_actor_frees_past_the_start_budget() {
        use graph::fake::{FakeGraph, GraphCall};

        let fake = FakeGraph::with_sinks(&[JBL_SINK]);
        let state = selected_state(&fake, &[JBL], &[JBL]).await;
        let held = state.router.hold_actor();

        let request = tokio::spawn(volume(
            State(state.clone()),
            Json(VolumeRequest { level: 0.8 }),
        ));
        settle().await;
        tokio::time::advance(Duration::from_millis(301)).await;
        settle().await;
        assert!(!request.is_finished(), "the request waits behind the hold");
        drop(held);
        settle().await;
        assert!(request.is_finished(), "the expiry is answered at once");
        let answer = request.await.expect("the handler task ends");

        let failed = failure(&answer);
        assert_eq!(
            failed.as_ref().map(|(status, _)| *status),
            Some(StatusCode::SERVICE_UNAVAILABLE)
        );
        assert!(
            failed
                .as_ref()
                .is_some_and(|(_, message)| message.contains(GRAPH_NOT_ANSWERING)),
            "got {failed:?}"
        );
        assert_eq!(fake.all_calls(), Vec::<GraphCall>::new());
        assert_eq!(state.engine.lock().await.poll_state().volume, COMMANDED);
    }

    // Criterion (#147): a volume set queued behind another one for the same
    // sinks is superseded — `POST /volume` then answers 200 with the current
    // playback state and does not write its level into the engine; the
    // winning request does. The graph receives the winner's level alone,
    // and the superseded request's reply carries a level other than its own:
    // the one commanded before, or the winner's if that landed first.
    #[tokio::test(start_paused = true)]
    async fn test_volume_whose_set_was_superseded_answers_200_and_leaves_the_engine_to_the_winner()
    {
        use graph::fake::{FakeGraph, GraphCall};

        let fake = FakeGraph::with_sinks(&[JBL_SINK]);
        let state = selected_state(&fake, &[JBL], &[JBL]).await;
        let held = state.router.hold_actor();

        let superseded = tokio::spawn(volume(
            State(state.clone()),
            Json(VolumeRequest { level: 0.3 }),
        ));
        settle().await;
        let winner = tokio::spawn(volume(
            State(state.clone()),
            Json(VolumeRequest { level: 0.5 }),
        ));
        settle().await;
        tokio::time::advance(Duration::from_millis(100)).await;
        settle().await;
        assert!(
            !superseded.is_finished() && !winner.is_finished(),
            "both requests wait behind the hold"
        );
        drop(held);
        settle().await;

        assert!(superseded.is_finished() && winner.is_finished());
        let superseded = superseded.await.expect("the handler task ends");
        let winner = winner.await.expect("the handler task ends");
        assert_eq!(
            failure(&superseded),
            None,
            "a superseded set is not a failure"
        );
        let carried = superseded.ok().map(|Json(reply)| reply.volume);
        assert!(
            carried == Some(COMMANDED) || carried == Some(0.5),
            "the reply carries the current level, never the superseded 0.3: {carried:?}"
        );
        assert_eq!(winner.ok().map(|Json(reply)| reply.volume), Some(0.5));
        assert_eq!(
            fake.calls(),
            vec![GraphCall::SetSinkVolume {
                sink: JBL_SINK.to_string(),
                level: 0.5
            }],
            "the 0.3 was never written"
        );
        assert_eq!(state.engine.lock().await.poll_state().volume, 0.5);
    }

    // Criterion (#147): a waiting request suspends — it holds nothing and
    // blocks no thread. On a single-threaded runtime, with `POST /volume`
    // waiting behind a held actor, an unrelated handler still answers at
    // once.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_an_unrelated_handler_answers_at_once_while_a_request_waits_behind_a_held_actor() {
        let fake = graph::fake::FakeGraph::with_sinks(&[JBL_SINK]);
        let state = selected_state(&fake, &[JBL], &[JBL]).await;
        let held = state.router.hold_actor();
        let before = tokio::time::Instant::now();

        let waiting = tokio::spawn(volume(
            State(state.clone()),
            Json(VolumeRequest { level: 0.8 }),
        ));
        settle().await;
        let unrelated = tokio::spawn(get_targets(State(state.clone())));
        settle().await;

        assert!(unrelated.is_finished(), "`GET /targets` answers at once");
        let Json(targets) = unrelated.await.expect("the handler task ends");
        assert_eq!(addresses(&targets), vec![JBL.to_string()]);
        assert!(
            !waiting.is_finished(),
            "the volume request is still waiting"
        );
        assert_eq!(
            tokio::time::Instant::now(),
            before,
            "the clock did not move"
        );
        drop(held);
    }

    // Criterion (#145, guard, sends nothing on a 503): `POST /play` behind a
    // held actor answers 503, the graph is never asked anything — read
    // after the release — and the tone does not start.
    #[tokio::test(start_paused = true)]
    async fn test_play_answers_503_when_the_actor_is_held_and_leaves_the_engine_not_playing() {
        use graph::fake::{FakeGraph, GraphCall};

        let fake = FakeGraph::with_sinks(&[JBL_SINK]);
        let state = selected_state(&fake, &[JBL], &[JBL]).await;
        let held = state.router.hold_actor();

        let request = tokio::spawn(play(State(state.clone())));
        settle().await;
        assert!(!request.is_finished(), "the request waits behind the hold");
        tokio::time::advance(router_handle::REQUEST_BOUND).await;
        settle().await;
        assert!(request.is_finished(), "the request gives up at the bound");
        let answer = request.await.expect("the handler task ends");
        drop(held);
        settle().await;

        let failed = failure(&answer);
        assert_eq!(
            failed.as_ref().map(|(status, _)| *status),
            Some(StatusCode::SERVICE_UNAVAILABLE)
        );
        assert!(
            failed
                .as_ref()
                .is_some_and(|(_, message)| message.contains(GRAPH_NOT_ANSWERING)),
            "got {failed:?}"
        );
        assert_eq!(fake.all_calls(), Vec::<GraphCall>::new());
        assert_ne!(
            state.engine.lock().await.poll_state().status,
            PlaybackStatus::Playing,
            "a 503 starts no tone"
        );
    }

    // Criterion (#147, guard, the empty selection): `POST /play` with nothing
    // selected is refused 400, as it is today, and sends the actor no
    // message — `tests/transport.rs` pins the same 4xx over a detached
    // graph, where a message would err "not running" before any router saw
    // the empty selection. The router here records what it is sent and
    // never answers: a play that sent its route first would give up 503.
    // The control: with the JBL selected, the same play sends its one route.
    #[tokio::test(start_paused = true)]
    async fn test_play_with_nothing_selected_answers_400_and_sends_the_actor_no_message() {
        let fake = graph::fake::FakeGraph::with_sinks(&[JBL_SINK]);
        let mut state = selected_state(&fake, &[JBL], &[]).await;
        let (router, received) = recording_router();
        state.router = router;

        let refused = play(State(state.clone())).await;

        assert_eq!(
            failure(&refused).map(|(status, _)| status),
            Some(StatusCode::BAD_REQUEST)
        );
        assert_eq!(*received.lock().unwrap(), Vec::new());
        assert_ne!(
            state.engine.lock().await.poll_state().status,
            PlaybackStatus::Playing
        );

        let connected = vec![JBL.to_string()];
        state
            .targets
            .lock()
            .await
            .select(JBL, &connected)
            .expect("a connected speaker can be selected");
        let before = tokio::time::Instant::now().into_std();
        let playing = tokio::spawn(play(State(state.clone())));
        settle().await;
        assert_eq!(
            *received.lock().unwrap(),
            vec![(
                format!("Route [{JBL}@0]"),
                Some(before + Duration::from_millis(300))
            )],
            "control: a play with a selection sends its one route, as a request"
        );
        playing.abort();
    }

    // Criterion (#145, guard, sends nothing on a 503): `POST /spotify/start`
    // behind a held actor answers 503 — a routing message that timed out —
    // routes nothing, and the backend stays stopped: `librespot` is spawned
    // only after the routing message answered.
    #[tokio::test(start_paused = true)]
    async fn test_spotify_start_answers_503_when_the_actor_is_held_and_spawns_nothing() {
        use graph::fake::{FakeGraph, GraphCall};

        let fake = FakeGraph::with_sinks(&[JBL_SINK]);
        let state = selected_state(&fake, &[JBL], &[JBL]).await;
        let held = state.router.hold_actor();

        let request = tokio::spawn(spotify_start(State(state.clone())));
        settle().await;
        assert!(!request.is_finished(), "the request waits behind the hold");
        tokio::time::advance(router_handle::REQUEST_BOUND).await;
        settle().await;
        assert!(request.is_finished(), "the request gives up at the bound");
        let answer = request.await.expect("the handler task ends");
        drop(held);
        settle().await;

        let failed = failure(&answer);
        assert_eq!(
            failed.as_ref().map(|(status, _)| *status),
            Some(StatusCode::SERVICE_UNAVAILABLE)
        );
        assert!(
            failed
                .as_ref()
                .is_some_and(|(_, message)| message.contains(GRAPH_NOT_ANSWERING)),
            "got {failed:?}"
        );
        assert_eq!(fake.all_calls(), Vec::<GraphCall>::new());
        assert_eq!(
            state.spotify.lock().await.poll_liveness().status,
            SpotifyStatus::Stopped
        );
    }

    // Criterion (#147): `POST /spotify/start` whose routing message expired
    // — the actor frees 301 ms after the request — answers 503 right then:
    // nothing was done, so it is not the spawn failure a routing that ran
    // and failed is. Nothing reaches the graph and nothing is spawned.
    #[tokio::test(start_paused = true)]
    async fn test_spotify_start_whose_routing_expired_answers_503_at_once_and_spawns_nothing() {
        use graph::fake::{FakeGraph, GraphCall};

        let fake = FakeGraph::with_sinks(&[JBL_SINK]);
        let state = selected_state(&fake, &[JBL], &[JBL]).await;
        let held = state.router.hold_actor();

        let request = tokio::spawn(spotify_start(State(state.clone())));
        settle().await;
        tokio::time::advance(Duration::from_millis(301)).await;
        settle().await;
        assert!(!request.is_finished(), "the request waits behind the hold");
        drop(held);
        settle().await;
        assert!(request.is_finished(), "the expiry is answered at once");
        let answer = request.await.expect("the handler task ends");

        let failed = failure(&answer);
        assert_eq!(
            failed.as_ref().map(|(status, _)| *status),
            Some(StatusCode::SERVICE_UNAVAILABLE)
        );
        assert!(
            failed
                .as_ref()
                .is_some_and(|(_, message)| message.contains(GRAPH_NOT_ANSWERING)),
            "got {failed:?}"
        );
        assert_eq!(fake.all_calls(), Vec::<GraphCall>::new());
        assert_eq!(
            state.spotify.lock().await.poll_liveness().status,
            SpotifyStatus::Stopped
        );
    }

    // Criterion (#147, guard, only "nothing was done" is a 503): a routing
    // that ran and failed keeps today's mapping — `SpotifyError::Spawn`, a
    // 500 naming the spawn and carrying the graph's failure. The near miss
    // is this graph failure answered as the 503 of an expiry. Nothing is
    // spawned: the route failed before any node name was resolved.
    #[tokio::test]
    async fn test_spotify_start_whose_routing_ran_and_failed_keeps_the_spawn_failure_mapping() {
        let fake = graph::fake::FakeGraph::with_sinks(&[JBL_SINK]);
        fake.fail(graph::fake::GraphOp::CreateCombinedSink);
        let state = selected_state(&fake, &[JBL], &[JBL]).await;

        let answer = spotify_start(State(state.clone())).await;

        let failed = failure(&answer);
        assert_eq!(
            failed.as_ref().map(|(status, _)| *status),
            Some(StatusCode::INTERNAL_SERVER_ERROR)
        );
        assert!(
            failed.as_ref().is_some_and(|(_, message)| {
                message.contains("failed to spawn Spotify backend")
                    && message.contains("CreateCombinedSink told to fail")
            }),
            "got {failed:?}"
        );
        assert_eq!(
            state.spotify.lock().await.poll_liveness().status,
            SpotifyStatus::Stopped
        );
    }

    // Criterion (#147): `librespot` is spawned only after the routing message
    // answered the resolved sink name. Over a free actor a start routes the
    // graph, then resolves the target — the sink list is read once more
    // after the last branch load — and only then reaches the spawn, which
    // under test finds no program: 500 "librespot not found", the backend
    // left stopped. A start that stopped after its routing would answer
    // something else than the spawn's own failure. The argv the spawn hands
    // a real `librespot` is left to the manual verification.
    #[tokio::test(start_paused = true)]
    async fn test_spotify_start_routes_and_resolves_the_sink_before_it_reaches_the_spawn() {
        use graph::fake::{FakeGraph, GraphCall};

        let fake = FakeGraph::with_sinks(&[JBL_SINK]);
        let state = selected_state(&fake, &[JBL], &[JBL]).await;

        let answer = spotify_start(State(state.clone())).await;

        let failed = failure(&answer);
        assert_eq!(
            failed.as_ref().map(|(status, _)| *status),
            Some(StatusCode::INTERNAL_SERVER_ERROR)
        );
        assert!(
            failed
                .as_ref()
                .is_some_and(|(_, message)| message.contains("librespot not found")),
            "got {failed:?}"
        );
        assert_eq!(
            fake.routing_calls(),
            vec![
                GraphCall::ClearStaleDefaultSink {
                    sink_name: COMBINED_SINK.to_string()
                },
                GraphCall::Teardown {
                    sink_name: COMBINED_SINK.to_string()
                },
                GraphCall::CreateCombinedSink {
                    sink_name: COMBINED_SINK.to_string()
                },
                jbl_load(),
            ]
        );
        assert_eq!(
            fake.all_calls().last(),
            Some(&GraphCall::Sinks),
            "the target is resolved after the route: {:?}",
            fake.all_calls()
        );
        let mut spotify = state.spotify.lock().await;
        assert_eq!(spotify.poll_liveness().status, SpotifyStatus::Stopped);
        assert_eq!(spotify.current_sink(), None);
    }

    // Criterion (#147, guard, the checks come before the routing): a start
    // that has nothing to spawn sends the actor no routing message at all —
    // an empty selection is refused (400), and a backend already running
    // answers its state. The router here records what it is sent and never
    // answers: a start that routed first would wait on it, and give up 503.
    // The control: a stopped backend with a selection sends its one routing
    // message, on the request path.
    #[tokio::test(start_paused = true)]
    async fn test_spotify_start_with_nothing_to_spawn_sends_the_actor_no_message() {
        let fake = graph::fake::FakeGraph::with_sinks(&[JBL_SINK]);
        let mut state = selected_state(&fake, &[JBL], &[]).await;
        let (router, received) = recording_router();
        state.router = router;

        let refused = spotify_start(State(state.clone())).await;
        assert_eq!(
            failure(&refused).map(|(status, _)| status),
            Some(StatusCode::BAD_REQUEST)
        );
        assert_eq!(*received.lock().unwrap(), Vec::new());

        let connected = vec![JBL.to_string()];
        state
            .targets
            .lock()
            .await
            .select(JBL, &connected)
            .expect("a connected speaker can be selected");
        let child =
            spotify::spawn_bound_to_this_thread("sleep", &["30".to_string()]).expect("spawn sleep");
        state.spotify.lock().await.adopt_child_for_test(child);

        let running = spotify_start(State(state.clone())).await;
        assert_eq!(
            running.ok().map(|Json(reply)| reply.status),
            Some(SpotifyStatus::Running)
        );
        assert_eq!(*received.lock().unwrap(), Vec::new());

        state
            .spotify
            .lock()
            .await
            .stop()
            .expect("the backend stops");
        let before = tokio::time::Instant::now().into_std();
        let starting = tokio::spawn(spotify_start(State(state.clone())));
        settle().await;
        assert_eq!(
            *received.lock().unwrap(),
            vec![(
                format!("RouteForSpotify [{JBL}@0]"),
                Some(before + Duration::from_millis(300))
            )],
            "control: a start with something to spawn routes first, as a request"
        );
        starting.abort();
    }

    // Criterion (#145, non-nominal): with the sink list unreadable,
    // `POST /volume` fails with the graph's own failure (500, as today) —
    // never "no PipeWire sink for speaker", which would blame a speaker for
    // a graph that could not be read.
    #[tokio::test]
    async fn test_volume_on_an_unreadable_sink_list_answers_500_with_the_graph_failure() {
        let fake = graph::fake::FakeGraph::with_sinks(&[JBL_SINK]);
        fake.fail(graph::fake::GraphOp::Sinks);
        let state = selected_state(&fake, &[JBL], &[JBL]).await;

        let answer = volume(State(state), Json(VolumeRequest { level: 0.8 })).await;

        let failed = failure(&answer);
        assert_eq!(
            failed.as_ref().map(|(status, _)| *status),
            Some(StatusCode::INTERNAL_SERVER_ERROR)
        );
        assert!(
            failed.as_ref().is_some_and(|(_, message)| {
                message.contains("Sinks told to fail") && !message.contains("no PipeWire sink")
            }),
            "got {failed:?}"
        );
    }

    // Criterion (#145): `GET /playback` behind an actor held past
    // `REQUEST_BOUND` answers 200 at the bound, says the graph is
    // unresponsive, and carries the commanded level — not the sinks' live
    // 0.8, which it could not have read. Nothing reaches the graph, before
    // or after the release.
    #[tokio::test(start_paused = true)]
    async fn test_playback_behind_a_held_actor_answers_unresponsive_with_the_commanded_volume() {
        use graph::fake::{FakeGraph, GraphCall};

        let fake = FakeGraph::with_sinks(&[JBL_SINK, SONY_SINK]);
        fake.set_volume(JBL_SINK, 0.8);
        fake.set_volume(SONY_SINK, 0.8);
        let state = selected_state(&fake, &[JBL, SONY], &[JBL, SONY]).await;
        let held = state.router.hold_actor();

        let request = tokio::spawn(playback(State(state.clone())));
        settle().await;
        assert!(!request.is_finished(), "the poll waits behind the hold");
        tokio::time::advance(router_handle::REQUEST_BOUND).await;
        settle().await;
        assert!(request.is_finished(), "the poll answers at the bound");
        let Json(reply) = request.await.expect("the handler task ends");
        drop(held);
        settle().await;

        assert_eq!(reply.audio_graph, AudioGraphStatus::Unresponsive);
        assert_eq!(reply.volume, COMMANDED);
        assert_eq!(reply.status, PlaybackStatus::Stopped);
        assert_eq!(fake.all_calls(), Vec::<GraphCall>::new());
    }

    // Criterion (#147): several identical volume reads queued — two
    // `GET /playback` polls behind a held actor. Once it is released the
    // read runs once, one read of the sink list, and both polls answer
    // responsive with the live level.
    #[tokio::test(start_paused = true)]
    async fn test_playback_polls_queued_behind_a_held_actor_are_answered_by_one_read() {
        let fake = graph::fake::FakeGraph::with_sinks(&[JBL_SINK, SONY_SINK]);
        fake.set_volume(JBL_SINK, 0.6);
        fake.set_volume(SONY_SINK, 0.6);
        let state = selected_state(&fake, &[JBL, SONY], &[JBL, SONY]).await;
        let held = state.router.hold_actor();

        let first = tokio::spawn(playback(State(state.clone())));
        settle().await;
        tokio::time::advance(Duration::from_millis(100)).await;
        let second = tokio::spawn(playback(State(state.clone())));
        settle().await;
        assert!(
            !first.is_finished() && !second.is_finished(),
            "both polls wait behind the hold"
        );
        drop(held);
        settle().await;

        for poll in [first, second] {
            assert!(poll.is_finished(), "the poll was answered");
            let Json(reply) = poll.await.expect("the handler task ends");
            assert_eq!(reply.audio_graph, AudioGraphStatus::Responsive);
            assert_eq!(reply.volume, 0.6);
        }
        assert_eq!(sink_list_reads(&fake), 1, "calls: {:?}", fake.all_calls());
    }

    // Criterion (#145): with the sink list unreadable, `GET /playback`
    // answers unresponsive with the commanded level.
    #[tokio::test]
    async fn test_playback_on_an_unreadable_sink_list_answers_unresponsive() {
        let fake = graph::fake::FakeGraph::with_sinks(&[JBL_SINK]);
        fake.set_volume(JBL_SINK, 0.8);
        fake.fail(graph::fake::GraphOp::Sinks);
        let state = selected_state(&fake, &[JBL], &[JBL]).await;

        let Json(reply) = playback(State(state)).await;

        assert_eq!(reply.audio_graph, AudioGraphStatus::Unresponsive);
        assert_eq!(reply.volume, COMMANDED);
    }

    // Criterion (#145, guard, stops at the first failure): two speakers and
    // an unreadable sink list — after the failed `sinks()` the poll asks the
    // graph nothing more. The near miss is the second speaker: a loop that
    // carries on reads the list again, and still answers unresponsive.
    #[tokio::test]
    async fn test_playback_stops_at_the_first_graph_failure() {
        let fake = graph::fake::FakeGraph::with_sinks(&[JBL_SINK, SONY_SINK]);
        fake.fail(graph::fake::GraphOp::Sinks);
        let state = selected_state(&fake, &[JBL, SONY], &[JBL, SONY]).await;

        let Json(reply) = playback(State(state)).await;

        assert_eq!(fake.all_calls(), vec![graph::fake::GraphCall::Sinks]);
        assert_eq!(reply.audio_graph, AudioGraphStatus::Unresponsive);
    }

    // Criterion (#145, guard, exactly once): two selected speakers, one read
    // of the sink list. The near miss is the second speaker: with one, a
    // per-speaker read also reads the list once. The levels agree, so the
    // live level is what the reply carries.
    #[tokio::test]
    async fn test_playback_with_two_speakers_reads_the_sink_list_once() {
        let fake = graph::fake::FakeGraph::with_sinks(&[JBL_SINK, SONY_SINK]);
        fake.set_volume(JBL_SINK, 0.6);
        fake.set_volume(SONY_SINK, 0.6);
        let state = selected_state(&fake, &[JBL, SONY], &[JBL, SONY]).await;

        let Json(reply) = playback(State(state)).await;

        assert_eq!(sink_list_reads(&fake), 1, "calls: {:?}", fake.all_calls());
        assert_eq!(reply.audio_graph, AudioGraphStatus::Responsive);
        assert_eq!(reply.volume, 0.6);
    }

    // Criterion (#145, guard, an absent sink is not a stall): the Sony is
    // selected but its sink is missing from a list that read fine. The poll
    // stays responsive, and the level is what `reported_volume` makes of
    // `[Some(0.8), None]` — the commanded one, as today. The near miss is
    // that `None`: an implementation mapping it to "unresponsive" fails here
    // only.
    #[tokio::test]
    async fn test_playback_with_a_selected_speaker_whose_sink_is_absent_answers_responsive() {
        let fake = graph::fake::FakeGraph::with_sinks(&[JBL_SINK]);
        fake.set_volume(JBL_SINK, 0.8);
        let state = selected_state(&fake, &[JBL, SONY], &[JBL, SONY]).await;

        let Json(reply) = playback(State(state)).await;

        assert_eq!(reply.audio_graph, AudioGraphStatus::Responsive);
        assert_eq!(reply.volume, COMMANDED);
    }

    // Criterion (#148, guard, stops at the first failure): the sink list
    // reads fine, then the JBL's level read fails — the graph stopped
    // answering after `sinks()`. The poll answers unresponsive with the
    // commanded level, and the Sony's level is never asked. The near miss is
    // the Sony, listed and readable: a read that collects every result and
    // then looks for an `Err` still answers unresponsive, but asks for it.
    #[tokio::test]
    async fn test_playback_on_a_failed_level_read_answers_unresponsive_and_asks_nothing_more() {
        use graph::fake::{FakeGraph, GraphCall, GraphOp};

        let fake = FakeGraph::with_sinks(&[JBL_SINK, SONY_SINK]);
        fake.set_volume(JBL_SINK, 0.8);
        fake.set_volume(SONY_SINK, 0.8);
        fake.fail_for(GraphOp::SinkVolume, JBL_SINK);
        let state = selected_state(&fake, &[JBL, SONY], &[JBL, SONY]).await;

        let Json(reply) = playback(State(state)).await;

        assert_eq!(reply.audio_graph, AudioGraphStatus::Unresponsive);
        assert_eq!(reply.volume, COMMANDED);
        assert!(
            !fake.all_calls().contains(&GraphCall::SinkVolume {
                sink: SONY_SINK.to_string()
            }),
            "the Sony is not asked after the JBL's read failed: {:?}",
            fake.all_calls()
        );
    }

    // Criterion (#148, guard, a failure is never "no level"): the *last*
    // speaker's level read fails. Swallowed into `None`, `reported_volume`
    // of `[Some(0.6), None]` gives the commanded level — which the
    // unresponsive path reports too — so `audio_graph` is what tells them
    // apart, and it is what this test asserts.
    #[tokio::test]
    async fn test_playback_on_a_failed_last_level_read_answers_unresponsive() {
        use graph::fake::{FakeGraph, GraphOp};

        let fake = FakeGraph::with_sinks(&[JBL_SINK, SONY_SINK]);
        fake.set_volume(JBL_SINK, 0.6);
        fake.set_volume(SONY_SINK, 0.6);
        fake.fail_for(GraphOp::SinkVolume, SONY_SINK);
        let state = selected_state(&fake, &[JBL, SONY], &[JBL, SONY]).await;

        let Json(reply) = playback(State(state)).await;

        assert_eq!(reply.audio_graph, AudioGraphStatus::Unresponsive);
        assert_eq!(reply.volume, COMMANDED);
    }

    // Criterion (#148, guard, no level is not a stall): the Sony's sink is
    // listed but has no level. The poll stays responsive, and the level is
    // what `reported_volume` makes of `[Some(0.8), None]` — the commanded
    // one. The near miss is the *listed* sink without a level, not an absent
    // one: an implementation mapping every `None` to a failure fails here.
    #[tokio::test]
    async fn test_playback_with_a_listed_sink_without_a_level_answers_responsive() {
        let fake = graph::fake::FakeGraph::with_sinks(&[JBL_SINK, SONY_SINK]);
        fake.set_volume(JBL_SINK, 0.8);
        let state = selected_state(&fake, &[JBL, SONY], &[JBL, SONY]).await;

        let Json(reply) = playback(State(state)).await;

        assert_eq!(reply.audio_graph, AudioGraphStatus::Responsive);
        assert_eq!(reply.volume, COMMANDED);
    }

    // Criterion (#145, guard, the empty selection asks the graph nothing):
    // with nothing selected the poll sends no message at all — it answers at
    // once while the actor is held, responsive, with the commanded level —
    // and the graph log stays empty once the actor is released. The near
    // miss is an implementation that sends an empty read anyway: right
    // answer once the actor frees, but it waits behind the hold.
    #[tokio::test(start_paused = true)]
    async fn test_playback_with_an_empty_selection_makes_no_graph_call() {
        use graph::fake::{FakeGraph, GraphCall};

        let fake = FakeGraph::with_sinks(&[JBL_SINK]);
        let state = selected_state(&fake, &[JBL], &[]).await;
        let held = state.router.hold_actor();

        let request = tokio::spawn(playback(State(state.clone())));
        settle().await;
        assert!(request.is_finished(), "no actor is waited for");
        let Json(reply) = request.await.expect("the handler task ends");
        drop(held);
        settle().await;

        assert_eq!(reply.audio_graph, AudioGraphStatus::Responsive);
        assert_eq!(reply.volume, COMMANDED);
        assert_eq!(fake.all_calls(), Vec::<GraphCall>::new());
    }

    // Criterion (#147, guard, the empty selection sends no message): the
    // same poll over a router that records what it is sent — nothing. The
    // held actor above shows the poll does not wait; this shows it does not
    // even send.
    #[tokio::test(start_paused = true)]
    async fn test_playback_with_an_empty_selection_sends_the_actor_no_message() {
        let fake = graph::fake::FakeGraph::with_sinks(&[JBL_SINK]);
        let mut state = selected_state(&fake, &[JBL], &[]).await;
        let (router, received) = recording_router();
        state.router = router;

        let Json(reply) = playback(State(state.clone())).await;

        assert_eq!(reply.audio_graph, AudioGraphStatus::Responsive);
        assert_eq!(reply.volume, COMMANDED);
        assert_eq!(*received.lock().unwrap(), Vec::new());

        // Control: with the JBL selected the same poll sends its one read.
        let connected = vec![JBL.to_string()];
        state
            .targets
            .lock()
            .await
            .select(JBL, &connected)
            .expect("a connected speaker can be selected");
        let poll = tokio::spawn(playback(State(state.clone())));
        settle().await;
        let sent: Vec<String> = received
            .lock()
            .unwrap()
            .iter()
            // Cloned out of the lock, as a snapshot.
            .map(|(message, _)| message.clone())
            .collect();
        assert_eq!(sent, vec![format!("SinkVolumes [{JBL}]")]);
        poll.abort();
    }

    // Criterion (#145): over a readable graph `GET /playback` answers
    // responsive with the same level as before this change — the live one.
    #[tokio::test]
    async fn test_playback_over_a_readable_graph_answers_responsive_with_the_live_volume() {
        let fake = graph::fake::FakeGraph::with_sinks(&[JBL_SINK]);
        fake.set_volume(JBL_SINK, 0.6);
        let state = selected_state(&fake, &[JBL], &[JBL]).await;

        let Json(reply) = playback(State(state)).await;

        assert_eq!(reply.audio_graph, AudioGraphStatus::Responsive);
        assert_eq!(reply.volume, 0.6);
    }

    // Criterion (#145): `/select` and `/deselect` answer at once while the
    // actor is held, with the stored selection. "At once" rather than
    // "within `REQUEST_BOUND`": every selection change is handed to the
    // background applier, so neither waits for the actor at all.
    #[tokio::test(start_paused = true)]
    async fn test_select_and_deselect_answer_at_once_while_the_actor_is_held() {
        let fake = graph::fake::FakeGraph::with_sinks(&[JBL_SINK, SONY_SINK, COMBINED_SINK]);
        fake.seed_branch(COMBINED_SINK, JBL_SINK, 0, Some(true));
        let state = selected_state(&fake, &[JBL, SONY], &[JBL]).await;
        start_applier(&state, &fake).await;
        let held = state.router.hold_actor();

        let selected = tokio::spawn(select_target(State(state.clone()), Path(SONY.to_string())));
        settle().await;
        assert!(selected.is_finished(), "select waits for no actor");
        let selected = selected.await.expect("the handler task ends");
        assert_eq!(
            selected.as_ref().ok().map(|reply| addresses(&reply.0)),
            Some(vec![JBL.to_string(), SONY.to_string()])
        );

        let deselected = tokio::spawn(deselect_target(
            State(state.clone()),
            Path(SONY.to_string()),
        ));
        settle().await;
        assert!(deselected.is_finished(), "deselect waits for no actor");
        let Json(after) = deselected.await.expect("the handler task ends");
        assert_eq!(addresses(&after), vec![JBL.to_string()]);
        assert_eq!(
            fake.all_calls(),
            Vec::<graph::fake::GraphCall>::new(),
            "nothing reaches the graph while the actor is held"
        );
        drop(held);
    }

    // Criterion (#145, guard, the latest selection, never a stale snapshot):
    // the Sony is selected, then deselected, both behind an actor held past
    // `REQUEST_BOUND`. Once it is released, one routing pass runs for the
    // selection current at that moment — the JBL alone, already routed — so
    // no branch into the Sony is ever loaded. The near miss is the first
    // change's snapshot, `[JBL, Sony]`, which the applier has already sent
    // when the second change lands: run as it was sent, it loads the Sony's
    // branch, and the next pass unloads it. The actor is held longer than
    // the bound, so an applier waiting with the request-path bound drops the
    // pass and fails the count.
    #[tokio::test(start_paused = true)]
    async fn test_selection_changes_behind_a_held_actor_route_the_latest_selection_once() {
        use graph::fake::{FakeGraph, GraphCall};

        let fake = FakeGraph::with_sinks(&[JBL_SINK, SONY_SINK, COMBINED_SINK]);
        fake.seed_branch(COMBINED_SINK, JBL_SINK, 0, Some(true));
        let state = selected_state(&fake, &[JBL, SONY], &[JBL]).await;
        start_applier(&state, &fake).await;
        let held = state.router.hold_actor();

        let _select = tokio::spawn(select_target(State(state.clone()), Path(SONY.to_string())));
        settle().await;
        let _deselect = tokio::spawn(deselect_target(
            State(state.clone()),
            Path(SONY.to_string()),
        ));
        settle().await;
        tokio::time::advance(router_handle::REQUEST_BOUND * 2).await;
        settle().await;
        assert_eq!(
            fake.all_calls(),
            Vec::<GraphCall>::new(),
            "nothing reaches the graph while the actor is held"
        );

        drop(held);
        settle().await;

        assert_eq!(passes(&fake), 1, "calls: {:?}", fake.all_calls());
        assert!(
            !fake.all_calls().iter().any(|call| matches!(
                call,
                GraphCall::LoadBranch { real_sink, .. } if real_sink == SONY_SINK
            )),
            "the Sony's branch was loaded: {:?}",
            fake.all_calls()
        );
        assert_eq!(
            fake.calls(),
            Vec::<GraphCall>::new(),
            "the latest selection is the one already routed"
        );
        let loaded: Vec<String> = fake
            .loaded(COMBINED_SINK)
            .into_iter()
            .map(|b| b.branch.sink)
            .collect();
        assert_eq!(loaded, vec![JBL_SINK.to_string()]);
    }

    // Criterion (#145, guard, the applier's message never expires and is
    // never given up): one selection change — the Sony selected — behind an
    // actor held 3 s past `REQUEST_BOUND`, and nothing after it to wake the
    // applier again. Once the actor is released the Sony's branch is loaded,
    // once. The near miss is an applier sending its routing as a request: it
    // gives up at the bound, or expires at the release, and with no second
    // change to retry on, the Sony stays silent until the next tick.
    #[tokio::test(start_paused = true)]
    async fn test_a_single_selection_change_behind_an_actor_held_past_the_bound_is_still_applied() {
        use graph::fake::{FakeGraph, GraphCall};

        let fake = FakeGraph::with_sinks(&[JBL_SINK, SONY_SINK, COMBINED_SINK]);
        fake.seed_branch(COMBINED_SINK, JBL_SINK, 0, Some(true));
        let state = selected_state(&fake, &[JBL, SONY], &[JBL]).await;
        start_applier(&state, &fake).await;
        let held = state.router.hold_actor();

        let selected = tokio::spawn(select_target(State(state.clone()), Path(SONY.to_string())));
        settle().await;
        assert!(selected.is_finished(), "select waits for no actor");
        tokio::time::advance(router_handle::REQUEST_BOUND + Duration::from_secs(3)).await;
        settle().await;
        assert_eq!(
            fake.all_calls(),
            Vec::<GraphCall>::new(),
            "nothing reaches the graph while the actor is held"
        );

        drop(held);
        settle().await;

        assert_eq!(
            fake.calls(),
            vec![GraphCall::LoadBranch {
                sink_name: COMBINED_SINK.to_string(),
                real_sink: SONY_SINK.to_string(),
                latency_ms: 0
            }]
        );
        assert_eq!(passes(&fake), 1, "calls: {:?}", fake.all_calls());
    }

    // Criteria (#147): the applier reads the routing generation, then the
    // selection, and sends one apply-selection stamped with it; once that
    // is applied, a running `librespot` that feeds another sink is respawned
    // through a routing message of its own, sent in the background — no
    // start deadline. The router here is scripted: it applies the selection
    // and refuses the Spotify route, so nothing is spawned, and what it was
    // sent is all the applier did. One `request_routing()` puts the
    // generation at 1.
    #[tokio::test(start_paused = true)]
    async fn test_the_applier_sends_the_stamped_selection_then_spotify_s_route_in_the_background() {
        let fake = graph::fake::FakeGraph::with_sinks(&[JBL_SINK, SONY_SINK]);
        let mut state = selected_state(&fake, &[JBL, SONY], &[JBL, SONY]).await;
        state.targets.lock().await.set_offset(SONY, 70);
        let received: Received = Arc::default();
        let log = Arc::clone(&received);
        state.router = RouterHandle::over(
            Box::new(move |envelope: router_actor::Envelope| {
                log.lock().unwrap().push((
                    router_actor::testing::describe(&envelope.message),
                    envelope.start_by,
                ));
                match envelope.message {
                    router_actor::Message::ApplySelection { reply, .. } => {
                        let _ = reply.send(Ok(()));
                    },
                    router_actor::Message::RouteForSpotify { reply, .. } => {
                        let _ = reply.send(Err(RouterError::Audio(AudioError::PipeWire(
                            "no daemon".to_string(),
                        ))));
                    },
                    _ => {},
                }
                Ok(())
            }),
            router_actor::Shared::new(),
        );
        // A running backend whose sink is not the combined one: `sleep`
        // stands for a `librespot` started towards something else.
        let child =
            spotify::spawn_bound_to_this_thread("sleep", &["30".to_string()]).expect("spawn sleep");
        state.spotify.lock().await.adopt_child_for_test(child);
        spawn_routing_applier(state.clone());
        settle().await;
        assert_eq!(
            *received.lock().unwrap(),
            Vec::new(),
            "nothing was asked yet"
        );

        state.router.request_routing();
        settle().await;

        let selection = format!("{JBL}@0,{SONY}@70");
        assert_eq!(
            *received.lock().unwrap(),
            vec![
                (format!("ApplySelection [{selection}] 1"), None),
                (format!("RouteForSpotify [{selection}]"), None),
            ]
        );
        assert_eq!(
            state.spotify.lock().await.poll_liveness().status,
            SpotifyStatus::Stopped,
            "the backend fed another sink: it was stopped, and its route was refused"
        );
    }

    // Criterion (#145): `/offset` answers within `REQUEST_BOUND` while the
    // actor is held, with the stored offset — each retune timed out, which
    // hands the change to the applier; after two offset changes the release
    // retunes the Sony's branch once, to the latest offset (240), never to
    // the intermediate one (120).
    #[tokio::test(start_paused = true)]
    async fn test_offset_changes_behind_a_held_actor_answer_in_time_and_apply_the_latest_once() {
        use graph::fake::{FakeGraph, GraphCall};

        let fake = FakeGraph::with_sinks(&[JBL_SINK, SONY_SINK, COMBINED_SINK]);
        fake.seed_branch(COMBINED_SINK, JBL_SINK, 0, Some(true));
        let sony_branch = fake.seed_branch(COMBINED_SINK, SONY_SINK, 0, Some(true));
        let state = selected_state(&fake, &[JBL, SONY], &[JBL, SONY]).await;
        start_applier(&state, &fake).await;
        let held = state.router.hold_actor();

        for offset_ms in [120, 240] {
            let request = tokio::spawn(set_target_offset(
                State(state.clone()),
                Path(SONY.to_string()),
                Json(OffsetRequest { offset_ms }),
            ));
            settle().await;
            assert!(
                !request.is_finished(),
                "the {offset_ms} ms change waits for its retune"
            );
            tokio::time::advance(router_handle::REQUEST_BOUND).await;
            settle().await;
            assert!(
                request.is_finished(),
                "the {offset_ms} ms change answers within the bound"
            );
            let Json(reply) = request.await.expect("the handler task ends");
            assert_eq!(offset_of(&reply, SONY), Some(offset_ms));
        }
        assert_eq!(fake.all_calls(), Vec::<GraphCall>::new());

        drop(held);
        settle().await;

        assert_eq!(
            fake.calls(),
            vec![GraphCall::SetBranchDelay {
                id: sony_branch,
                delay_ms: 240
            }]
        );
    }

    // Criterion (#147): an offset retune that *expired* — the actor frees
    // 301 ms after the request, so the message is not run — is handed to
    // the routing applier exactly as one that timed out is: the handler
    // answers 200 with the stored offset as soon as the expiry comes back,
    // and the applier retunes the Sony's branch to it, once. The near miss
    // is an expiry treated as any other graph failure: logged, and the
    // offset left unapplied until the next tick.
    #[tokio::test(start_paused = true)]
    async fn test_an_offset_change_whose_retune_expired_is_applied_by_the_applier_once() {
        use graph::fake::{FakeGraph, GraphCall};

        let fake = FakeGraph::with_sinks(&[JBL_SINK, SONY_SINK, COMBINED_SINK]);
        fake.seed_branch(COMBINED_SINK, JBL_SINK, 0, Some(true));
        let sony_branch = fake.seed_branch(COMBINED_SINK, SONY_SINK, 0, Some(true));
        let state = selected_state(&fake, &[JBL, SONY], &[JBL, SONY]).await;
        start_applier(&state, &fake).await;
        let held = state.router.hold_actor();

        let request = tokio::spawn(set_target_offset(
            State(state.clone()),
            Path(SONY.to_string()),
            Json(OffsetRequest { offset_ms: 120 }),
        ));
        settle().await;
        tokio::time::advance(Duration::from_millis(301)).await;
        settle().await;
        assert!(!request.is_finished(), "the change waits for its retune");
        assert_eq!(fake.all_calls(), Vec::<GraphCall>::new());

        drop(held);
        settle().await;

        assert!(request.is_finished(), "the expiry is answered at once");
        let Json(reply) = request.await.expect("the handler task ends");
        assert_eq!(offset_of(&reply, SONY), Some(120));
        assert_eq!(
            fake.calls(),
            vec![GraphCall::SetBranchDelay {
                id: sony_branch,
                delay_ms: 120
            }],
            "the applier applied the stored offset, once"
        );
    }

    // Criterion (#147, guard, only "nothing was done" hands an offset to the
    // applier): a retune the graph answered with an error — the delay was
    // refused — is logged and does not call `request_routing`. The near miss
    // is this `AudioError::PipeWire`: an arm widened to every error would
    // re-route on a refused delay — a second pass over the branches and a
    // second attempt at the delay. The handler still answers 200 with the
    // stored offset.
    #[tokio::test(start_paused = true)]
    async fn test_an_offset_change_whose_retune_the_graph_refused_is_not_handed_to_the_applier() {
        use graph::fake::{FakeGraph, GraphCall, GraphOp};

        let fake = FakeGraph::with_sinks(&[JBL_SINK, SONY_SINK, COMBINED_SINK]);
        fake.seed_branch(COMBINED_SINK, JBL_SINK, 0, Some(true));
        let sony_branch = fake.seed_branch(COMBINED_SINK, SONY_SINK, 0, Some(true));
        fake.fail(GraphOp::SetBranchDelay);
        let state = selected_state(&fake, &[JBL, SONY], &[JBL, SONY]).await;
        start_applier(&state, &fake).await;
        let generation = state.router.routing_generation();

        let Json(reply) = set_target_offset(
            State(state.clone()),
            Path(SONY.to_string()),
            Json(OffsetRequest { offset_ms: 120 }),
        )
        .await;
        settle().await;

        assert_eq!(offset_of(&reply, SONY), Some(120));
        assert_eq!(
            fake.calls(),
            vec![GraphCall::SetBranchDelay {
                id: sony_branch,
                delay_ms: 120
            }],
            "the retune's one refused attempt, and no applier pass after it"
        );
        assert_eq!(passes(&fake), 1, "calls: {:?}", fake.all_calls());
        assert_eq!(state.router.routing_generation(), generation);
    }

    // Criterion (#147): a retune that finds the sink list unreadable answers
    // that error — another error than "nothing was done" — so the handler
    // logs it, does not call `request_routing`, and still answers 200 with
    // the stored offset. The graph is asked for its sink list and nothing
    // more.
    #[tokio::test(start_paused = true)]
    async fn test_an_offset_change_over_an_unreadable_sink_list_answers_the_stored_offset() {
        use graph::fake::{FakeGraph, GraphCall, GraphOp};

        let fake = FakeGraph::with_sinks(&[JBL_SINK, SONY_SINK, COMBINED_SINK]);
        fake.seed_branch(COMBINED_SINK, JBL_SINK, 0, Some(true));
        fake.seed_branch(COMBINED_SINK, SONY_SINK, 0, Some(true));
        fake.fail(GraphOp::Sinks);
        let state = selected_state(&fake, &[JBL, SONY], &[JBL, SONY]).await;
        start_applier(&state, &fake).await;
        let generation = state.router.routing_generation();

        let Json(reply) = set_target_offset(
            State(state.clone()),
            Path(SONY.to_string()),
            Json(OffsetRequest { offset_ms: 120 }),
        )
        .await;
        settle().await;

        assert_eq!(offset_of(&reply, SONY), Some(120));
        assert_eq!(fake.all_calls(), vec![GraphCall::Sinks]);
        assert_eq!(state.router.routing_generation(), generation);
    }

    // Criterion (#145): an offset change retunes only a combined sink that
    // is loaded — with none, the graph is asked whether it exists and
    // nothing more. The near miss is the missing sink: a retune that skips
    // the check goes on to read the branches of a sink that is not there.
    #[tokio::test]
    async fn test_offset_change_with_no_combined_sink_loaded_retunes_nothing() {
        use graph::fake::{FakeGraph, GraphCall};

        let fake = FakeGraph::with_sinks(&[JBL_SINK]);
        let state = selected_state(&fake, &[JBL], &[JBL]).await;

        let Json(reply) = set_target_offset(
            State(state),
            Path(JBL.to_string()),
            Json(OffsetRequest { offset_ms: 120 }),
        )
        .await;

        assert_eq!(offset_of(&reply, JBL), Some(120));
        assert_eq!(fake.all_calls(), vec![GraphCall::Sinks]);
    }

    // Criterion (#145, #67): deselecting the last speaker behind a held
    // actor answers at once with an empty selection; Spotify is stopped and
    // the tone paused before it answers, as today — they need no actor —
    // and the combined sink is torn down once the actor is released, never
    // dropped, even after a hold longer than `REQUEST_BOUND`.
    #[tokio::test(start_paused = true)]
    async fn test_last_deselect_behind_a_held_actor_tears_down_once_released() {
        use graph::fake::{FakeGraph, GraphCall};

        let fake = FakeGraph::with_sinks(&[JBL_SINK, COMBINED_SINK]);
        fake.seed_branch(COMBINED_SINK, JBL_SINK, 0, Some(true));
        let state = selected_state(&fake, &[JBL], &[JBL]).await;
        state
            .engine
            .lock()
            .await
            .play()
            .expect("the null output plays");
        let child =
            spotify::spawn_bound_to_this_thread("sleep", &["30".to_string()]).expect("spawn sleep");
        state.spotify.lock().await.adopt_child_for_test(child);
        start_applier(&state, &fake).await;
        let held = state.router.hold_actor();

        let request = tokio::spawn(deselect_target(State(state.clone()), Path(JBL.to_string())));
        settle().await;
        assert!(request.is_finished(), "deselect waits for no actor");
        let Json(after) = request.await.expect("the handler task ends");
        assert_eq!(addresses(&after), Vec::<String>::new());
        assert_eq!(
            state.engine.lock().await.poll_state().status,
            PlaybackStatus::Paused
        );
        assert_eq!(
            state.spotify.lock().await.poll_liveness().status,
            SpotifyStatus::Stopped
        );

        tokio::time::advance(router_handle::REQUEST_BOUND * 2).await;
        settle().await;
        assert_eq!(fake.all_calls(), Vec::<GraphCall>::new());

        drop(held);
        settle().await;

        assert_eq!(
            fake.calls(),
            vec![GraphCall::Teardown {
                sink_name: COMBINED_SINK.to_string()
            }]
        );
    }

    // Criterion (#145, non-nominal): the `/devices` last-loss teardown goes
    // through the same applier — the poll's sync returns at once behind a
    // held actor, and the teardown runs once the actor is released.
    #[tokio::test(start_paused = true)]
    async fn test_last_loss_on_a_devices_poll_behind_a_held_actor_tears_down_once_released() {
        use graph::fake::{FakeGraph, GraphCall};

        let fake = FakeGraph::with_sinks(&[JBL_SINK, COMBINED_SINK]);
        fake.seed_branch(COMBINED_SINK, JBL_SINK, 0, Some(true));
        let state = selected_state(&fake, &[JBL], &[JBL]).await;
        // Off: nothing promises the speaker back, so its loss tears down.
        state.name.lock().await.set_restore_during_playback(false);
        start_applier(&state, &fake).await;
        let held = state.router.hold_actor();

        let polled = state.clone();
        let sync = tokio::spawn(async move { sync_connected(&polled, &[]).await });
        settle().await;
        assert!(sync.is_finished(), "the poll waits for no actor");
        assert!(state.targets.lock().await.speakers().is_empty());
        assert_eq!(fake.all_calls(), Vec::<GraphCall>::new());

        drop(held);
        settle().await;

        assert_eq!(
            fake.calls(),
            vec![GraphCall::Teardown {
                sink_name: COMBINED_SINK.to_string()
            }]
        );
    }

    // Criterion (#145, guard, background waits are unbounded): a repair pass
    // behind an actor held 3 s past `REQUEST_BOUND` (5 s) is still waiting,
    // not given up, and rebuilds once the actor is released. The near miss
    // is the hold's length: a handle applying the bound everywhere drops
    // this pass at 2 s, and one stamping it with a start deadline expires it
    // at the release.
    #[tokio::test(start_paused = true)]
    async fn test_repair_pass_behind_an_actor_held_past_the_request_bound_still_routes() {
        let fake = graph::fake::FakeGraph::new();
        let state = vanished_state(&fake, true).await;
        let held = state.router.hold_actor();

        let repairing = state.clone();
        let pass = tokio::spawn(async move {
            branch_repair_pass(&repairing, audio::PassReason::SafetyNet).await
        });
        settle().await;
        // 5 s with the 2 s bound: longer than the bound whatever its value.
        tokio::time::advance(router_handle::REQUEST_BOUND + Duration::from_secs(3)).await;
        settle().await;
        assert!(
            !pass.is_finished(),
            "the pass is still waiting past the bound"
        );
        assert!(fake.all_calls().is_empty(), "calls: {:?}", fake.all_calls());

        drop(held);
        settle().await;

        assert!(pass.is_finished(), "the pass ran once the actor freed");
        assert_eq!(rebuilds(&fake), 1, "calls: {:?}", fake.calls());
    }

    // Criterion (#147): the repair pass sends one repair message — the
    // selection, no start deadline — and logs and falls back from its one
    // answer exactly as before: woken by the combined sink's removal, it
    // falls back when the answer says the route failed or the re-target
    // did, and not when it says both went through. The router here is
    // scripted, so the answer is the only thing the pass can have read: a
    // pass that asked the graph anything else would find nothing behind it.
    #[tokio::test]
    async fn test_repair_pass_sends_one_repair_message_and_falls_back_from_its_one_answer() {
        use router_actor::RepairOutcome;

        let answers: [(fn() -> RepairOutcome, bool); 3] = [
            (
                || RepairOutcome {
                    routed: Ok(()),
                    changed: true,
                    retarget_failed: false,
                },
                false,
            ),
            (
                || RepairOutcome {
                    routed: Err(AudioError::PipeWire("no sink for the JBL".to_string())),
                    changed: false,
                    retarget_failed: false,
                },
                true,
            ),
            (
                || RepairOutcome {
                    routed: Ok(()),
                    changed: true,
                    retarget_failed: true,
                },
                true,
            ),
        ];
        for (outcome, falls_back) in answers {
            let fake = graph::fake::FakeGraph::new();
            let mut state = vanished_state(&fake, true).await;
            let (router, received) = router_answering_a_repair_with(outcome());
            state.router = router;

            let fell_back = branch_repair_pass(&state, combined_reason()).await;

            assert_eq!(fell_back, falls_back, "answer: {:?}", outcome());
            assert_eq!(
                *received.lock().unwrap(),
                vec![(format!("Repair [{JBL}@0]"), None)]
            );
            assert_tone_untouched_and_nothing_claimed(&state).await;
        }
    }

    // Criterion (#147, guard): the repair pass's guard runs before anything
    // is sent — with nothing playing, the pass sends the actor no message.
    // The control is the same state once the tone plays: one repair.
    #[tokio::test(start_paused = true)]
    async fn test_repair_pass_sends_no_message_while_nothing_plays() {
        let fake = graph::fake::FakeGraph::new();
        let mut state = vanished_state(&fake, true).await;
        let (router, received) = recording_router();
        state.router = router;
        state
            .engine
            .lock()
            .await
            .stop()
            .expect("the null output stops");

        let fell_back = branch_repair_pass(&state, audio::PassReason::SafetyNet).await;

        assert!(!fell_back);
        assert_eq!(*received.lock().unwrap(), Vec::new());

        state
            .engine
            .lock()
            .await
            .play()
            .expect("the null output plays");
        let repairing = state.clone();
        let pass = tokio::spawn(async move {
            branch_repair_pass(&repairing, audio::PassReason::SafetyNet).await
        });
        settle().await;
        assert_eq!(
            *received.lock().unwrap(),
            vec![(format!("Repair [{JBL}@0]"), None)],
            "control: a playing state sends its one repair"
        );
        pass.abort();
    }

    // Criterion (#147): the confirmation timer learns the due time without
    // sending a message — with nothing armed it sends the actor nothing,
    // however long it waits. The near miss is a timer that asks the actor
    // when the next reload is due.
    #[tokio::test(start_paused = true)]
    async fn test_confirmation_timer_sends_the_actor_no_message_while_nothing_is_armed() {
        let fake = graph::fake::FakeGraph::with_sinks(&[JBL_SINK, COMBINED_SINK]);
        let mut state = timed_state(&fake).await;
        let (router, received) = recording_router();
        state.router = router;

        spawn_confirmation_timer(state.clone());
        settle().await;
        tokio::time::advance(audio::CONFIRM_GAP * 3).await;
        settle().await;

        assert_eq!(*received.lock().unwrap(), Vec::new());
    }
}
