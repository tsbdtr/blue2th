// SPDX-License-Identifier: MIT OR Apache-2.0

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
        RouterHandle::over_fake(&graph::fake::FakeGraph::new()),
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

// ---- #160: `--bind`, and `BLUE2TH_BIND` read by debug builds only ----

/// The LAN address the fixtures detect.
fn lan() -> Option<std::net::Ipv4Addr> {
    Some(std::net::Ipv4Addr::new(192, 168, 1, 107))
}

// Criterion: precedence, first step — `--bind` wins over `BLUE2TH_BIND`
// (even in a debug build, where the env var is read) and over the LAN.
#[test]
fn test_bind_address_prefers_the_bind_flag_over_everything() {
    let choice = bind_address(
        Some("192.168.1.20:4000"),
        Some("10.1.2.3:4321"),
        true,
        lan(),
    );
    assert_eq!(choice.addr, "192.168.1.20:4000");
}

// Criterion: precedence, second step — in a debug build, `BLUE2TH_BIND`
// wins over the detected LAN address, with nothing to warn about.
#[test]
fn test_bind_address_debug_build_uses_the_env_override() {
    let choice = bind_address(None, Some("0.0.0.0:4000"), true, lan());
    assert_eq!(
        choice,
        BindChoice {
            addr: "0.0.0.0:4000".to_string(),
            warning: None,
        }
    );
}

// Criterion: precedence, third step — without an override the backend binds
// its LAN address on the standard port, rather than every interface.
#[test]
fn test_bind_address_uses_the_detected_lan_address() {
    for debug in [true, false] {
        assert_eq!(
            bind_address(None, None, debug, lan()).addr,
            format!("192.168.1.107:{DEFAULT_PORT}")
        );
    }
}

// Criterion (non-nominal): with no LAN address resolvable (no interface up)
// the server falls back to `0.0.0.0` rather than refusing to start.
#[test]
fn test_bind_address_falls_back_to_every_interface() {
    for debug in [true, false] {
        assert_eq!(bind_address(None, None, debug, None).addr, DEFAULT_BIND);
    }
}

// Criterion (guard): a release build never reads `BLUE2TH_BIND`. Near-miss:
// the env var holds a valid, bindable address — everything else in the
// precedence would take it; only the build-mode guard sends the release
// build to the LAN address instead.
#[test]
fn test_bind_address_release_build_ignores_the_env_override() {
    assert_eq!(
        bind_address(None, Some("10.1.2.3:4321"), false, lan()).addr,
        format!("192.168.1.107:{DEFAULT_PORT}")
    );
    assert_eq!(
        bind_address(None, Some("10.1.2.3:4321"), false, None).addr,
        DEFAULT_BIND,
        "with no LAN address the release build falls back to the default, not to the env"
    );
}

// Criterion: when `BLUE2TH_BIND` is set on a release build, a warning says
// it is read only by debug builds and names `--bind`.
#[test]
fn test_bind_address_release_build_warns_about_the_ignored_env_var() {
    let warning = bind_address(None, Some("10.1.2.3:4321"), false, lan()).warning;
    assert!(
        warning.is_some(),
        "an ignored BLUE2TH_BIND must be warned about"
    );
    let warning = warning.unwrap_or_default();
    for needle in ["BLUE2TH_BIND", "--bind", "debug"] {
        assert!(
            warning.contains(needle),
            "the warning must mention {needle:?}, got {warning:?}"
        );
    }
}

// Criterion: the warning holds whenever the env var is set on a release
// build — `--bind` winning anyway does not make the ignored setting silent.
#[test]
fn test_bind_address_release_build_warns_even_when_the_bind_flag_wins() {
    let choice = bind_address(
        Some("192.168.1.20:4000"),
        Some("10.1.2.3:4321"),
        false,
        lan(),
    );
    assert_eq!(choice.addr, "192.168.1.20:4000");
    assert!(choice.warning.is_some(), "BLUE2TH_BIND was set and ignored");
}

// Criterion: no spurious warning — a release build with no `BLUE2TH_BIND`
// has nothing to warn about.
#[test]
fn test_bind_address_release_build_without_the_env_var_warns_nothing() {
    assert_eq!(bind_address(None, None, false, lan()).warning, None);
    assert_eq!(
        bind_address(Some("192.168.1.20:4000"), None, false, lan()).warning,
        None
    );
}

// Criterion (empty value): an empty `BLUE2TH_BIND` is unset, as before —
// never an empty bind address.
#[test]
fn test_bind_address_treats_an_empty_env_var_as_unset() {
    assert_eq!(
        bind_address(None, Some(""), true, lan()).addr,
        format!("192.168.1.107:{DEFAULT_PORT}")
    );
}

/// The arguments after the program name, owned as `std::env::args` yields
/// them.
fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|a| (*a).to_string()).collect()
}

// Criterion: no arguments — no extra codes requested, no bind override.
#[test]
fn test_parse_args_without_arguments_requests_nothing() {
    assert_eq!(parse_args(&args(&[])), Ok(CliOptions::default()));
}

// Criterion: unknown arguments are ignored, as they were before #160.
#[test]
fn test_parse_args_ignores_unknown_arguments() {
    assert_eq!(
        parse_args(&args(&["--verbose", "extra"])),
        Ok(CliOptions::default())
    );
}

// Criterion: `--pair` alone arms one code, exactly as today.
#[test]
fn test_parse_args_bare_pair_requests_one_code() {
    assert_eq!(
        parse_args(&args(&["--pair"])),
        Ok(CliOptions {
            pair: Some(1),
            bind: None,
        })
    );
}

// Criterion (guard, "only 1..=10"): both ends of the range are accepted.
// Near-miss of the refusals below.
#[test]
fn test_parse_args_pair_accepts_both_ends_of_the_range() {
    for (value, count) in [("1", 1u32), ("2", 2), ("10", 10)] {
        assert_eq!(
            parse_args(&args(&["--pair", value])),
            Ok(CliOptions {
                pair: Some(count),
                bind: None,
            }),
            "--pair {value}"
        );
    }
}

// Criterion (guard, "only 1..=10"): just outside the range, negative,
// malformed and fractional counts refuse to start. `-1` starts with a dash
// but is not a flag: only `--…` is, so it must not read as a bare `--pair`.
#[test]
fn test_parse_args_pair_refuses_a_count_outside_one_to_ten() {
    for value in ["0", "11", "-1", "abc", "2.5"] {
        let parsed = parse_args(&args(&["--pair", value]));
        assert!(
            matches!(parsed, Err(CliError::PairCount(_))),
            "--pair {value} must refuse to start, got {parsed:?}"
        );
    }
}

// Criterion: the refusal names the accepted range, 1 to 10.
#[test]
fn test_parse_args_pair_refusal_names_the_accepted_range() {
    let message = parse_args(&args(&["--pair", "11"]))
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(
        message.contains("--pair") && message.contains("1 to 10"),
        "the message must name --pair and the range 1 to 10, got {message:?}"
    );
}

// Criterion: `--pair` followed by another flag arms one code, and the flag
// that follows is still read.
#[test]
fn test_parse_args_pair_followed_by_a_flag_requests_one_code() {
    assert_eq!(
        parse_args(&args(&["--pair", "--bind", "192.168.1.20:4000"])),
        Ok(CliOptions {
            pair: Some(1),
            bind: Some("192.168.1.20:4000".to_string()),
        })
    );
}

// Criterion: `--bind host:port` is read, in either order with `--pair n`;
// the count and the address are distinct values, so a swap fails.
#[test]
fn test_parse_args_reads_bind_and_pair_in_either_order() {
    let expected = Ok(CliOptions {
        pair: Some(3),
        bind: Some("192.168.1.20:4000".to_string()),
    });
    assert_eq!(
        parse_args(&args(&["--bind", "192.168.1.20:4000", "--pair", "3"])),
        expected
    );
    assert_eq!(
        parse_args(&args(&["--pair", "3", "--bind", "192.168.1.20:4000"])),
        expected
    );
}

// Criterion (guard): `--bind` with no value, an empty value, or a flag as
// its value refuses to start. Near-miss: `--bind ""` — an empty value
// reads like "no override" everywhere else, and would quietly fall back to
// the LAN address; only the guard turns it into a refusal.
#[test]
fn test_parse_args_bind_refuses_a_missing_empty_or_flag_value() {
    for list in [
        &["--bind"][..],
        &["--bind", ""][..],
        &["--bind", "--pair"][..],
        &["--pair", "2", "--bind"][..],
    ] {
        let parsed = parse_args(&args(list));
        assert_eq!(parsed, Err(CliError::BindValue), "{list:?}");
    }
}

// Criterion: the `--bind` refusal names the flag, so the operator knows
// what to fix.
#[test]
fn test_parse_args_bind_refusal_names_the_flag() {
    let message = parse_args(&args(&["--bind"]))
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(message.contains("--bind"), "got {message:?}");
}

// Criterion: `--pair <n>` arms n codes at start, whether or not the token
// was just minted. Near-miss: a start that parses the count and arms one.
#[test]
fn test_codes_to_arm_follows_the_pair_count() {
    let three = CliOptions {
        pair: Some(3),
        bind: None,
    };
    assert_eq!(codes_to_arm(&three, false), Some(3));
    assert_eq!(codes_to_arm(&three, true), Some(3));
}

// Criterion: with no `--pair`, a freshly minted token arms exactly one code,
// and an already paired backend arms none.
#[test]
fn test_codes_to_arm_without_the_flag_depends_on_a_fresh_token() {
    assert_eq!(codes_to_arm(&CliOptions::default(), true), Some(1));
    assert_eq!(codes_to_arm(&CliOptions::default(), false), None);
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
    let link = blue2th_proto::pair_deep_link("http://192.168.1.107:4000", "blue2th-PC", "K7M2QX");
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
    let link = blue2th_proto::pair_deep_link("http://192.168.1.107:4000", "blue2th-PC", "K7M2QX");
    let other = blue2th_proto::pair_deep_link("http://192.168.1.107:4000", "blue2th-PC", "AAAAAA");
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

// ---- #160: one banner block per armed code ----

const BANNER_URL: &str = "http://192.168.1.107:4000";

// Criterion: with n = 1 the banner is unchanged — exactly the single-code
// banner, with no `1/1` numbering.
#[test]
fn test_pairing_banners_with_one_code_is_the_single_code_banner() {
    let banner = pairing_banners(BANNER_URL, "blue2th-PC", &["K7M2QX".to_string()]);
    assert_eq!(banner, pairing_banner(BANNER_URL, "blue2th-PC", "K7M2QX"));
    assert!(!banner.contains("1/1"), "a single code is not numbered");
}

// Criterion: the banner prints one block per code, numbered `k/n`, each a
// QR of **its own** code's deep link followed by that code. Near-miss: one
// QR followed by every code, which shows the codes and the numbers too.
#[test]
fn test_pairing_banners_prints_one_numbered_block_per_code() {
    let codes = ["K7M2QX", "ABCDEF", "HJKMNP"].map(str::to_string);
    let banner = pairing_banners(BANNER_URL, "blue2th-PC", &codes);

    // Where each code's own QR starts and ends in the banner.
    let qrs: Vec<Option<(usize, usize)>> = codes
        .iter()
        .map(|code| {
            let qr = pairing_qr(&blue2th_proto::pair_deep_link(
                BANNER_URL,
                "blue2th-PC",
                code,
            ));
            banner.find(&qr).map(|at| (at, at + qr.len()))
        })
        .collect();
    assert!(
        qrs.iter().all(Option::is_some),
        "every code needs a QR of its own deep link, got {banner}"
    );
    let qrs: Vec<(usize, usize)> = qrs.into_iter().flatten().collect();
    assert!(
        qrs.windows(2).all(|w| w[0].1 <= w[1].0),
        "the blocks come in order, one after the other"
    );

    let n = codes.len();
    for (k, code) in codes.iter().enumerate() {
        let (qr_start, _) = qrs[k];
        let next_start = qrs.get(k + 1).map_or(banner.len(), |q| q.0);
        let previous_end = if k == 0 { 0 } else { qrs[k - 1].1 };
        let block = &banner[qr_start..next_start];
        assert!(
            block.contains(code.as_str()),
            "{code} must follow its own QR, inside block {}",
            k + 1
        );
        let label = format!("{}/{n}", k + 1);
        assert!(
            banner[previous_end..next_start].contains(&label),
            "block {} must be numbered {label}",
            k + 1
        );
    }
}

// Criterion (security): the QR carries the **code**, never the token — the
// link travels through Android's intent system, which another app declaring
// the `blue2th` scheme could listen to.
#[test]
fn test_pairing_banner_never_prints_the_api_token() {
    let mut store = AuthStore::with_token("super-secret-api-token");
    let codes = store.arm_pairing(2, std::time::SystemTime::now());
    let banner = pairing_banners("http://192.168.1.107:4000", "blue2th-PC", &codes);
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
async fn test_repair_pass_after_the_combined_sink_s_removal_falls_back_when_the_rebuild_fails() {
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
async fn test_repair_pass_after_the_combined_sink_s_removal_falls_back_when_the_retarget_fails() {
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

// Criterion (#147, 2026-10-03): a started message the daemon did not
// answer maps to 503 "the audio graph is not answering", bare or through
// `RouterError::Audio` — the router timeout's own answer, compared whole.
// Guard (only the deadline is 503): the near miss is a `PipeWire` error
// carrying the very text the deadline exit produced before `Unanswered`
// existed. A mapping that matches "did not answer" in the message, or
// that answers every `PipeWire` 503, passes on `Unanswered` alone; this
// one must stay 500 with its own message.
#[test]
fn test_an_unanswered_message_maps_to_503_and_a_graph_failure_saying_so_stays_500() {
    let bare = AppError::from(AudioError::Unanswered);
    let through_router = AppError::from(RouterError::Audio(AudioError::Unanswered));
    let answered = AppError::from(AudioError::PipeWire(
        "PipeWire did not answer a sync round trip".to_string(),
    ));
    let answered_through_router = AppError::from(RouterError::Audio(AudioError::PipeWire(
        "PipeWire did not answer a sync round trip".to_string(),
    )));

    assert_eq!(bare.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(bare.message, GRAPH_NOT_ANSWERING);
    assert_eq!(through_router.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(through_router.message, GRAPH_NOT_ANSWERING);
    assert_eq!(
        bare.message,
        AppError::from(RouterError::TimedOut).message,
        "the same answer as a router that did not answer in time"
    );
    for failed in [&answered, &answered_through_router] {
        assert_eq!(failed.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            failed
                .message
                .contains("PipeWire did not answer a sync round trip"),
            "got {:?}",
            failed.message
        );
    }
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
async fn test_volume_whose_set_was_superseded_answers_200_and_leaves_the_engine_to_the_winner() {
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

// ─── #147 (2026-10-03): a started message the daemon did not answer ────
//
// The actor is free and starts the message at once; the graph call it
// reaches answers `AudioError::Unanswered`, as the sync round trip past
// the message's deadline does in production. Unlike an expiry, the
// message ran: the graph log shows what it sent before the stall.

// Criterion (#147, 2026-10-03): `POST /volume` whose set reaches the
// graph and answers `Unanswered` answers 503 "the audio graph is not
// answering", and the engine's commanded level is left unchanged.
// Guard (only the deadline is 503): the near miss is the same set failed
// with the fake's usual `PipeWire("… told to fail")`, a graph that
// answered an error: it stays 500 with its own message. Each half runs
// on its own graph, with its own level, so the two attempts are told
// apart in the logs.
#[tokio::test]
async fn test_volume_whose_set_went_unanswered_answers_503_and_a_refused_set_stays_500() {
    use graph::fake::{FakeGraph, GraphCall, GraphOp};

    let stalled = FakeGraph::with_sinks(&[JBL_SINK]);
    stalled.fail_unanswered(GraphOp::SetSinkVolume);
    let stalled_state = selected_state(&stalled, &[JBL], &[JBL]).await;
    let refused = FakeGraph::with_sinks(&[JBL_SINK]);
    refused.fail(GraphOp::SetSinkVolume);
    let refused_state = selected_state(&refused, &[JBL], &[JBL]).await;

    let unanswered = volume(
        State(stalled_state.clone()),
        Json(VolumeRequest { level: 0.8 }),
    )
    .await;
    let failed = volume(
        State(refused_state.clone()),
        Json(VolumeRequest { level: 0.6 }),
    )
    .await;

    assert_eq!(
        failure(&unanswered),
        Some((
            StatusCode::SERVICE_UNAVAILABLE,
            GRAPH_NOT_ANSWERING.to_string()
        ))
    );
    assert_eq!(
        stalled.calls(),
        vec![GraphCall::SetSinkVolume {
            sink: JBL_SINK.to_string(),
            level: 0.8
        }],
        "the set reached the graph before it stalled"
    );
    assert_eq!(
        stalled_state.engine.lock().await.poll_state().volume,
        COMMANDED
    );

    let failed = failure(&failed);
    assert_eq!(
        failed.as_ref().map(|(status, _)| *status),
        Some(StatusCode::INTERNAL_SERVER_ERROR),
        "a graph that answered an error is not a graph that did not answer"
    );
    assert!(
        failed
            .as_ref()
            .is_some_and(|(_, message)| message.contains("SetSinkVolume told to fail")),
        "got {failed:?}"
    );
    assert_eq!(
        refused.calls(),
        vec![GraphCall::SetSinkVolume {
            sink: JBL_SINK.to_string(),
            level: 0.6
        }]
    );
    assert_eq!(
        refused_state.engine.lock().await.poll_state().volume,
        COMMANDED
    );
}

// Criterion (#147, 2026-10-03): `POST /play` whose route answers
// `Unanswered` answers 503 "the audio graph is not answering", and the
// engine does not start. The stall is the combined sink's creation, a
// step whose error the route propagates as it is: the route ran up to
// it — the stale default checked, the old sink torn down — and stopped
// there, so this 503 does not mean "nothing was sent".
#[tokio::test]
async fn test_play_whose_route_went_unanswered_answers_503_and_leaves_the_engine_not_playing() {
    use graph::fake::{FakeGraph, GraphCall, GraphOp};

    let fake = FakeGraph::with_sinks(&[JBL_SINK]);
    fake.fail_unanswered(GraphOp::CreateCombinedSink);
    let state = selected_state(&fake, &[JBL], &[JBL]).await;

    let answer = play(State(state.clone())).await;

    assert_eq!(
        failure(&answer),
        Some((
            StatusCode::SERVICE_UNAVAILABLE,
            GRAPH_NOT_ANSWERING.to_string()
        ))
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
        ],
        "the route ran until the stall, and no further"
    );
    assert_ne!(
        state.engine.lock().await.poll_state().status,
        PlaybackStatus::Playing,
        "a 503 starts no tone"
    );
}

// Criterion (#152): `POST /play` over a graph whose reconciliation's first
// sink-list read is unanswered answers 503 "the audio graph is not
// answering" — it answered 200 in 0.41 s on a frozen daemon (#147's
// budget test) — and starts no tone. The combined sink is up with a
// branch into each speaker and only the JBL is selected, so a pass that
// acted on the list would unload the Sony's branch: no mutating call
// reaches the graph.
#[tokio::test]
async fn test_play_whose_reconcile_read_went_unanswered_answers_503_and_changes_nothing() {
    use graph::fake::{FakeGraph, GraphCall, GraphOp};

    let fake = FakeGraph::with_sinks(&[JBL_SINK, SONY_SINK, COMBINED_SINK]);
    fake.seed_branch(COMBINED_SINK, JBL_SINK, 0, Some(true));
    fake.seed_branch(COMBINED_SINK, SONY_SINK, 0, Some(true));
    let state = selected_state(&fake, &[JBL, SONY], &[JBL]).await;
    fake.fail_unanswered_after(GraphOp::Sinks, 1);

    let answer = play(State(state.clone())).await;

    assert_eq!(
        failure(&answer),
        Some((
            StatusCode::SERVICE_UNAVAILABLE,
            GRAPH_NOT_ANSWERING.to_string()
        ))
    );
    assert_eq!(fake.calls(), Vec::<GraphCall>::new());
    assert_eq!(fake.loaded(COMBINED_SINK).len(), 2, "no branch unloaded");
    assert_ne!(
        state.engine.lock().await.poll_state().status,
        PlaybackStatus::Playing,
        "a 503 starts no tone"
    );
}

// Criterion (#152): `POST /play` whose route stalls while resolving a
// branch's sink — a build from nothing, the JBL's resolution answered and
// its branch loaded, the Sony's resolution unanswered — answers 503 "the
// audio graph is not answering", where the stall read as "no PipeWire
// sink for prefix" and answered 500.
#[tokio::test]
async fn test_play_whose_branch_resolution_went_unanswered_answers_503() {
    use graph::fake::{FakeGraph, GraphOp};

    let fake = FakeGraph::with_sinks(&[JBL_SINK, SONY_SINK]);
    let state = selected_state(&fake, &[JBL, SONY], &[JBL, SONY]).await;
    fake.fail_unanswered_after(GraphOp::Sinks, 2);

    let answer = play(State(state.clone())).await;

    assert_eq!(
        failure(&answer),
        Some((
            StatusCode::SERVICE_UNAVAILABLE,
            GRAPH_NOT_ANSWERING.to_string()
        ))
    );
    let loaded: Vec<String> = fake
        .loaded(COMBINED_SINK)
        .into_iter()
        .map(|b| b.branch.sink)
        .collect();
    assert_eq!(loaded, vec![JBL_SINK.to_string()], "the JBL loaded first");
    assert_ne!(
        state.engine.lock().await.poll_state().status,
        PlaybackStatus::Playing,
        "a 503 starts no tone"
    );
}

// Criterion (#147, 2026-10-03): a route whose branch load answers
// `Unanswered` — the combined sink already in place, the Sony's load
// stalling after the JBL's went through — makes `POST /play` answer 503
// "the audio graph is not answering", and the JBL's branch is still
// recorded as loaded: in the graph, and armed for its confirming reload,
// which the actor publishes. The engine does not start.
// Guard (a branch pass is `Unanswered` only when a failure is): the near
// miss is the same load refused with the fake's usual
// `PipeWire("… told to fail")`, on a graph of its own, which must stay
// 500 with that refusal in its message.
#[tokio::test]
async fn test_play_whose_branch_load_went_unanswered_answers_503_and_keeps_the_branch_loaded_before_it(
) {
    use graph::fake::{FakeGraph, GraphCall, GraphOp};

    let load = |real_sink: &str| GraphCall::LoadBranch {
        sink_name: COMBINED_SINK.to_string(),
        real_sink: real_sink.to_string(),
        latency_ms: 0,
    };

    let stalled = FakeGraph::with_sinks(&[JBL_SINK, SONY_SINK, COMBINED_SINK]);
    stalled.fail_unanswered_for(GraphOp::LoadBranch, SONY_SINK);
    let stalled_state = selected_state(&stalled, &[JBL, SONY], &[JBL, SONY]).await;
    let refused = FakeGraph::with_sinks(&[JBL_SINK, SONY_SINK, COMBINED_SINK]);
    refused.fail_for(GraphOp::LoadBranch, SONY_SINK);
    let refused_state = selected_state(&refused, &[JBL, SONY], &[JBL, SONY]).await;

    let unanswered = play(State(stalled_state.clone())).await;
    let failed = play(State(refused_state.clone())).await;
    settle().await;

    assert_eq!(
        failure(&unanswered),
        Some((
            StatusCode::SERVICE_UNAVAILABLE,
            GRAPH_NOT_ANSWERING.to_string()
        ))
    );
    assert_eq!(
        stalled.routing_calls(),
        vec![load(JBL_SINK), load(SONY_SINK)],
        "the pass reached the Sony's load, past the JBL's"
    );
    let loaded: Vec<String> = stalled
        .loaded(COMBINED_SINK)
        .into_iter()
        .map(|b| b.branch.sink)
        .collect();
    assert_eq!(loaded, vec![JBL_SINK.to_string()]);
    assert!(
        stalled_state.router.confirmation_due().borrow().is_some(),
        "the JBL's load is recorded: its confirming reload is armed"
    );
    assert_ne!(
        stalled_state.engine.lock().await.poll_state().status,
        PlaybackStatus::Playing,
        "a 503 starts no tone"
    );

    let failed = failure(&failed);
    assert_eq!(
        failed.as_ref().map(|(status, _)| *status),
        Some(StatusCode::INTERNAL_SERVER_ERROR),
        "a refused load is an answer, not a graph that did not answer"
    );
    assert!(
        failed
            .as_ref()
            .is_some_and(|(_, message)| message.contains("LoadBranch told to fail")),
        "got {failed:?}"
    );
    assert_eq!(
        refused.routing_calls(),
        vec![load(JBL_SINK), load(SONY_SINK)]
    );
}

// Criterion (#147, 2026-10-03): `POST /devices/{addr}/offset` whose
// retune answers `Unanswered` logs it and does not call
// `request_routing`: the delay may have been set, so it is not "nothing
// was done". The handler still answers 200 with the stored offset.
// Guard (`Unanswered` is not "nothing was done"): the near miss is a
// retune that answers `Expired` — held past the start budget — which
// must call `request_routing`, beside the unanswered one that must not.
// An arm widened to the new variant re-routes after the stall: the
// routing generation moves and the applier runs a second pass.
#[tokio::test(start_paused = true)]
async fn test_an_offset_change_whose_retune_went_unanswered_is_not_handed_to_the_applier_unlike_an_expired_one(
) {
    use graph::fake::{FakeGraph, GraphCall, GraphOp};

    // The unanswered retune.
    let stalled = FakeGraph::with_sinks(&[JBL_SINK, SONY_SINK, COMBINED_SINK]);
    stalled.seed_branch(COMBINED_SINK, JBL_SINK, 0, Some(true));
    let stalled_branch = stalled.seed_branch(COMBINED_SINK, SONY_SINK, 0, Some(true));
    stalled.fail_unanswered(GraphOp::SetBranchDelay);
    let stalled_state = selected_state(&stalled, &[JBL, SONY], &[JBL, SONY]).await;
    start_applier(&stalled_state, &stalled).await;
    let generation = stalled_state.router.routing_generation();

    let Json(reply) = set_target_offset(
        State(stalled_state.clone()),
        Path(SONY.to_string()),
        Json(OffsetRequest { offset_ms: 120 }),
    )
    .await;
    settle().await;

    assert_eq!(offset_of(&reply, SONY), Some(120));
    assert_eq!(
        stalled.calls(),
        vec![GraphCall::SetBranchDelay {
            id: stalled_branch,
            delay_ms: 120
        }],
        "the retune's one unanswered attempt, and no applier pass after it"
    );
    assert_eq!(
        stalled
            .loaded(COMBINED_SINK)
            .iter()
            .find(|b| b.id == stalled_branch)
            .map(|b| b.branch.latency_ms),
        Some(0),
        "the fake did answer `Unanswered`: it applied nothing"
    );
    assert_eq!(passes(&stalled), 1, "calls: {:?}", stalled.all_calls());
    assert_eq!(stalled_state.router.routing_generation(), generation);

    // The near miss: the expired retune, on a graph of its own.
    let expired = FakeGraph::with_sinks(&[JBL_SINK, SONY_SINK, COMBINED_SINK]);
    expired.seed_branch(COMBINED_SINK, JBL_SINK, 0, Some(true));
    let expired_branch = expired.seed_branch(COMBINED_SINK, SONY_SINK, 0, Some(true));
    let expired_state = selected_state(&expired, &[JBL, SONY], &[JBL, SONY]).await;
    start_applier(&expired_state, &expired).await;
    let generation = expired_state.router.routing_generation();
    let held = expired_state.router.hold_actor();

    let request = tokio::spawn(set_target_offset(
        State(expired_state.clone()),
        Path(SONY.to_string()),
        Json(OffsetRequest { offset_ms: 120 }),
    ));
    settle().await;
    tokio::time::advance(Duration::from_millis(301)).await;
    settle().await;
    drop(held);
    settle().await;

    assert!(request.is_finished(), "the expiry is answered at once");
    let Json(reply) = request.await.expect("the handler task ends");
    assert_eq!(offset_of(&reply, SONY), Some(120));
    assert_ne!(
        expired_state.router.routing_generation(),
        generation,
        "control: an expired retune is handed to the applier"
    );
    assert_eq!(
        expired.calls(),
        vec![GraphCall::SetBranchDelay {
            id: expired_branch,
            delay_ms: 120
        }],
        "control: the applier applied the stored offset, once"
    );
}

// Criterion (#147, 2026-10-03): `POST /spotify/start` whose routing
// answers `Unanswered` keeps `SpotifyError::Spawn`'s 500 — the explicit
// `match` in `start_spotify` is left as it is — and `librespot` is not
// spawned: the route stopped at the stall, before any node name was
// resolved. Guard (`Unanswered` is not "nothing was done"): the near
// miss is a routing that expired behind a held actor, which answers 503;
// an arm widened to the new variant answers the unanswered one 503 too.
// The message is compared whole, built from the same errors, so the
// spawn's own "librespot not found" — what a start whose routing
// succeeded reaches under test — cannot pass for it.
#[tokio::test(start_paused = true)]
async fn test_spotify_start_whose_routing_went_unanswered_keeps_the_spawn_failure_unlike_an_expired_one(
) {
    use graph::fake::{FakeGraph, GraphCall, GraphOp};

    // The near miss: the expired routing.
    let expired = FakeGraph::with_sinks(&[JBL_SINK]);
    let expired_state = selected_state(&expired, &[JBL], &[JBL]).await;
    let held = expired_state.router.hold_actor();
    let request = tokio::spawn(spotify_start(State(expired_state.clone())));
    settle().await;
    tokio::time::advance(Duration::from_millis(301)).await;
    settle().await;
    drop(held);
    settle().await;
    assert!(request.is_finished(), "the expiry is answered at once");
    let answer = request.await.expect("the handler task ends");
    assert_eq!(
        failure(&answer),
        Some((
            StatusCode::SERVICE_UNAVAILABLE,
            GRAPH_NOT_ANSWERING.to_string()
        )),
        "control: an expired routing is the 503"
    );

    // The unanswered routing.
    let stalled = FakeGraph::with_sinks(&[JBL_SINK]);
    stalled.fail_unanswered(GraphOp::CreateCombinedSink);
    let stalled_state = selected_state(&stalled, &[JBL], &[JBL]).await;

    let answer = spotify_start(State(stalled_state.clone())).await;

    let spawn_failure = AppError::from(SpotifyError::Spawn(
        RouterError::Audio(AudioError::Unanswered).to_string(),
    ));
    assert_eq!(
        failure(&answer),
        Some((StatusCode::INTERNAL_SERVER_ERROR, spawn_failure.message))
    );
    assert_eq!(
        stalled.all_calls().last(),
        Some(&GraphCall::CreateCombinedSink {
            sink_name: COMBINED_SINK.to_string()
        }),
        "the route stopped at the stall, and the target was never resolved: {:?}",
        stalled.all_calls()
    );
    let mut spotify = stalled_state.spotify.lock().await;
    assert_eq!(spotify.poll_liveness().status, SpotifyStatus::Stopped);
    assert_eq!(spotify.current_sink(), None);
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
    let pass =
        tokio::spawn(
            async move { branch_repair_pass(&repairing, audio::PassReason::SafetyNet).await },
        );
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

// Criterion (#147): a repair message left without an answer — no graph
// thread at all, or one that dropped the message — is read as a route
// that failed, as a router that could not reach the daemon answered
// before: woken by the combined sink's removal, the pass falls back to
// the pause. The near miss is the scripted answer of a route that went
// through, which `test_repair_pass_sends_one_repair_message_and_falls_back_from_its_one_answer`
// shows does not fall back; a fallback outcome read as `Ok` makes both
// routers here answer `false`.
#[tokio::test]
async fn test_repair_pass_whose_message_got_no_answer_falls_back_as_a_failed_route() {
    let routers = [
        (
            "no graph thread",
            RouterHandle::over(
                Box::new(|_envelope: router_actor::Envelope| {
                    Err(AudioError::PipeWire(
                        "the PipeWire graph thread is not running".to_string(),
                    ))
                }),
                router_actor::Shared::new(),
            ),
        ),
        (
            "a dropped message",
            RouterHandle::over(
                Box::new(|envelope: router_actor::Envelope| {
                    drop(envelope);
                    Ok(())
                }),
                router_actor::Shared::new(),
            ),
        ),
    ];
    for (label, router) in routers {
        let fake = graph::fake::FakeGraph::new();
        let mut state = vanished_state(&fake, true).await;
        state.router = router;

        let fell_back = branch_repair_pass(&state, combined_reason()).await;

        assert!(fell_back, "{label}: an unanswered repair falls back");
        assert_tone_untouched_and_nothing_claimed(&state).await;
    }
}

// Guard (#147): the applier hands Spotify nothing after it tore the
// combined sink down for an empty selection — the lock-holding applier
// ended its pass on the teardown. Here `librespot` runs (a `sleep`
// stands for it) while nothing is selected: the applier sends the empty
// selection and nothing else, and the backend keeps running. The near
// miss is a resync run on the empty selection, which finds a backend
// feeding no sink it wants, stops it, and cannot start it again.
#[tokio::test(start_paused = true)]
async fn test_the_applier_on_an_empty_selection_tears_down_and_leaves_spotify_alone() {
    let fake = graph::fake::FakeGraph::with_sinks(&[JBL_SINK]);
    let state = test_state_on(AudioEngine::new(), &fake);

    let (sent, spotify_status) = one_applier_pass_beside_a_running_spotify(state, || Ok(())).await;

    assert_eq!(sent, vec![("ApplySelection [] 1".to_string(), None)]);
    assert_eq!(spotify_status, SpotifyStatus::Running);
}

// Guard (#147): a selection the graph thread answered outdated hands
// Spotify nothing — the snapshot it carried is no longer the
// selection, and the request that outdated it has already woken the
// pass that resyncs from the latest one. Here `librespot` runs, fed no
// sink of this selection: the near miss is a resync run on the outdated
// snapshot, which stops it and sends a Spotify route for that snapshot.
#[tokio::test(start_paused = true)]
async fn test_the_applier_answered_outdated_leaves_spotify_to_the_next_pass() {
    let fake = graph::fake::FakeGraph::with_sinks(&[JBL_SINK]);
    let state = selected_state(&fake, &[JBL], &[JBL]).await;

    let (sent, spotify_status) =
        one_applier_pass_beside_a_running_spotify(state, || Err(RouterError::Outdated)).await;

    assert_eq!(sent, vec![(format!("ApplySelection [{JBL}@0] 1"), None)]);
    assert_eq!(spotify_status, SpotifyStatus::Running);
}

/// Run one routing-applier pass on `state`, beside a running `librespot`
/// (a `sleep` stands for it), over a router that answers the selection
/// with `answer` and nothing else. Returns what the router was sent and
/// the backend's status once the pass is done; the backend is stopped
/// before returning.
async fn one_applier_pass_beside_a_running_spotify(
    mut state: AppState,
    answer: fn() -> Result<(), RouterError>,
) -> (Vec<(String, Option<std::time::Instant>)>, SpotifyStatus) {
    let received: Received = Arc::default();
    let log = Arc::clone(&received);
    state.router = RouterHandle::over(
        Box::new(move |envelope: router_actor::Envelope| {
            log.lock().unwrap().push((
                router_actor::testing::describe(&envelope.message),
                envelope.start_by,
            ));
            if let router_actor::Message::ApplySelection { reply, .. } = envelope.message {
                let _ = reply.send(answer());
            }
            Ok(())
        }),
        router_actor::Shared::new(),
    );
    let child =
        spotify::spawn_bound_to_this_thread("sleep", &["30".to_string()]).expect("spawn sleep");
    state.spotify.lock().await.adopt_child_for_test(child);
    spawn_routing_applier(state.clone());
    settle().await;

    state.router.request_routing();
    settle().await;

    let status = state.spotify.lock().await.poll_liveness().status;
    let _ = state.spotify.lock().await.stop();
    // Cloned out of the lock, as a snapshot.
    let sent = received.lock().unwrap().clone();
    (sent, status)
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
    let pass =
        tokio::spawn(
            async move { branch_repair_pass(&repairing, audio::PassReason::SafetyNet).await },
        );
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
