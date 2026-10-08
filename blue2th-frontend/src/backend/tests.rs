// SPDX-License-Identifier: MIT OR Apache-2.0

use super::*;

/// The settings cache is process-wide, so a test that replaces it and one
/// that asserts "nothing is configured" cannot run at the same time: the
/// first one's active backend leaks into the second one's assertion. Tests
/// touching the cache take this lock and leave it empty behind them.
/// Async-aware on purpose: these tests hold the guard across `await`s, which
/// a `std::sync::Mutex` must never do.
static SETTINGS_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[test]
fn test_health_url_appends_path() {
    assert_eq!(
        health_url("http://10.0.0.5:4000"),
        "http://10.0.0.5:4000/health"
    );
}

#[test]
fn test_health_url_tolerates_trailing_slash() {
    assert_eq!(
        health_url("http://10.0.0.5:4000/"),
        "http://10.0.0.5:4000/health"
    );
}

/// Settings holding a single active backend at `url`.
fn active_at(url: &str) -> AppSettings {
    let mut settings = AppSettings::default();
    settings.add("Salon", url).expect("add the test backend");
    settings.activate(0).expect("activate the test backend");
    settings
}

// Criterion (phase 6.2): `backend_base_url()` resolves at runtime from the
// settings — the active entry's URL, with no compile-time value involved.
#[test]
fn test_base_url_from_settings_returns_the_active_backend_url() {
    let settings = active_at("http://192.168.1.107:4000");
    assert_eq!(
        base_url_from(&settings).map_err(|e| e.to_string()),
        Ok("http://192.168.1.107:4000".to_string())
    );
}

// Criterion (phase 6.2): with nothing configured the lookup fails fast with a
// "no backend configured" error — there is no fallback address, not even
// localhost, so no request can be built at all.
#[test]
fn test_base_url_from_settings_without_a_backend_is_an_error() {
    let error = base_url_from(&AppSettings::default())
        .expect_err("an unconfigured app must have no address");
    assert!(
        error.to_string().contains(NO_BACKEND_CONFIGURED),
        "the error must name the missing configuration, got {error}"
    );
}

// Criterion (phase 6.2): a configured but inactive backend is still no
// address — the app only knows the backend the user activated.
#[test]
fn test_base_url_from_settings_without_an_active_backend_is_an_error() {
    let mut settings = AppSettings::default();
    settings
        .add("Salon", "http://192.168.1.107:4000")
        .expect("add a backend without activating it");
    assert!(base_url_from(&settings).is_err());
}

// Criterion (phase 6.2): every `backend.rs` call goes through the runtime
// lookup — with nothing configured a call fails fast with the "no backend
// configured" error rather than performing a request (and timing out).
#[tokio::test]
async fn test_ping_backend_without_a_configured_backend_fails_fast() {
    let _guard = SETTINGS_GUARD.lock().await;
    crate::settings::set_current(AppSettings::default());

    let started = std::time::Instant::now();
    let error = ping_backend()
        .await
        .expect_err("an unconfigured app must not reach any backend");
    assert!(
        error.to_string().contains(NO_BACKEND_CONFIGURED),
        "expected a configuration error, got {error}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "no request may be attempted, so the call must return immediately"
    );
}

// Criterion (phase 6.2): the app pushes the name over `POST /config` — the
// client builds the `/config` URL, tolerating a trailing slash on the base.
#[test]
fn test_config_url_appends_path() {
    assert_eq!(
        config_url("http://10.0.0.5:4000"),
        "http://10.0.0.5:4000/config"
    );
    assert_eq!(
        config_url("http://10.0.0.5:4000/"),
        "http://10.0.0.5:4000/config"
    );
}

// Criterion (phase 6.2): switching backends repoints the app even when the
// name push fails (the new backend is unreachable) and even when pausing the
// previous one fails — often *why* the user is switching. The failure is
// surfaced, never blocking: the app must never be stuck on a dead backend.
#[tokio::test]
async fn test_activate_backend_switches_locally_even_when_the_push_fails() {
    let _guard = SETTINGS_GUARD.lock().await;

    let mut settings = AppSettings::default();
    // Port 1 is never listening: both the pause and the push are refused.
    settings
        .add("Salon", "http://127.0.0.1:1")
        .expect("add Salon");
    settings
        .add("Bureau", "http://127.0.0.1:2")
        .expect("add Bureau");
    settings.activate(0).expect("start on Salon");

    let outcome = activate_backend(&mut settings, 1).await;
    assert!(
        outcome.is_err(),
        "an unreachable backend must surface the failure to the toast"
    );
    assert_eq!(
        settings.active_backend().map(|b| b.name.as_str()),
        Some("Bureau"),
        "the switch must still happen locally"
    );

    // Leave the process-wide cache as we found it, as the guard's contract says.
    crate::settings::set_current(AppSettings::default());
}

// ---- phase 6.3: the config push carries the restore setting ----

/// Extract the body of a raw HTTP request, once it has fully arrived.
/// `None` means "keep reading". Fallible rather than asserting: `clippy`'s
/// `allow-expect-in-tests` does not excuse a helper from panicking, and the
/// test function is the right place to fail.
fn request_body(raw: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(raw).to_string();
    let (head, body) = text.split_once("\r\n\r\n")?;
    let length: usize = head.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse().ok())
            .flatten()
    })?;
    (body.len() >= length).then(|| body[..length].to_string())
}

/// Run one `set_config_at` against a throwaway loopback listener and return
/// the JSON body it pushed. No real backend, no hardware: the point is what
/// goes on the wire.
async fn captured_config_push(
    name: &str,
    restore_during_playback: bool,
    auto_reconnect: bool,
) -> Result<serde_json::Value, String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("bind the test listener: {e}"))?;
    let addr = listener
        .local_addr()
        .map_err(|e| format!("read the test listener address: {e}"))?;

    let server = tokio::spawn(async move {
        let (mut stream, _) = listener
            .accept()
            .await
            .map_err(|e| format!("accept: {e}"))?;
        let mut raw = Vec::new();
        let mut chunk = [0u8; 1024];
        let body = loop {
            match request_body(&raw) {
                Some(body) => break body,
                None => {
                    let read = stream
                        .read(&mut chunk)
                        .await
                        .map_err(|e| format!("read the request: {e}"))?;
                    if read == 0 {
                        return Err("the client closed before sending a body".to_string());
                    }
                    raw.extend_from_slice(&chunk[..read]);
                },
            }
        };
        // A well-formed `ServerConfig` so the client's decode step succeeds.
        let payload = r#"{"name":"Salon","restore_during_playback":true,"auto_reconnect":true}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{payload}",
            payload.len()
        );
        stream
            .write_all(response.as_bytes())
            .await
            .map_err(|e| format!("write the response: {e}"))?;
        let _ = stream.flush().await;
        Ok::<String, String>(body)
    });

    let base = format!("http://{addr}");
    // No token: this fixture reads the *body* the push sends, and the canned
    // listener answers whatever the header says.
    let pushed = set_config_at(
        &base,
        None,
        ConfigRequest {
            // Owned copy: `ConfigRequest` is a plain DTO built for serialization.
            name: name.to_string(),
            restore_during_playback,
            auto_reconnect,
            spotify_volume_lock: None,
        },
    )
    .await;
    let body = server
        .await
        .map_err(|e| format!("join the test listener: {e}"))??;
    pushed.map_err(|e| format!("set_config_at: {e}"))?;
    serde_json::from_str(&body).map_err(|e| format!("parse the pushed body ({body}): {e}"))
}

// Criterion (phase 6.3): the config push carries the flag — `POST /config`
// sends `restore_during_playback` next to the name, in both states.
#[tokio::test]
async fn test_config_push_carries_the_restore_flag() {
    let pushed = captured_config_push("Salon", true, true)
        .await
        .expect("push the config with the flag on");
    assert_eq!(pushed.get("name").and_then(|v| v.as_str()), Some("Salon"));
    assert_eq!(
        pushed
            .get("restore_during_playback")
            .and_then(serde_json::Value::as_bool),
        Some(true),
        "the pushed body must carry the caller's flag, got {pushed}"
    );

    let pushed = captured_config_push("Salon", false, true)
        .await
        .expect("push the config with the flag off");
    assert_eq!(
        pushed
            .get("restore_during_playback")
            .and_then(serde_json::Value::as_bool),
        Some(false),
        "turning the setting off must reach the backend, got {pushed}"
    );
}

// Criterion (phase 6.5): `set_config_at` puts `auto_reconnect` in the pushed
// body, in both states — the toggle has to reach the backend to mean anything.
#[tokio::test]
async fn test_config_push_carries_the_auto_reconnect_flag() {
    let pushed = captured_config_push("Salon", true, true)
        .await
        .expect("push the config with auto-reconnect on");
    assert_eq!(pushed.get("name").and_then(|v| v.as_str()), Some("Salon"));
    assert_eq!(
        pushed
            .get("auto_reconnect")
            .and_then(serde_json::Value::as_bool),
        Some(true),
        "the pushed body must carry the caller's flag, got {pushed}"
    );

    let pushed = captured_config_push("Salon", true, false)
        .await
        .expect("push the config with auto-reconnect off");
    assert_eq!(
        pushed
            .get("auto_reconnect")
            .and_then(serde_json::Value::as_bool),
        Some(false),
        "turning the setting off must reach the backend, got {pushed}"
    );
    assert_eq!(
        pushed
            .get("restore_during_playback")
            .and_then(serde_json::Value::as_bool),
        Some(true),
        "the phase 6.3 flag must keep its own value, got {pushed}"
    );
}

// Criterion (phase 6.2): switching backends quietens the one being left
// behind — a different address than the newly active one.
#[test]
fn test_is_left_behind_is_true_for_another_backend() {
    assert!(is_left_behind(
        "http://127.0.0.1:1",
        Some("http://127.0.0.1:2")
    ));
}

// Criterion (phase 6.2): re-activating the backend already in use must not
// pause it — that would stop the playback the user just asked to keep.
#[test]
fn test_is_left_behind_is_false_for_the_same_backend() {
    assert!(!is_left_behind(
        "http://127.0.0.1:1",
        Some("http://127.0.0.1:1")
    ));
}

// Criterion (phase 6.2): with no backend left active (the switch target
// vanished), the previous one is still quietened rather than left playing.
#[test]
fn test_is_left_behind_is_true_when_nothing_is_active() {
    assert!(is_left_behind("http://127.0.0.1:1", None));
}

/// One entry at `url`, named `name`. Built by hand: a fixture must not
/// depend on the settings functions under test elsewhere.
fn entry(name: &str, url: &str) -> crate::settings::BackendEntry {
    crate::settings::BackendEntry {
        name: name.to_string(),
        url: url.to_string(),
        restore_during_playback: true,
        auto_reconnect: true,
        token: None,
        pairing: crate::settings::PairingMethod::Code,
        id: None,
        config_pending: false,
    }
}

// Criterion: deleting the last entry pointing at a machine releases it —
// otherwise the PC keeps streaming with nothing left in the app to stop it.
#[test]
fn test_is_last_reference_is_true_when_nothing_points_at_it_any_more() {
    let remaining = vec![entry("Bureau", "http://127.0.0.1:2")];
    assert!(is_last_reference(&remaining, "http://127.0.0.1:1"));
    assert!(is_last_reference(&[], "http://127.0.0.1:1"));
}

// Criterion (non-nominal): only *names* are unique, so two entries may carry
// the same address. Deleting one label must not silence a machine the app
// still drives through the other.
#[test]
fn test_is_last_reference_is_false_while_another_entry_shares_the_address() {
    let remaining = vec![
        entry("Bureau", "http://127.0.0.1:2"),
        entry("Salon bis", "http://127.0.0.1:1"),
    ];
    assert!(!is_last_reference(&remaining, "http://127.0.0.1:1"));
}

#[test]
fn test_sse_device_payload_extracts_device_json() {
    let block = "event:device\ndata:{\"address\":\"AA\"}\n\n";
    assert_eq!(
        sse_device_payload(block).as_deref(),
        Some("{\"address\":\"AA\"}")
    );
}

#[test]
fn test_sse_device_payload_ignores_non_device_and_comments() {
    assert_eq!(sse_device_payload("event:error\ndata:boom\n\n"), None);
    assert_eq!(sse_device_payload(": keep-alive\n\n"), None);
}

// Criterion: a connected speaker can be selected as a playback target — the
// client posts to `/devices/{addr}/select`.
#[test]
fn test_device_action_url_builds_select_path() {
    assert_eq!(
        device_action_url("http://10.0.0.5:4000", "AA:BB:CC:DD:EE:FF", "select"),
        "http://10.0.0.5:4000/devices/AA:BB:CC:DD:EE:FF/select"
    );
}

// Criterion: deselecting removes the speaker — the client posts to
// `/devices/{addr}/deselect`.
#[test]
fn test_device_action_url_builds_deselect_path() {
    assert_eq!(
        device_action_url("http://10.0.0.5:4000", "AA:BB:CC:DD:EE:FF", "deselect"),
        "http://10.0.0.5:4000/devices/AA:BB:CC:DD:EE:FF/deselect"
    );
}

// Criterion: a per-speaker latency offset can be set — the client posts to
// `/devices/{addr}/offset`.
#[test]
fn test_device_action_url_builds_offset_path_tolerating_trailing_slash() {
    assert_eq!(
        device_action_url("http://10.0.0.5:4000/", "AA:BB:CC:DD:EE:FF", "offset"),
        "http://10.0.0.5:4000/devices/AA:BB:CC:DD:EE:FF/offset"
    );
}

// Criterion: `GET /targets` returns the current selection — the client builds
// the `/targets` URL, tolerating a trailing slash.
#[test]
fn test_targets_url_appends_path() {
    assert_eq!(
        targets_url("http://10.0.0.5:4000"),
        "http://10.0.0.5:4000/targets"
    );
    assert_eq!(
        targets_url("http://10.0.0.5:4000/"),
        "http://10.0.0.5:4000/targets"
    );
}

// Criterion: mobile exposes a Spotify start call — the client posts to
// `/spotify/start`.
#[test]
fn test_spotify_url_builds_start_path() {
    assert_eq!(
        spotify_url("http://10.0.0.5:4000", "start"),
        "http://10.0.0.5:4000/spotify/start"
    );
}

// Criterion: mobile exposes a Spotify stop call — the client posts to
// `/spotify/stop`, tolerating a trailing slash on the base.
#[test]
fn test_spotify_url_builds_stop_path_tolerating_trailing_slash() {
    assert_eq!(
        spotify_url("http://10.0.0.5:4000/", "stop"),
        "http://10.0.0.5:4000/spotify/stop"
    );
}

// Criterion: mobile exposes a Spotify status call — the client builds the
// `/spotify/status` URL.
#[test]
fn test_spotify_url_builds_status_path() {
    assert_eq!(
        spotify_url("http://10.0.0.5:4000", "status"),
        "http://10.0.0.5:4000/spotify/status"
    );
}

// Criterion (phase 5.2): mobile exposes an SSE now-playing subscription — the
// client builds the `/spotify/now-playing` URL, tolerating a trailing slash.
#[test]
fn test_now_playing_url_appends_path() {
    assert_eq!(
        now_playing_url("http://10.0.0.5:4000"),
        "http://10.0.0.5:4000/spotify/now-playing"
    );
    assert_eq!(
        now_playing_url("http://10.0.0.5:4000/"),
        "http://10.0.0.5:4000/spotify/now-playing"
    );
}

// Criterion (phase 5.2): the SSE reader parses a `now-playing` event block into
// a `NowPlaying` snapshot.
#[test]
fn test_sse_now_playing_payload_parses_now_playing_event() {
    let block = concat!(
        "event:now-playing\n",
        "data:{\"state\":\"playing\",\"title\":\"Song\",\"artist\":\"Artist\",",
        "\"album\":\"Album\",\"progress_ms\":12000,\"duration_ms\":210000}\n\n",
    );
    let np = sse_now_playing_payload(block).expect("parse now-playing event");
    assert_eq!(np.state, blue2th_proto::NowPlayingState::Playing);
    assert_eq!(np.title.as_deref(), Some("Song"));
}

// Criterion (phase 5.2): the SSE reader ignores keep-alive comments and other
// event kinds (returns None).
#[test]
fn test_sse_now_playing_payload_ignores_non_now_playing_and_comments() {
    assert!(sse_now_playing_payload("event:error\ndata:boom\n\n").is_none());
    assert!(sse_now_playing_payload(": keep-alive\n\n").is_none());
}

// ---- phase 6.4: the bearer on every call, and the pairing exchange ----

use crate::settings::{BackendEntry, PairingMethod};

/// Settings holding one active backend at `url`, paired or not.
///
/// Built by hand rather than through `add`/`set_token`, so a fixture never
/// depends on the functions under test.
fn active_with_token(url: &str, token: Option<&str>) -> AppSettings {
    AppSettings {
        backends: vec![BackendEntry {
            name: "Salon".to_string(),
            url: url.to_string(),
            restore_during_playback: true,
            auto_reconnect: true,
            token: token.map(str::to_string),
            pairing: PairingMethod::Code,
            // Phase 6.6: an entry that never met a discovered service.
            id: None,
            config_pending: false,
        }],
        active: Some(0),
        auto_repair_url: true,
        discovery_adds_backends: true,
    }
}

/// The whole raw request once it has fully arrived (head, and body when a
/// `content-length` announces one). `None` means "keep reading".
fn request_complete(raw: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(raw).to_string();
    let (head, body) = text.split_once("\r\n\r\n")?;
    let length: usize = head
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())
                .flatten()
        })
        .unwrap_or(0);
    (body.len() >= length).then(|| text.clone())
}

/// Read a header value out of a raw request.
fn header_value(raw: &str, name: &str) -> Option<String> {
    raw.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name)
            .then(|| value.trim().to_string())
    })
}

/// Serve exactly one request on a throwaway loopback listener with a canned
/// reply, and hand back the base URL plus the raw request the client sent.
/// No real backend and no hardware: the point is what goes on the wire.
async fn canned_backend(
    status_line: &'static str,
    payload: &'static str,
) -> Result<(String, tokio::task::JoinHandle<Result<String, String>>), String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("bind the test listener: {e}"))?;
    let addr = listener
        .local_addr()
        .map_err(|e| format!("read the test listener address: {e}"))?;

    let handle = tokio::spawn(async move {
        let (mut stream, _) = listener
            .accept()
            .await
            .map_err(|e| format!("accept: {e}"))?;
        let mut raw = Vec::new();
        let mut chunk = [0u8; 1024];
        let request = loop {
            match request_complete(&raw) {
                Some(request) => break request,
                None => {
                    let read = stream
                        .read(&mut chunk)
                        .await
                        .map_err(|e| format!("read the request: {e}"))?;
                    if read == 0 {
                        return Err("the client closed before sending a request".to_string());
                    }
                    raw.extend_from_slice(&chunk[..read]);
                },
            }
        };
        let response = format!(
            "HTTP/1.1 {status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{payload}",
            payload.len()
        );
        stream
            .write_all(response.as_bytes())
            .await
            .map_err(|e| format!("write the response: {e}"))?;
        let _ = stream.flush().await;
        Ok::<String, String>(request)
    });

    Ok((format!("http://{addr}"), handle))
}

// ---- backend protocol compatibility check (#33) ----
//
// A workspace build compiles one `blue2th-proto`, so a `HealthStatus::ok()`
// built in-process could never be incompatible with itself: every payload
// below is canned on the wire, with the numbers written out by hand.

// Criterion: mobile — `check_backend_protocol(url)` returns `Ok(())` for a
// backend whose announced range contains this app's `PROTOCOL_VERSION`.
#[tokio::test]
async fn test_check_backend_protocol_accepts_a_backend_inside_the_range() {
    // Range 1..=1, matching `PROTOCOL_VERSION` at the time of writing.
    let (base, served) = canned_backend(
        "200 OK",
        r#"{"status":"ok","version":"0.1.0","protocol":1,"protocol_min":1}"#,
    )
    .await
    .expect("start the canned backend");

    let checked = check_backend_protocol(&base).await;
    let _ = served.await.expect("join the test listener");

    assert_eq!(
        blue2th_proto::PROTOCOL_VERSION,
        1,
        "the canned payload above is written for contract 1"
    );
    assert_eq!(
        checked.map_err(|e| e.to_string()),
        Ok(CompatibleBackend { url: base }),
        "a backend announcing 1..=1 serves an app speaking 1, at the checked URL"
    );
}

// Criterion (non-nominal): a backend speaking an older contract than the app
// is reported as `BackendTooOld` — update the backend on the server.
#[tokio::test]
async fn test_check_backend_protocol_refuses_a_backend_that_is_too_old() {
    let (base, served) = canned_backend(
        "200 OK",
        r#"{"status":"ok","version":"0.0.1","protocol":0,"protocol_min":0}"#,
    )
    .await
    .expect("start the canned backend");

    let checked = check_backend_protocol(&base).await;
    let _ = served.await.expect("join the test listener");

    assert_eq!(
        checked.err().and_then(|e| e.protocol_mismatch()),
        Some(ProtocolMismatch::BackendTooOld),
        "the app is newer than the backend: the server is the side to update"
    );
}

// Criterion (non-nominal): a backend that dropped support for apps this old
// is reported as `BackendTooNew` — update the app on the phone.
#[tokio::test]
async fn test_check_backend_protocol_refuses_a_backend_that_is_too_new() {
    let (base, served) = canned_backend(
        "200 OK",
        r#"{"status":"ok","version":"9.9.9","protocol":99,"protocol_min":99}"#,
    )
    .await
    .expect("start the canned backend");

    let checked = check_backend_protocol(&base).await;
    let _ = served.await.expect("join the test listener");

    assert_eq!(
        checked.err().and_then(|e| e.protocol_mismatch()),
        Some(ProtocolMismatch::BackendTooNew),
        "the backend no longer serves an app this old: the phone is the side to update"
    );
}

// Criterion (non-nominal): a backend predating the mechanism announces
// neither field; both read `0`, which fails the upper bound and is reported
// as "backend too old".
#[tokio::test]
async fn test_check_backend_protocol_reads_a_payload_without_the_fields_as_too_old() {
    let (base, served) = canned_backend("200 OK", r#"{"status":"ok","version":"0.1.0"}"#)
        .await
        .expect("start the canned backend");

    let checked = check_backend_protocol(&base).await;
    let _ = served.await.expect("join the test listener");

    assert_eq!(
        checked.err().and_then(|e| e.protocol_mismatch()),
        Some(ProtocolMismatch::BackendTooOld),
        "a backend that announces nothing is one to update"
    );
}

// Criterion (non-nominal): an unreachable backend stays a plain transport
// error. "Cannot reach" must never read as "incompatible" — the two point at
// completely different fixes.
#[tokio::test]
async fn test_check_backend_protocol_reports_an_unreachable_backend_as_a_plain_error() {
    // Port 1 is privileged and unbound: the connection is refused at once,
    // with no listener and no hardware involved.
    let checked = check_backend_protocol("http://127.0.0.1:1").await;

    // Compared as one value so the failure shows what actually came back:
    // it must be an error, carrying neither a mismatch nor a pairing hint.
    assert_eq!(
        checked.map_err(|e| (e.protocol_mismatch(), e.is_not_paired())),
        Err((None, false)),
        "a transport failure says nothing about the wire contract, nor about pairing"
    );
}

// Criterion: every backend call carries the bearer when the active entry has
// one — asserted on the wire, for a GET call.
#[tokio::test]
async fn test_backend_calls_carry_the_bearer_token() {
    let _guard = SETTINGS_GUARD.lock().await;
    let (base, served) = canned_backend("200 OK", r#"{"speakers":[],"routing":"idle"}"#)
        .await
        .expect("start the canned backend");
    crate::settings::set_current(active_with_token(&base, Some("tok-123")));

    let outcome = fetch_targets().await;
    let request = served
        .await
        .expect("join the test listener")
        .expect("serve one request");
    crate::settings::set_current(AppSettings::default());

    assert!(outcome.is_ok(), "the call must succeed: {outcome:?}");
    assert_eq!(
        header_value(&request, "authorization").as_deref(),
        Some("Bearer tok-123"),
        "every guarded call must carry the bearer, got {request}"
    );
}

// Criterion: the same holds for the calls that push a body — the config push
// is the one every sync of a pending change goes through (#160).
#[tokio::test]
async fn test_the_config_push_carries_the_bearer_token() {
    let _guard = SETTINGS_GUARD.lock().await;
    let (base, served) = canned_backend(
        "200 OK",
        r#"{"name":"Salon","restore_during_playback":true}"#,
    )
    .await
    .expect("start the canned backend");
    let mut settings = active_with_token(&base, Some("tok-123"));
    if let Some(salon) = settings.backends.first_mut() {
        salon.config_pending = true;
    }

    let outcome = sync_config(&mut settings).await;
    let request = served
        .await
        .expect("join the test listener")
        .expect("serve one request");
    crate::settings::set_current(AppSettings::default());

    assert!(outcome.is_ok(), "the push must succeed: {outcome:?}");
    assert_eq!(
        header_value(&request, "authorization").as_deref(),
        Some("Bearer tok-123")
    );
}

// Criterion (non-nominal): with no token for the active backend the calls
// fail fast — the way an unconfigured backend does — instead of every screen
// failing on its own after a timeout.
#[tokio::test]
async fn test_a_call_without_a_token_fails_fast_as_not_paired() {
    let _guard = SETTINGS_GUARD.lock().await;
    // Port 1 is never listening: reaching it at all would take a timeout.
    crate::settings::set_current(active_with_token("http://127.0.0.1:1", None));

    let started = std::time::Instant::now();
    let error = fetch_targets()
        .await
        .expect_err("an unpaired app must not call the backend");
    crate::settings::set_current(AppSettings::default());

    assert!(
        error.is_not_paired(),
        "the failure must point at pairing, got {error}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "no request may be attempted, so the call must return immediately"
    );
}

// Criterion: `pair(backend, code)` exchanges the code for the token — and sends
// no bearer, since the app has none yet.
#[tokio::test]
async fn test_pair_exchanges_the_code_for_the_token() {
    let (base, served) = canned_backend("200 OK", r#"{"token":"api-token-value"}"#)
        .await
        .expect("start the canned backend");

    let token = pair(&CompatibleBackend { url: base }, "K7M2QX").await;
    let request = served
        .await
        .expect("join the test listener")
        .expect("serve one request");

    assert_eq!(
        token.map_err(|e| e.to_string()),
        Ok("api-token-value".to_string())
    );
    assert!(
        request.starts_with("POST /pair "),
        "the exchange must POST /pair, got {request}"
    );
    assert!(
        request.contains("K7M2QX"),
        "the submitted code must be on the wire, got {request}"
    );
    assert_eq!(
        header_value(&request, "authorization"),
        None,
        "pairing is the one call made without a bearer"
    );
}

// Criterion (non-nominal): a refused code reads as "not paired", distinct
// from an unreachable backend, so the settings page can say so.
#[tokio::test]
async fn test_pair_with_a_refused_code_reports_not_paired() {
    let (base, served) = canned_backend("401 Unauthorized", "pairing refused")
        .await
        .expect("start the canned backend");

    let error = pair(&CompatibleBackend { url: base }, "AAAAAA")
        .await
        .expect_err("a refused code must not yield a token");
    let _ = served.await;

    assert!(error.is_not_paired(), "got {error}");
}

// Criterion (non-nominal): the SSE feeds must treat a 401 as terminal rather
// than as a network blip — the subscription returns a "not paired" failure,
// which is what lets the reconnect loop stop instead of spinning forever.
#[tokio::test]
async fn test_now_playing_subscription_reports_not_paired_on_401() {
    let _guard = SETTINGS_GUARD.lock().await;
    let (base, served) = canned_backend("401 Unauthorized", "not paired")
        .await
        .expect("start the canned backend");
    crate::settings::set_current(active_with_token(&base, Some("stale-token")));

    let error = subscribe_now_playing(|_| {})
        .await
        .expect_err("a 401 must end the subscription");
    let _ = served.await;
    crate::settings::set_current(AppSettings::default());

    assert!(error.is_not_paired(), "got {error}");
}

// Criterion (non-nominal): `/health` answers while everything else 401s, so
// the probe must reach it **without** a token — an alive-but-unpaired
// backend has to read as "paired?", never as "offline".
#[tokio::test]
async fn test_ping_backend_reaches_health_without_a_token() {
    let _guard = SETTINGS_GUARD.lock().await;
    let (base, served) = canned_backend(
        "200 OK",
        r#"{"status":"ok","version":"0.1.0","auth_required":true}"#,
    )
    .await
    .expect("start the canned backend");
    crate::settings::set_current(active_with_token(&base, None));

    let health = ping_backend().await;
    let request = served
        .await
        .expect("join the test listener")
        .expect("serve one request");
    crate::settings::set_current(AppSettings::default());

    assert_eq!(
        health.map(|h| h.auth_required).map_err(|e| e.to_string()),
        Ok(true),
        "an unpaired app must still be able to tell the backend is alive"
    );
    assert_eq!(
        header_value(&request, "authorization"),
        None,
        "there is no token to send yet"
    );
}

// Criterion: once paired, the probe carries the bearer like every other
// call — one code path, whether the app holds a token or not.
#[tokio::test]
async fn test_ping_backend_carries_the_bearer_once_paired() {
    let _guard = SETTINGS_GUARD.lock().await;
    let (base, served) = canned_backend(
        "200 OK",
        r#"{"status":"ok","version":"0.1.0","auth_required":true}"#,
    )
    .await
    .expect("start the canned backend");
    crate::settings::set_current(active_with_token(&base, Some("tok-123")));

    let health = ping_backend().await;
    let request = served
        .await
        .expect("join the test listener")
        .expect("serve one request");
    crate::settings::set_current(AppSettings::default());

    assert!(health.is_ok(), "the probe must succeed: {health:?}");
    assert_eq!(
        header_value(&request, "authorization").as_deref(),
        Some("Bearer tok-123")
    );
}

// Criterion: a 401 on a plain (non-SSE) call is surfaced as "not paired"
// too, not as a bare status line — a revoked token must read the same way
// whichever screen hits it first.
#[tokio::test]
async fn test_a_plain_call_reports_not_paired_on_401() {
    let _guard = SETTINGS_GUARD.lock().await;
    let (base, served) = canned_backend("401 Unauthorized", "not paired")
        .await
        .expect("start the canned backend");
    crate::settings::set_current(active_with_token(&base, Some("stale-token")));

    let error = fetch_targets()
        .await
        .expect_err("a revoked token must not yield a selection");
    let _ = served.await;
    crate::settings::set_current(AppSettings::default());

    assert!(error.is_not_paired(), "got {error}");
}

// Criterion (non-nominal): the `/scan` feed treats a 401 as terminal in the
// same way `/spotify/now-playing` does — both SSE routes are guarded, so
// both must stop rather than retry a revoked token.
#[tokio::test]
async fn test_scan_reports_not_paired_on_401() {
    let _guard = SETTINGS_GUARD.lock().await;
    let (base, served) = canned_backend("401 Unauthorized", "not paired")
        .await
        .expect("start the canned backend");
    crate::settings::set_current(active_with_token(&base, Some("stale-token")));

    let error = scan_devices()
        .await
        .expect_err("a 401 must end the scan feed");
    let _ = served.await;
    crate::settings::set_current(AppSettings::default());

    assert!(error.is_not_paired(), "got {error}");
}

// Criterion: a 401 from any route maps to the typed "not paired" failure,
// whatever the backend wrote in the body.
#[test]
fn test_backend_error_for_401_is_not_paired() {
    let error = backend_error_for(401, "some server wording");
    assert!(error.is_not_paired());
    assert!(
        error.to_string().contains(NOT_PAIRED),
        "the user must read that the app is not paired, got {error}"
    );
}

// Criterion: any other failure keeps the backend's own message, which is the
// only thing telling the user what actually went wrong.
#[test]
fn test_backend_error_for_another_status_keeps_the_backend_message() {
    let error = backend_error_for(503, "Spotify client id not configured");
    assert!(!error.is_not_paired());
    assert_eq!(error.to_string(), "Spotify client id not configured");

    // An empty body still has to say something.
    let bare = backend_error_for(500, "");
    assert!(!bare.to_string().trim().is_empty());
}

// ---- #52: a refused Bluetooth pairing is typed, but only by /connect ----

/// Every status the backend answers today, straight from `AppError`: 400,
/// 401, 403, 404, 409, 412, 429, 500, 502, 503. Eight of them come from the
/// Spotify mapping alone, which is why no status-only rule can own a
/// route-specific meaning.
const BACKEND_STATUSES: [u16; 10] = [400, 401, 403, 404, 409, 412, 429, 500, 502, 503];

// Criterion: `backend_error_for` flags **nothing** as a pairing failure —
// 409 included. It is the single mapping point for every route, so anything
// it flags becomes global, and the next status added server-side would
// re-break the flag silently.
#[test]
fn test_backend_error_for_flags_no_status_as_a_pairing_failure() {
    for status in BACKEND_STATUSES {
        let error = backend_error_for(status, "boom");
        assert!(
            !error.is_pairing_failed(),
            "HTTP {status} must carry no pairing meaning of its own, got {error}"
        );
    }
}

// Criterion: 409 in particular is not flagged here. It is already
// `SpotifyApiError::NotConnected`, and the connect route is the only place
// allowed to read it as a refused speaker.
#[test]
fn test_backend_error_for_409_is_not_flagged_as_a_pairing_failure() {
    let error = backend_error_for(409, "pairing failed: Authentication Timeout");
    assert!(
        !error.is_pairing_failed(),
        "a bare 409 says nothing about a Bluetooth bond, got {error}"
    );
    assert!(
        !error.is_not_paired(),
        "a 409 must not read as an unpaired backend either, got {error}"
    );
}

// Criterion: the response status is **retained** on the error instead of
// being interpreted — that is what lets a single route, and no one else,
// give it a meaning.
#[test]
fn test_backend_error_for_retains_the_response_status() {
    for status in BACKEND_STATUSES {
        let error = backend_error_for(status, "boom");
        assert_eq!(
            error.status(),
            Some(status),
            "HTTP {status} must survive on the error, got {error}"
        );
    }
}

// Criterion: the 401 rule is unchanged — it is genuinely global, since any
// route can reject a stale token. It stays typed, and it keeps its status.
#[test]
fn test_backend_error_for_401_is_still_not_paired() {
    let error = backend_error_for(401, "some server wording");
    assert!(error.is_not_paired(), "got {error}");
    assert!(
        !error.is_pairing_failed(),
        "an unpaired app is not a refused speaker, got {error}"
    );
    assert_eq!(error.status(), Some(401), "got {error}");
    assert!(
        error.to_string().contains(NOT_PAIRED),
        "the user must read that the app is not paired, got {error}"
    );
}

// Criterion: the body still survives for every status but the typed 401 —
// the backend's own wording is all the user gets on most screens.
#[test]
fn test_backend_error_for_keeps_the_body_for_every_status() {
    for status in BACKEND_STATUSES.into_iter().filter(|s| *s != 401) {
        let error = backend_error_for(status, "  the backend's own wording  ");
        assert_eq!(
            error.to_string(),
            "the backend's own wording",
            "HTTP {status} must keep the body verbatim"
        );
    }
}

// Criterion: with nothing to show, every status falls back to the plain
// status line — never to the Bluetooth pairing wording.
#[test]
fn test_backend_error_for_without_a_body_falls_back_to_the_status_line() {
    for status in BACKEND_STATUSES.into_iter().filter(|s| *s != 401) {
        let error = backend_error_for(status, "   ");
        assert_eq!(error.to_string(), format!("HTTP {status}"));
        assert_ne!(
            error.to_string(),
            BLUETOOTH_PAIRING_FAILED,
            "a bodiless HTTP {status} must not be dressed as a refused speaker"
        );
    }
}

// Criterion (regression, the reason this pass exists): an error built the
// way `/spotify/play` builds one — 409, a Spotify body — is not a pairing
// failure, because it never went through the connect route. This is what
// stops the two meanings collapsing back together.
#[test]
fn test_a_409_from_the_spotify_route_is_never_a_pairing_failure() {
    let error = backend_error_for(409, "no active Spotify device");
    assert!(
        !error.is_pairing_failed(),
        "SpotifyApiError::NotConnected is not a refused speaker, got {error}"
    );
    assert_eq!(
        error.to_string(),
        "no active Spotify device",
        "the Spotify wording is all the dialog shows"
    );
}

// Criterion (regression): the same for the 502 that burnt the first attempt
// — a failed Spotify token exchange is not a speaker refusing the bond, and
// it keeps its body.
#[test]
fn test_backend_error_for_502_is_not_a_pairing_failure_and_keeps_its_body() {
    let error = backend_error_for(502, "Spotify token exchange failed: invalid code");
    assert!(
        !error.is_pairing_failed(),
        "a failed Spotify token exchange is not a refused speaker, got {error}"
    );
    assert!(!error.is_not_paired(), "got {error}");
    assert_eq!(
        error.to_string(),
        "Spotify token exchange failed: invalid code",
        "a 502 must keep the backend's own wording verbatim"
    );
}

// ---- the connect route is the only producer of the flag ----

// Criterion: the connect route's pure mapping turns a retained 409 into a
// pairing failure, keeping the BlueZ wording the backend sent.
#[test]
fn test_pairing_failure_if_conflict_flags_a_retained_409() {
    let error = backend_error_for(409, "pairing failed: Authentication Timeout")
        .pairing_failure_if_conflict();

    assert!(
        error.is_pairing_failed(),
        "a 409 on /connect means the speaker refused the bond, got {error}"
    );
    assert!(
        !error.is_not_paired(),
        "a refused speaker must not read as an unpaired backend, got {error}"
    );
    assert_eq!(
        error.to_string(),
        "pairing failed: Authentication Timeout",
        "the BlueZ wording is what tells the operator what happened"
    );
}

// Criterion: a bodiless 409 on the connect route still reads as a pairing
// failure, falling back to the constant rather than to a bare status line.
#[test]
fn test_pairing_failure_if_conflict_without_a_body_falls_back_to_the_constant() {
    let error = backend_error_for(409, "   ").pairing_failure_if_conflict();
    assert!(error.is_pairing_failed(), "got {error}");
    assert_eq!(error.to_string(), BLUETOOTH_PAIRING_FAILED);
}

// Criterion: the bodiless case is recognised by comparing the message with
// the status line, so a backend answering 409 with `HTTP 409` as its body
// takes that same branch. Harmless, and pinned here so it stays a known
// property rather than a surprise: the row is flagged either way, and both
// messages say the same thing to the log — the row itself shows the
// localised `device.pairing_failed`, never this text.
#[test]
fn test_pairing_failure_if_conflict_with_a_status_line_body_is_still_flagged() {
    let error = backend_error_for(409, "HTTP 409").pairing_failure_if_conflict();
    assert!(error.is_pairing_failed(), "got {error}");
    assert_eq!(error.to_string(), BLUETOOTH_PAIRING_FAILED);
}

// Criterion: every other status passes through unflagged, so a connect that
// failed for any other reason still greys the row exactly as before.
#[test]
fn test_pairing_failure_if_conflict_leaves_every_other_status_unflagged() {
    for status in BACKEND_STATUSES.into_iter().filter(|s| *s != 409) {
        let error = backend_error_for(status, "boom").pairing_failure_if_conflict();
        assert!(
            !error.is_pairing_failed(),
            "only a 409 is a refused bond, HTTP {status} gave {error}"
        );
    }
}

// Criterion: a `not_paired` error passes through untouched — a 401 on the
// connect route is still "pair the app with the backend", not "the speaker
// refused the bond".
#[test]
fn test_pairing_failure_if_conflict_leaves_a_not_paired_error_untouched() {
    let error = backend_error_for(401, "stale token").pairing_failure_if_conflict();
    assert!(error.is_not_paired(), "got {error}");
    assert!(
        !error.is_pairing_failed(),
        "the app-to-backend pairing and the Bluetooth one must not collide, got {error}"
    );
    assert!(error.to_string().contains(NOT_PAIRED), "got {error}");
}

// Criterion: `not_paired` wins over the connect route's reading even when
// the two coincide. `backend_error_for` cannot produce this pair today (only
// a 401 sets the flag), so the guard is what makes the precedence a rule
// rather than an accident of the current status mapping — and this test is
// what fails if the guard is dropped.
#[test]
fn test_pairing_failure_if_conflict_leaves_a_not_paired_409_untouched() {
    let error = BackendError {
        status: Some(CONFLICT),
        ..BackendError::not_paired()
    }
    .pairing_failure_if_conflict();

    assert!(error.is_not_paired(), "got {error}");
    assert!(
        !error.is_pairing_failed(),
        "an unpaired app must never be reported as a refused speaker, got {error}"
    );
}

// Criterion: a failure that never saw a response (no token stored) carries
// no status, so the connect mapping cannot flag it.
#[test]
fn test_pairing_failure_if_conflict_leaves_a_statusless_error_untouched() {
    let error = BackendError::new("connection refused");
    assert_eq!(error.status(), None, "got {error}");
    let error = error.pairing_failure_if_conflict();
    assert!(!error.is_pairing_failed(), "got {error}");
    assert_eq!(error.to_string(), "connection refused");
}

// Criterion: the constructor mirrors `not_paired()` — flagged as a pairing
// failure, and not as an unpaired backend.
#[test]
fn test_pairing_failed_constructor_is_flagged_as_a_pairing_failure() {
    let error = BackendError::pairing_failed();
    assert!(error.is_pairing_failed(), "got {error}");
    assert!(!error.is_not_paired(), "got {error}");
    assert!(error.protocol_mismatch().is_none(), "got {error}");
}

// Criterion: end to end, `connect_device` is the route that applies the
// mapping — a backend answering 409 there yields a flagged error.
#[tokio::test]
async fn test_connect_device_reports_a_pairing_failure_on_409() {
    let _guard = SETTINGS_GUARD.lock().await;
    let (base, served) = canned_backend("409 Conflict", "pairing failed: Authentication Timeout")
        .await
        .expect("start the canned backend");
    crate::settings::set_current(active_with_token(&base, Some("tok-123")));

    let error = connect_device("AA:BB:CC:DD:EE:FF")
        .await
        .expect_err("a refused bond must not yield a device");
    let _ = served.await;
    crate::settings::set_current(AppSettings::default());

    assert!(
        error.is_pairing_failed(),
        "the connect route must type the 409, got {error}"
    );
}

// Criterion (regression): the very same status on another route is not
// flagged, because the mapping lives on the connect route alone.
#[tokio::test]
async fn test_play_does_not_report_a_pairing_failure_on_409() {
    let _guard = SETTINGS_GUARD.lock().await;
    let (base, served) = canned_backend("409 Conflict", "no active Spotify device")
        .await
        .expect("start the canned backend");
    crate::settings::set_current(active_with_token(&base, Some("tok-123")));

    let error = play()
        .await
        .expect_err("a 409 must not yield a playback state");
    let _ = served.await;
    crate::settings::set_current(AppSettings::default());

    assert!(
        !error.is_pairing_failed(),
        "only /connect reads a 409 as a refused bond, got {error}"
    );
    assert_eq!(error.to_string(), "no active Spotify device");
}

// Criterion (regression): the sibling route built on the very same helper is
// not flagged either. `connect_device` and `disconnect_device` both go
// through `post_device_action`, so moving the mapping one level down is the
// easy accident that would make the flag inexact again — this is the test
// that catches it, which the `/spotify/play` one cannot.
#[tokio::test]
async fn test_disconnect_device_does_not_report_a_pairing_failure_on_409() {
    let _guard = SETTINGS_GUARD.lock().await;
    let (base, served) = canned_backend("409 Conflict", "device is busy")
        .await
        .expect("start the canned backend");
    crate::settings::set_current(active_with_token(&base, Some("tok-123")));

    let error = disconnect_device("AA:BB:CC:DD:EE:FF")
        .await
        .expect_err("a 409 must not yield a device");
    let _ = served.await;
    crate::settings::set_current(AppSettings::default());

    assert!(
        !error.is_pairing_failed(),
        "the connect route alone reads a 409 as a refused bond, got {error}"
    );
    assert_eq!(error.to_string(), "device is busy");
}

// Criterion: switching backends quietens the one being left behind with
// **its own** token — the active token is by then the other backend's, so
// sending that (or none) would have the pause refused with a 401.
#[tokio::test]
async fn test_activate_backend_pauses_the_previous_one_with_its_own_token() {
    let _guard = SETTINGS_GUARD.lock().await;
    let (leaving, served) = canned_backend("204 No Content", "")
        .await
        .expect("start the canned backend");

    let mut settings = active_with_token(&leaving, Some("leaving-token"));
    settings
        .add("Bureau", "http://127.0.0.1:2")
        .expect("add the backend being switched to");
    settings
        .set_token(1, Some("arriving-token".to_string()))
        .expect("pair the backend being switched to");

    // Fails on the arriving backend (port 2 is never listening); the pause on
    // the one being left is what this test reads.
    let _ = activate_backend(&mut settings, 1).await;
    let request = served
        .await
        .expect("join the test listener")
        .expect("serve one request");
    crate::settings::set_current(AppSettings::default());

    assert!(
        request.starts_with("POST /spotify/pause "),
        "the backend being left must be paused, got {request}"
    );
    assert_eq!(
        header_value(&request, "authorization").as_deref(),
        Some("Bearer leaving-token"),
        "the pause must carry the leaving backend's own token, got {request}"
    );
}

// Criterion: the app resolves the active backend's address **and** token in
// one place, so no call site can forget the bearer.
#[test]
fn test_authed_base_from_returns_the_url_and_token() {
    let settings = active_with_token("http://192.168.1.107:4000", Some("tok-123"));
    assert_eq!(
        authed_base_from(&settings).map_err(|e| e.to_string()),
        Ok((
            "http://192.168.1.107:4000".to_string(),
            "tok-123".to_string()
        ))
    );
}

// Criterion (non-nominal): an active backend with no token is "not paired",
// not "no backend configured" — the two send the user to different places.
#[test]
fn test_authed_base_from_without_a_token_reports_not_paired() {
    let settings = active_with_token("http://192.168.1.107:4000", None);
    let error = authed_base_from(&settings).expect_err("an unpaired backend has no bearer");
    assert!(error.is_not_paired(), "got {error}");
}

// Criterion: with nothing active the failure stays the phase 6.2 one.
#[test]
fn test_authed_base_from_without_an_active_backend_is_unconfigured() {
    let error = authed_base_from(&AppSettings::default())
        .expect_err("an unconfigured app must have no address");
    assert!(
        error.to_string().contains(NO_BACKEND_CONFIGURED),
        "got {error}"
    );
    assert!(
        !error.is_not_paired(),
        "nothing configured is not the same as not paired"
    );
}

// Criterion: the pairing exchange posts to `{base}/pair`, tolerating a
// trailing slash on the base like every other URL builder here.
#[test]
fn test_pair_url_appends_path() {
    assert_eq!(
        pair_url("http://10.0.0.5:4000"),
        "http://10.0.0.5:4000/pair"
    );
    assert_eq!(
        pair_url("http://10.0.0.5:4000/"),
        "http://10.0.0.5:4000/pair"
    );
}

// Criterion: the credential travels as a bearer, the scheme the server
// parses.
#[test]
fn test_auth_header_value_is_a_bearer() {
    assert_eq!(auth_header_value("tok-123"), "Bearer tok-123");
}

// ---- #160: the browser reads the backend's config, and reports presence ----

/// What a fresh backend's `GET /config` returns, with the two toggles set
/// to **different** values so a swap between them shows, and the volume
/// lock on so its absence downstream means something.
const BACKEND_CONFIG: &str = r#"{"name":"blue2th-PC","restore_during_playback":false,"auto_reconnect":true,"spotify_volume_lock":true}"#;

/// How long a test waits for the call under test to reach the canned
/// backend. Bounded so a client that never sends — a stub, a regression —
/// fails the test instead of hanging it while it holds `SETTINGS_GUARD`.
const SERVED_BOUND: Duration = Duration::from_secs(5);

/// The raw request the canned backend served, or why there is none: the
/// bound elapsed with no request sent, or the listener task failed. The
/// listener is aborted on a timeout, so nothing is left accepting.
async fn served_within_bound(
    mut served: tokio::task::JoinHandle<Result<String, String>>,
) -> Result<String, String> {
    match tokio::time::timeout(SERVED_BOUND, &mut served).await {
        Ok(joined) => joined.map_err(|e| format!("join the test listener: {e}"))?,
        Err(_) => {
            served.abort();
            Err(format!(
                "no request reached the backend within {SERVED_BOUND:?}"
            ))
        },
    }
}

// Criterion: a paired browser reads `GET /config` and gets the backend's
// `ServerConfig`, the bearer on the request.
#[tokio::test]
async fn test_fetch_config_reads_the_backend_config_with_the_bearer() {
    let _guard = SETTINGS_GUARD.lock().await;
    let (base, served) = canned_backend("200 OK", BACKEND_CONFIG)
        .await
        .expect("start the canned backend");
    // The cache is left alone: the address comes from the snapshot.
    let settings = active_with_token(&base, Some("tok-123"));

    let outcome = fetch_config(&settings).await;
    let request = served_within_bound(served).await;

    assert_eq!(
        outcome.map_err(|e| e.to_string()),
        Ok(ServerConfig {
            name: "blue2th-PC".to_string(),
            restore_during_playback: false,
            auto_reconnect: true,
            spotify_volume_lock: true,
        })
    );
    let request = request.unwrap_or_default();
    assert!(
        request.starts_with("GET /config "),
        "the read is a GET on /config, got {request}"
    );
    assert_eq!(
        header_value(&request, "authorization").as_deref(),
        Some("Bearer tok-123")
    );
}

// Criterion (non-nominal): a revoked token (401) on `GET /config` surfaces
// as "not paired", like every other route.
#[tokio::test]
async fn test_fetch_config_reports_not_paired_on_401() {
    let _guard = SETTINGS_GUARD.lock().await;
    let (base, served) = canned_backend("401 Unauthorized", "not paired")
        .await
        .expect("start the canned backend");
    // The cache is left alone: the address comes from the snapshot.
    let settings = active_with_token(&base, Some("stale-token"));

    let outcome = fetch_config(&settings).await;
    let request = served_within_bound(served).await;

    assert!(
        outcome.as_ref().is_err_and(BackendError::is_not_paired),
        "got {outcome:?}"
    );
    assert!(
        request.is_ok(),
        "the 401 must come from the backend, not from a call never sent: {request:?}"
    );
}

// Criterion (non-nominal): an unpaired browser does not read the config —
// it fails fast as "not paired", before any request.
#[tokio::test]
async fn test_fetch_config_without_a_token_fails_fast_as_not_paired() {
    let _guard = SETTINGS_GUARD.lock().await;
    // Port 1 is never listening: reaching it at all would take a timeout.
    // The cache is left alone: the address comes from the snapshot.
    let settings = active_with_token("http://127.0.0.1:1", None);

    let outcome = fetch_config(&settings).await;

    assert!(
        outcome.as_ref().is_err_and(BackendError::is_not_paired),
        "got {outcome:?}"
    );
}

// Criterion: `spotify_volume_lock` is neither shown nor sent — once the
// backend's config is adopted, the config the browser pushes on a user edit
// still leaves the lock out. The adopted name is checked too, so the test
// cannot pass on an adoption that did nothing.
#[test]
fn test_an_adopted_config_never_sends_the_volume_lock() {
    let mut settings = active_with_token("http://localhost:8080", Some("tok-123"));
    crate::settings::adopt_config(
        &mut settings,
        &ServerConfig {
            name: "blue2th-PC".to_string(),
            restore_during_playback: false,
            auto_reconnect: true,
            spotify_volume_lock: true,
        },
    );
    let body = settings.active_backend().map(config_body);

    assert_eq!(
        body.as_ref().map(|b| b.name.as_str()),
        Some("blue2th-PC"),
        "the adopted name is what the next push carries"
    );
    assert_eq!(body.and_then(|b| b.spotify_volume_lock), None);
}

// Criterion (guard, empty value): an empty origin, or the literal `"null"`
// a `file://` page reports, yields no backend, so every call fails with "no
// backend configured". Near-miss: the stored blob holds a paired entry — a
// reduction that kept it, or that built an entry at an empty URL, would
// answer with a base (or "not paired") instead.
#[test]
fn test_an_unusable_origin_fails_as_no_backend_configured() {
    for origin in ["", "null"] {
        let stored = active_with_token("http://localhost:8080", Some("tok-123"));
        let settings = crate::settings::browser_settings(stored, origin);

        assert_eq!(
            authed_base_from(&settings).map_err(|e| e.to_string()),
            Err(NO_BACKEND_CONFIGURED.to_string()),
            "origin {origin:?}"
        );
    }
}

// Criterion: browser presence posts go to `/client/presence` with the
// Bearer token and `keepalive: true`, the presence in the body. Two
// presences, so a body that ignores its argument fails.
#[test]
fn test_browser_presence_post_carries_the_bearer_and_keepalive() {
    let settings = active_with_token("http://localhost:8080", Some("tok-123"));
    for presence in [ClientPresence::Background, ClientPresence::Foreground] {
        let post = browser_presence_post(&settings, presence);
        assert!(post.is_ok(), "a paired browser posts: {post:?}");
        let post = post.ok();

        assert_eq!(
            post.as_ref().map(|p| p.url.as_str()),
            Some("http://localhost:8080/client/presence")
        );
        assert_eq!(
            post.as_ref().map(|p| p.authorization.as_str()),
            Some("Bearer tok-123")
        );
        assert_eq!(post.as_ref().map(|p| p.keepalive), Some(true));
        assert_eq!(
            post.and_then(|p| serde_json::from_str::<PresenceRequest>(&p.body).ok()),
            Some(PresenceRequest { presence })
        );
    }
}

// Criterion (non-nominal): an unpaired browser posts nothing — "not
// paired"; with no backend at all, "no backend configured".
#[test]
fn test_browser_presence_post_fails_like_every_guarded_call() {
    let unpaired = active_with_token("http://localhost:8080", None);
    let outcome = browser_presence_post(&unpaired, ClientPresence::Foreground);
    assert!(
        outcome.as_ref().is_err_and(BackendError::is_not_paired),
        "got {outcome:?}"
    );

    let outcome = browser_presence_post(&AppSettings::default(), ClientPresence::Foreground);
    assert_eq!(
        outcome.map_err(|e| e.to_string()),
        Err(NO_BACKEND_CONFIGURED.to_string())
    );
}

// ---- #160: config sync — push only what the backend has not acknowledged ----

/// What the backend answers to a push of Salon's default config.
const SALON_CONFIG: &str = r#"{"name":"Salon","restore_during_playback":true,"auto_reconnect":true,"spotify_volume_lock":false}"#;

// Criterion: with nothing pending, a sync **reads** `GET /config` (with the
// bearer) and adopts it — the phone included, which used to re-push its
// stored copy over what the browser had set.
#[tokio::test]
async fn test_sync_config_reads_and_adopts_when_nothing_is_pending() {
    let _guard = SETTINGS_GUARD.lock().await;
    let (base, served) = canned_backend("200 OK", BACKEND_CONFIG)
        .await
        .expect("start the canned backend");
    let mut settings = active_with_token(&base, Some("tok-123"));

    let outcome = sync_config(&mut settings).await;
    let request = served_within_bound(served).await;
    crate::settings::set_current(AppSettings::default());

    assert_eq!(outcome.map_err(|e| e.to_string()), Ok(()));
    let request = request.unwrap_or_else(|e| e);
    assert!(
        request.starts_with("GET /config "),
        "nothing pending: the sync reads, got {request}"
    );
    assert_eq!(
        header_value(&request, "authorization").as_deref(),
        Some("Bearer tok-123")
    );
    assert_eq!(
        settings.active_backend().map(|b| (
            b.name.as_str(),
            b.restore_during_playback,
            b.auto_reconnect
        )),
        Some(("blue2th-PC", false, true)),
        "the backend's config is adopted"
    );
}

// Criterion: a pending change is **pushed**, and the acknowledgement clears
// the pending mark — nothing is adopted over it.
#[tokio::test]
async fn test_sync_config_pushes_a_pending_change_and_clears_it() {
    let _guard = SETTINGS_GUARD.lock().await;
    let (base, served) = canned_backend("200 OK", SALON_CONFIG)
        .await
        .expect("start the canned backend");
    let mut settings = active_with_token(&base, Some("tok-123"));
    if let Some(salon) = settings.backends.first_mut() {
        salon.config_pending = true;
    }

    let outcome = sync_config(&mut settings).await;
    let request = served_within_bound(served).await;
    crate::settings::set_current(AppSettings::default());

    assert_eq!(outcome.map_err(|e| e.to_string()), Ok(()));
    let request = request.unwrap_or_else(|e| e);
    assert!(
        request.starts_with("POST /config "),
        "a pending change is pushed, got {request}"
    );
    assert!(
        request.contains(r#""name":"Salon""#),
        "the push carries the entry's own name, got {request}"
    );
    assert_eq!(
        settings
            .active_backend()
            .map(|b| (b.name.as_str(), b.config_pending)),
        Some(("Salon", false)),
        "acknowledged: no longer pending"
    );
}

// Criterion: a push the backend refuses leaves the change pending, so it
// goes out at the next sync — the reason the phone used to re-push.
#[tokio::test]
async fn test_sync_config_keeps_a_change_pending_when_the_push_fails() {
    let _guard = SETTINGS_GUARD.lock().await;
    let (base, served) = canned_backend("500 Internal Server Error", "boom")
        .await
        .expect("start the canned backend");
    let mut settings = active_with_token(&base, Some("tok-123"));
    if let Some(salon) = settings.backends.first_mut() {
        salon.config_pending = true;
    }

    let outcome = sync_config(&mut settings).await;
    let request = served_within_bound(served).await;
    crate::settings::set_current(AppSettings::default());

    assert!(outcome.is_err(), "the refusal is surfaced");
    let request = request.unwrap_or_else(|e| e);
    assert!(
        request.starts_with("POST /config "),
        "a pending change is pushed, got {request}"
    );
    assert_eq!(
        settings.active_backend().map(|b| b.config_pending),
        Some(true),
        "refused: still pending"
    );
}

// Criterion: activating a backend the app holds nothing unsent for **reads**
// its config instead of pushing the app's copy over it. Near-miss: the
// pre-#160 activation, which re-imposed a stale name and stale toggles.
#[tokio::test]
async fn test_activate_backend_reads_the_config_of_a_synced_backend() {
    let _guard = SETTINGS_GUARD.lock().await;
    let (arriving, served) = canned_backend("200 OK", BACKEND_CONFIG)
        .await
        .expect("start the canned backend");
    // Leaving a backend on port 1, which is never listening: its pause is
    // refused at once, and the arriving backend's request is what is read.
    let mut settings = active_with_token("http://127.0.0.1:1", None);
    settings.backends.push(crate::settings::BackendEntry {
        token: Some("arriving-token".to_string()),
        ..entry("Bureau", &arriving)
    });

    let _ = activate_backend(&mut settings, 1).await;
    let request = served_within_bound(served).await;
    crate::settings::set_current(AppSettings::default());

    let request = request.unwrap_or_else(|e| e);
    assert!(
        request.starts_with("GET /config "),
        "activating a synced backend reads its config, got {request}"
    );
    assert_eq!(
        settings
            .active_backend()
            .map(|b| (b.name.as_str(), b.restore_during_playback)),
        Some(("blue2th-PC", false)),
        "the arriving backend's config is adopted"
    );
}
