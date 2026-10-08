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

use audio::{AudioEngine, AudioError};
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
    /// The way to the routing logic, which the graph thread owns and runs one
    /// message at a time (#147). Nothing is held while a message waits: the
    /// handle sends, and the caller's task is suspended until the answer.
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

/// Env var overriding the bind address in **debug builds only** (#160): the dev
/// loop sets it once in a shell. Release builds take `--bind` instead, so an
/// environment inherited by a service cannot move the API to another
/// interface unnoticed.
const BIND_ENV: &str = "BLUE2TH_BIND";

/// The most pairing codes `--pair <n>` arms at once.
const MAX_PAIR_COUNT: u32 = 10;

/// Run the backend: initialise tracing, bind the socket and serve the router.
pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let cli = match parse_args(&args) {
        Ok(cli) => cli,
        Err(e) => {
            tracing::error!("{e}");
            return Err(e.into());
        },
    };

    let mut auth_store = AuthStore::with_store(auth::auth_store_path());
    let server_name = config::ServerName::with_store(config::name_store_path());
    let addr = resolve_bind_address(cli.bind.as_deref());

    if let Some(count) = codes_to_arm(&cli, auth_store.minted_a_new_token()) {
        let codes = auth_store.arm_pairing(count, std::time::SystemTime::now());
        tracing::info!(
            "{}",
            pairing_banners(&advertised_url(&addr), server_name.name(), &codes)
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
        RouterHandle::over_graph(graph),
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

/// The address the backend binds to with no `--bind`: `BLUE2TH_BIND` when set
/// in a debug build, otherwise the host's LAN IPv4, otherwise every interface.
///
/// Binding to the LAN address is **defence in depth, not authentication**: it
/// stops the API being served on other interfaces (a VPN, a laptop's public
/// one). It does not restrict who on the LAN may connect — that is the bearer
/// token's job.
pub fn lan_bind_address() -> String {
    resolve_bind_address(None)
}

/// The address the backend binds to, `--bind` (`cli`) first; see
/// [`bind_address`] for the precedence. Logs the warning it produces.
fn resolve_bind_address(cli: Option<&str>) -> String {
    let env = std::env::var(BIND_ENV).ok();
    let choice = bind_address(
        cli,
        env.as_deref(),
        cfg!(debug_assertions),
        preferred_lan_ipv4(&host_ipv4_addresses()),
    );
    if let Some(warning) = &choice.warning {
        tracing::warn!("{warning}");
    }
    choice.addr
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

/// The bind address the backend settles on, plus the warning to log when the
/// choice ignored something the operator set.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BindChoice {
    /// The `host:port` to bind.
    addr: String,
    /// What to tell the operator, e.g. that a release build ignored
    /// `BLUE2TH_BIND`. `None` when nothing was ignored.
    warning: Option<String>,
}

/// Pick the bind address from the `--bind` flag, the `BLUE2TH_BIND` value, the
/// build mode and a detected LAN address. Pure, so the precedence is testable
/// without touching the environment or the host's interfaces; `debug` comes
/// from `cfg!(debug_assertions)` at the call site.
fn bind_address(
    cli: Option<&str>,
    env: Option<&str>,
    debug: bool,
    detected: Option<std::net::Ipv4Addr>,
) -> BindChoice {
    // An empty env var is unset, never an empty bind address.
    let env = env.filter(|a| !a.is_empty());
    let warning = (!debug && env.is_some()).then(|| {
        format!(
            "{BIND_ENV} is read only by debug builds and was ignored; \
             use --bind <host:port> instead"
        )
    });
    let override_addr = cli.or(if debug { env } else { None });
    let addr = match (override_addr, detected) {
        // An explicit override always wins: the operator knows their network
        // better than a heuristic does.
        (Some(addr), _) => addr.to_string(),
        (None, Some(lan)) => format!("{lan}:{DEFAULT_PORT}"),
        // No routable address (no interface up): every interface, with the
        // warning left to the caller. Refusing to start would be worse.
        (None, None) => DEFAULT_BIND.to_string(),
    };
    BindChoice { addr, warning }
}

/// The options the backend reads off its command line (#160).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CliOptions {
    /// How many pairing codes `--pair [n]` asked for: `Some(1)` for a bare
    /// `--pair`, `None` when the flag is absent.
    pub pair: Option<u32>,
    /// The address `--bind <host:port>` asked for, `None` when absent.
    pub bind: Option<String>,
}

/// Why the command line was refused. The server does not start on any of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    /// `--pair` followed by something that is not a whole number in 1..=10.
    PairCount(String),
    /// `--bind` with no value, an empty one, or another flag in its place.
    BindValue,
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CliError::PairCount(value) => write!(
                f,
                "--pair expects a number of codes from 1 to {MAX_PAIR_COUNT}, got {value:?}"
            ),
            CliError::BindValue => write!(f, "--bind expects an address, as <host:port>"),
        }
    }
}

impl std::error::Error for CliError {}

/// Parse the backend's arguments — those **after** the program name. Pure.
///
/// Unknown arguments are ignored, as they always were.
pub fn parse_args(args: &[String]) -> Result<CliOptions, CliError> {
    let mut options = CliOptions::default();
    let mut args = args.iter().peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--pair" => {
                // Only `--…` is a flag: `-1` is a (refused) count, not a bare
                // `--pair` followed by something else.
                let count = match args.next_if(|next| !next.starts_with("--")) {
                    Some(value) => value
                        .parse::<u32>()
                        .ok()
                        .filter(|n| (1..=MAX_PAIR_COUNT).contains(n))
                        // Owned copy: the refusal outlives the argument list.
                        .ok_or_else(|| CliError::PairCount(value.clone()))?,
                    None => 1,
                };
                options.pair = Some(count);
            },
            "--bind" => {
                // An empty value would read as "no override" and quietly fall
                // back to the automatic choice: refused instead.
                let value = args
                    .next()
                    .filter(|value| !value.is_empty() && !value.starts_with("--"))
                    .ok_or(CliError::BindValue)?;
                // Owned copy: the options outlive the argument list.
                options.bind = Some(value.clone());
            },
            _ => {},
        }
    }
    Ok(options)
}

/// How many pairing codes to arm at start, `None` for none. Pure.
///
/// A pairing window is opened whenever nobody *can* be paired — a first run,
/// but also a store that was missing, unreadable or malformed, since the token
/// minted in its place has just invalidated every paired client — and
/// otherwise only when the operator asks for one. Arming a code at every
/// restart would leave the one open door ajar for no reason. `--pair <n>` sets
/// the count either way; a fresh token alone arms one.
fn codes_to_arm(cli: &CliOptions, minted_a_new_token: bool) -> Option<u32> {
    cli.pair.or(minted_a_new_token.then_some(1))
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

/// The startup banner for every armed code: one [`pairing_banner`] block per
/// code, numbered `k/n`. With a single code it is exactly [`pairing_banner`].
pub fn pairing_banners(url: &str, name: &str, codes: &[String]) -> String {
    let n = codes.len();
    if n == 1 {
        return codes
            .first()
            .map(|code| pairing_banner(url, name, code))
            .unwrap_or_default();
    }
    codes
        .iter()
        .enumerate()
        .map(|(k, code)| {
            format!(
                "\nPairing code {}/{n}:{}",
                k + 1,
                pairing_banner(url, name, code)
            )
        })
        .collect()
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
        RouterHandle::over_graph(graph_pw::PipeWireGraph::spawn()),
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
/// reaches a teardown — deselecting the last speaker does — destroy
/// the operator's live `blue2th_combined`.
pub fn app_with_auth_store(spotify_auth: SpotifyAuth, auth: AuthStore) -> Router {
    app_with_auth_and_targets(
        spotify_auth,
        SpeakerTargets::new(),
        config::ServerName::new(),
        auth,
        RouterHandle::over_graph(graph_pw::PipeWireGraph::detached()),
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
        RouterHandle::over_graph(graph_pw::PipeWireGraph::detached()),
    )
}

/// Build the router around an explicit Spotify auth driver, an explicit
/// playback selection and an explicit handle onto the audio router, so the
/// on-disk seams and the audio graph stay in the caller's hands.
fn app_with_auth_and_targets(
    mut spotify_auth: SpotifyAuth,
    speaker_targets: SpeakerTargets,
    server_name: config::ServerName,
    auth: AuthStore,
    audio_router: RouterHandle,
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
        router: audio_router,
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
/// Each pass reads the routing generation, then the selection, and sends the
/// two together (#147). A selection that changed while the message waited
/// moves the generation on: the graph thread answers that message outdated
/// without running it, and the change has already woken the next pass, which
/// sends the latest selection. The graph thread never reads the selection
/// itself.
fn spawn_routing_applier(state: AppState) {
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    let mut requests = state.router.routing_requests();
    tokio::spawn(async move {
        while requests.changed().await.is_ok() {
            // Marked seen before the generation is read: a request made from
            // here on wakes another pass, so a stamp it outdates is never the
            // last one sent.
            requests.borrow_and_update();
            let generation = state.router.routing_generation();
            let speakers = state.targets.lock().await.speakers();
            match state.router.apply_selection(&speakers, generation).await {
                Ok(()) => {},
                Err(RouterError::Outdated) => continue,
                Err(e) if speakers.is_empty() => {
                    tracing::warn!("could not tear the combined sink down: {e}");
                    continue;
                },
                Err(e) => {
                    tracing::warn!("could not re-route after a selection change: {e}");
                    continue;
                },
            }
            if speakers.is_empty() {
                continue;
            }
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
/// confirming reload falls due, then run one pass for it. The due time is the
/// one the graph thread publishes after every message (#147): the timer sends
/// nothing to learn it, and with nothing armed it waits for that to change.
fn spawn_confirmation_timer(state: AppState) {
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    let mut published = state.router.confirmation_due();
    tokio::spawn(async move {
        loop {
            let due = *published.borrow_and_update();
            let Some(due) = due else {
                if published.changed().await.is_err() {
                    return;
                }
                continue;
            };
            tokio::time::sleep_until(tokio::time::Instant::from_std(due)).await;
            branch_repair_pass(&state, audio::PassReason::ConfirmationDue).await;
            // A pass that could not take the reload — nothing playing, a graph
            // that cannot be read — leaves it due: wait for a new arming or a
            // gap, rather than spinning on a time already past.
            if *published.borrow() == Some(due) {
                tokio::select! {
                    changed = published.changed() => {
                        if changed.is_err() {
                            return;
                        }
                    },
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
    // One message, one answer (#147): what the pass logs and falls back on is
    // read off it, and nothing else is asked of the graph thread. A message
    // left without an answer — no graph thread, or one that died — is read as
    // a route that failed.
    let router_actor::RepairOutcome {
        routed,
        changed,
        retarget_failed,
    } = match state.router.repair(&speakers).await {
        Ok(outcome) => outcome,
        Err(e) => router_actor::RepairOutcome {
            routed: Err(match e {
                RouterError::Audio(e) => e,
                other => AudioError::PipeWire(other.to_string()),
            }),
            changed: false,
            retarget_failed: false,
        },
    };
    let retargeted_ok = !retarget_failed;
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
    // Snapshot the selection and release the guard before the routing message.
    let speakers = state.targets.lock().await.speakers();
    // Refused here, before anything is sent: with no graph thread at all a
    // message errs before any router sees the empty selection.
    if speakers.is_empty() {
        return Err(AudioError::NoSpeakerConnected.into());
    }
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
/// `/playback`. An empty selection is rejected (4xx). A set superseded by a
/// later one answers 200 with the current playback state (#147).
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
    match state.router.set_sink_volumes(&macs, req.level).await {
        Ok(()) => {
            let mut engine = state.engine.lock().await;
            Ok(Json(engine.set_volume(req.level)?))
        },
        // A later request for the same speakers replaced this one before it
        // started (#147): its level is the one applied, and the one the
        // engine is told. This reply carries the state as it stands.
        Err(RouterError::Superseded) => Ok(Json(state.engine.lock().await.poll_state())),
        Err(e) => Err(e.into()),
    }
}

/// `GET /playback` — current playback state, reconciled so it returns to
/// `Stopped` once the tone ends on its own, and carrying a volume that is true
/// of *every* selected speaker: the live sink level when they all agree (so a
/// change made on a speaker itself is reflected), the commanded level otherwise
/// — see `audio::reported_volume`.
///
/// A read the graph thread did not answer or start in time, a sink list that
/// cannot be read (#145), or a speaker's level read that fails (#148) is not
/// a failed poll: the reply carries the commanded level and says the graph is
/// unresponsive. A listed sink with no level is not a failure.
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
/// The routing is a request: one the graph thread did not start in time, or
/// did not answer within the request's bound, is a 503, while a routing that
/// ran and failed is a failed spawn. `librespot` is spawned here, on the
/// caller's thread, once the routing answered the node to point it at.
async fn start_spotify(
    state: &AppState,
    spotify: &mut SpotifyBackend,
    speakers: &[SpeakerTarget],
) -> Result<SpotifyState, AppError> {
    if spotify.needs_spawn(speakers)? {
        let resolved = match state.router.route_for_spotify(speakers).await {
            Ok(resolved) => resolved,
            Err(RouterError::TimedOut | RouterError::Audio(AudioError::Expired)) => {
                return Err(AppError::graph_not_answering());
            },
            Err(e) => return Err(SpotifyError::Spawn(e.to_string()).into()),
        };
        spotify.spawn_towards(&resolved, speakers)?;
    }
    state.spotify_volume.lock().await.mark_respawned();
    Ok(spotify.status())
}

/// [`start_spotify`] for the background routing applier, whose routing message
/// carries no start deadline and is waited for without a bound: a respawn
/// delayed is better than a `librespot` left stopped.
async fn start_spotify_in_background(
    state: &AppState,
    spotify: &mut SpotifyBackend,
    speakers: &[SpeakerTarget],
) -> Result<SpotifyState, SpotifyError> {
    if spotify.needs_spawn(speakers)? {
        let resolved = state
            .router
            .route_for_spotify_in_background(speakers)
            .await
            .map_err(|e| SpotifyError::Spawn(e.to_string()))?;
        spotify.spawn_towards(&resolved, speakers)?;
    }
    state.spotify_volume.lock().await.mark_respawned();
    Ok(spotify.status())
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
        // Only the off → on edge, never every push: every push carries the
        // whole config, so a rename re-sends `auto_reconnect: true`, and
        // re-arming there would reset a running backoff ladder.
        let resumed = !stored.auto_reconnect() && req.auto_reconnect;
        stored.set_auto_reconnect(req.auto_reconnect);
        // Absent means "not mentioned", never "off": every push carries the
        // whole config, and a client without the field must not undo the guard
        // each time (#58).
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
/// A retune the graph thread did not start in time, or did not answer within
/// the request's bound, is handed to the routing applier (#145, #147): its
/// pass reconciles every branch to the offsets stored by then, so the latest
/// one is applied once the graph answers, and a Spotify sink that moved is
/// resynced there too. Any other failure is the graph's own answer to a
/// retune that ran, and is only logged.
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
        Err(RouterError::TimedOut | RouterError::Audio(AudioError::Expired)) => {
            state.router.request_routing();
        },
        Err(e) => tracing::warn!("could not retune the speaker offset live: {e}"),
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
/// the selection current when its message starts: a change made while the
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

    /// The 503 of an audio graph that did not answer the request: the graph
    /// thread did not answer in time (#145), took the message out of its
    /// queue too late to start it (#146), or started it and the daemon did not
    /// answer before its deadline (#147). One answer for all three — the client
    /// cannot tell them apart, and has nothing different to do.
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
            // The graph did not answer — the message expired unstarted, or the
            // daemon stalled on it (#147): unavailable, not a server fault.
            AudioError::Expired | AudioError::Unanswered => AppError::graph_not_answering(),
        }
    }
}

impl From<RouterError> for AppError {
    fn from(err: RouterError) -> Self {
        match err {
            RouterError::TimedOut => AppError::graph_not_answering(),
            RouterError::Audio(e) => e.into(),
            // Neither reaches a handler as an error: `POST /volume` answers a
            // superseded set 200, and only the applier sends a selection.
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
mod tests;
