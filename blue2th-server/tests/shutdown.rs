// SPDX-License-Identifier: MIT OR Apache-2.0

//! Integration tests for #122: the server takes its `librespot` child down with
//! it. Two mechanisms are pinned here — the SIGINT/SIGTERM future `run()` races
//! the serve future against, and the shutdown stop that goes through the same
//! `SpotifyBackend::stop` as `POST /spotify/stop`. The parent-death signal on
//! the child is a unit test in `spotify.rs`.
//!
//! A signal cannot be delivered inside the shared test process: with no handler
//! installed SIGTERM's default action would kill every other test with it, and
//! with one installed, the tests would race for the same handler. So the tests
//! that need a signal re-exec **this binary** on a single test name, with an
//! environment marker that flips that test into its *probe* role: install the
//! future, signal itself, print a marker once the future resolved. The outer
//! role asserts a clean exit and the marker on stdout. `current_exe()` is used
//! because `CARGO_BIN_EXE_*` is not defined for test binaries.
//!
//! `run()` itself (socket, stores, mDNS) and the log line are manual seams.

use std::{process::Stdio, time::Duration};

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use blue2th_proto::SpotifyStatus;
use blue2th_server::{auth::AuthStore, spotify_auth::SpotifyAuth, AppState};
use nix::{
    sys::signal::{kill, Signal},
    unistd::Pid,
};
use tower::ServiceExt; // for `oneshot`

/// The API token these tests pair with. Held in memory only: no test may build
/// the router through `app()`, which reloads — and, on a malformed store,
/// rotates — the operator's real API token.
const TOKEN: &str = "test-api-token";

/// Set in the environment of a re-exec'd copy of this binary to flip a test
/// into its probe role.
const PROBE_ENV: &str = "BLUE2TH_SHUTDOWN_PROBE";

/// Where the fake `pactl` a probe may find on `PATH` records its invocations.
const PACTL_LOG_ENV: &str = "BLUE2TH_PACTL_LOG";

/// Printed by a probe once it got past the behaviour under test — the proof the
/// path ran, so an empty observation is never mistaken for a passing one.
const MARKER: &str = "blue2th-shutdown-probe: done";

/// The router and the state it was built around: store-free, known token.
fn build_app_and_state() -> (axum::Router, AppState) {
    blue2th_server::app_with_auth_store_and_state(
        SpotifyAuth::with_config(None, "blue2th://spotify-callback".to_string()),
        AuthStore::with_token(TOKEN),
    )
}

/// Add the bearer every guarded route requires.
fn authorized(builder: axum::http::request::Builder) -> axum::http::request::Builder {
    builder.header("authorization", format!("Bearer {TOKEN}"))
}

fn in_probe_role() -> bool {
    std::env::var_os(PROBE_ENV).is_some()
}

/// Run `test_name` in a fresh copy of this test binary, in its probe role, with
/// `extra_env` on top of the inherited environment. Bounded, so a probe that
/// hangs on a future that never resolves fails instead of stalling the suite.
async fn run_probe(
    test_name: &str,
    extra_env: &[(&str, String)],
) -> std::io::Result<std::process::Output> {
    let exe = std::env::current_exe()?;
    let mut command = tokio::process::Command::new(exe);
    command
        .arg(test_name)
        .args(["--exact", "--nocapture", "--test-threads=1"])
        .env(PROBE_ENV, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in extra_env {
        command.env(key, value);
    }
    tokio::time::timeout(Duration::from_secs(10), command.output())
        .await
        .map_err(|_| std::io::Error::other("the probe did not finish within 10 s"))?
}

/// The outer role's common checks: the probe exited cleanly *and* reached the
/// marker. Returns the probe's stdout for any further assertion.
fn assert_probe_completed(output: &std::process::Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "the probe did not exit cleanly ({:?}); a signal in the status means the future never took it\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
    assert!(
        stdout.contains(MARKER),
        "the probe exited 0 without reaching the marker\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    stdout
}

/// Probe body for the signal tests: arm `shutdown_signal()`, deliver `signal`
/// to this very process once the future has been polled, and report whether it
/// resolved within 2 s. The caller prints the marker on `Ok`.
async fn probe_shutdown_signal(signal: Signal) -> Result<(), tokio::time::error::Elapsed> {
    let shutdown = blue2th_server::shutdown_signal();
    tokio::pin!(shutdown);
    // Sent from a task that yields first: on this single-threaded runtime the
    // future below is polled — and its handlers registered — before the sleep
    // ends. Delivered earlier, the signal takes its default action and the
    // probe simply dies, which the outer role reports as "never took it". A
    // `kill` that fails surfaces the same way, as the timeout below.
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = kill(Pid::this(), signal);
    });
    tokio::time::timeout(Duration::from_secs(2), shutdown).await
}

// Criterion (#122): `shutdown_signal()` resolves on SIGTERM — `kill`, systemd.
#[tokio::test]
async fn test_shutdown_signal_resolves_on_sigterm() {
    if in_probe_role() {
        probe_shutdown_signal(Signal::SIGTERM)
            .await
            .expect("shutdown_signal() resolved within 2 s of SIGTERM");
        println!("{MARKER}");
        return;
    }
    let output = run_probe("test_shutdown_signal_resolves_on_sigterm", &[])
        .await
        .expect("run the probe");
    assert_probe_completed(&output);
}

// Criterion (#122): `shutdown_signal()` resolves on SIGINT — Ctrl-C in the
// terminal that runs the server.
#[tokio::test]
async fn test_shutdown_signal_resolves_on_sigint() {
    if in_probe_role() {
        probe_shutdown_signal(Signal::SIGINT)
            .await
            .expect("shutdown_signal() resolved within 2 s of SIGINT");
        println!("{MARKER}");
        return;
    }
    let output = run_probe("test_shutdown_signal_resolves_on_sigint", &[])
        .await
        .expect("run the probe");
    assert_probe_completed(&output);
}

// Criterion (#122): `stop_sources_for_shutdown` on a backend that is already
// `Stopped` is a no-op returning `Stopped` — idempotent, never an error, so a
// second Ctrl-C during the teardown has nothing to break.
#[tokio::test]
async fn test_stop_sources_for_shutdown_on_a_stopped_backend_is_a_no_op() {
    let (_router, state) = build_app_and_state();

    let first = blue2th_server::stop_sources_for_shutdown(&state).await;
    assert_eq!(first.status, SpotifyStatus::Stopped);

    let second = blue2th_server::stop_sources_for_shutdown(&state).await;
    assert_eq!(second.status, SpotifyStatus::Stopped);
    assert_eq!(
        second.device_name, first.device_name,
        "a no-op must report the same state twice"
    );
}

// Criterion (#122): the state handed to the shutdown path holds the *same*
// `SpotifyBackend` the router does — a rename pushed through `POST /config` is
// what the reconciled shutdown state reports, so `stop()` acts on the child the
// routes started rather than on a fresh, empty backend.
#[tokio::test]
async fn test_stop_sources_for_shutdown_reports_the_name_the_router_configured() {
    let (router, state) = build_app_and_state();

    let request = authorized(Request::builder())
        .method("POST")
        .uri("/config")
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"name":"Lpt","restore_during_playback":true,"auto_reconnect":true}"#,
        ))
        .expect("build request");
    let response = router.oneshot(request).await.expect("router response");
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the rename was not accepted"
    );

    let reconciled = blue2th_server::stop_sources_for_shutdown(&state).await;
    assert_eq!(reconciled.status, SpotifyStatus::Stopped);
    assert_eq!(
        reconciled.device_name, "Lpt",
        "the shutdown path reads a different backend than the routes write"
    );
}

/// The one `pactl` line the probe writes itself, *after* the shutdown path:
/// it proves the fake on `PATH` is the one being resolved and that it logs, so
/// an empty log can only mean a broken fixture — never a passing test.
const PACTL_SENTINEL: &str = "--blue2th-fixture-check";

// Criterion (#122): the PipeWire graph is *not* torn down on shutdown — the
// combined sink and its loopbacks are what keep the speakers routed across a
// restart, and a dead `librespot` feeds them nothing. Observed through a fake
// `pactl` placed first on the probe's `PATH`, which logs every invocation: the
// shutdown path must produce none (`teardown_combined` would list, then unload),
// so the log holds exactly the sentinel the probe appends afterwards.
#[tokio::test]
async fn test_stop_sources_for_shutdown_leaves_the_pipewire_graph_alone() {
    if in_probe_role() {
        let (_router, state) = build_app_and_state();
        let _ = blue2th_server::stop_sources_for_shutdown(&state).await;
        let sentinel = std::process::Command::new("pactl")
            .arg(PACTL_SENTINEL)
            .status()
            .expect("run the fake pactl from PATH");
        assert!(sentinel.success(), "the fake pactl failed: {sentinel:?}");
        println!("{MARKER}");
        return;
    }

    let dir = std::env::temp_dir().join(format!("blue2th-shutdown-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the fake pactl directory");
    let fake_pactl = dir.join("pactl");
    std::fs::write(
        &fake_pactl,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$BLUE2TH_PACTL_LOG\"\n",
    )
    .expect("write the fake pactl");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_pactl, std::fs::Permissions::from_mode(0o755))
            .expect("make the fake pactl executable");
    }
    let log = dir.join("pactl.log");
    let path = format!(
        "{}:{}",
        dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let output = run_probe(
        "test_stop_sources_for_shutdown_leaves_the_pipewire_graph_alone",
        &[("PATH", path), (PACTL_LOG_ENV, log.display().to_string())],
    )
    .await
    .expect("run the probe");
    let calls = std::fs::read_to_string(&log).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);

    assert_probe_completed(&output);
    assert_eq!(
        calls.trim(),
        PACTL_SENTINEL,
        "expected only the probe's sentinel in the pactl log; anything before it is the shutdown path touching the PipeWire graph"
    );
}
