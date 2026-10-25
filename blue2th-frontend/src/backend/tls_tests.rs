// SPDX-License-Identifier: MIT OR Apache-2.0

//! The backend calls over a real TLS handshake (#196), against a loopback
//! backend whose key is made per test (`crate::tls::testing`). No real backend
//! and no hardware: the point is which key each call accepts, and what goes on
//! the wire once it has.
//!
//! The settings cache is process-wide: every test that touches it holds
//! [`SETTINGS_GUARD`], shared with `backend::tests`, and leaves it empty.

use std::future::Future;

use super::tests::SETTINGS_GUARD;
use super::*;
use crate::settings::{AppSettings, BackendEntry, PairingMethod};
use crate::tls::testing::{
    header_value, method_and_path, one_digit_off, ordinary, Conn, Seen, TestKey, TlsBackend,
    HEALTH_OK,
};

/// The token every paired entry of these tests holds.
const TOKEN: &str = "tok-123";

/// What the backend answers to `POST /api/pair`.
const PAIR_OK: &str = r#"{"token":"api-token-value"}"#;

/// A device address no action name can be confused with.
const DEVICE: &str = "11:22:33:44:55:66";

/// One entry at `url`, holding `token` and `pin`. Built by hand: a fixture must
/// not depend on the settings functions under test elsewhere.
fn entry(name: &str, url: &str, token: Option<&str>, pin: Option<&str>) -> BackendEntry {
    BackendEntry {
        name: name.to_string(),
        url: url.to_string(),
        restore_during_playback: true,
        auto_reconnect: true,
        token: token.map(str::to_string),
        pairing: PairingMethod::Code,
        id: None,
        config_pending: false,
        pin: pin.map(str::to_string),
    }
}

/// Settings holding one active entry at `url`.
fn active(url: &str, token: Option<&str>, pin: Option<&str>) -> AppSettings {
    AppSettings {
        backends: vec![entry("Salon", url, token, pin)],
        active: Some(0),
        auto_repair_url: true,
        discovery_adds_backends: true,
    }
}

/// A loopback TLS backend presenting `key` on `connections` successive
/// connections, each answering `200 OK` with `payload`.
async fn backend_presenting(
    key: &TestKey,
    connections: usize,
    payload: &'static str,
) -> Option<TlsBackend> {
    let plan = (0..connections)
        .map(|_| ordinary(key).map(|presents| Conn::ok(presents, payload)))
        .collect::<Option<Vec<_>>>()?;
    TlsBackend::start(plan).await
}

// ---- The probe ----

// Criterion: every later call goes through a client that accepts only the
// pinned key — and the probe of a paired entry carries its bearer.
#[tokio::test]
async fn test_ping_backend_with_the_pinned_key_answers_and_carries_the_bearer() {
    let _guard = SETTINGS_GUARD.lock().await;
    let key = TestKey::generate().expect("generate a key");
    let backend = backend_presenting(&key, 1, HEALTH_OK)
        .await
        .expect("start the TLS backend");
    crate::settings::set_current(active(&backend.url, Some(TOKEN), Some(&key.pin_hex())));

    let health = ping_backend().await;
    let requests = backend.requests_within_bound().await;
    crate::settings::set_current(AppSettings::default());

    assert!(
        health.is_ok(),
        "the pinned key must be accepted: {health:?}"
    );
    assert_eq!(
        requests
            .first()
            .and_then(|r| header_value(r, "authorization")),
        Some(format!("Bearer {TOKEN}"))
    );
}

// Criterion: a refused key is `identity_changed` — and the token never goes on
// the refused connection. Near-miss: another machine at the address, with a
// certificate valid in every other respect.
#[tokio::test]
async fn test_ping_backend_against_another_key_is_identity_changed() {
    let _guard = SETTINGS_GUARD.lock().await;
    let key = TestKey::generate().expect("generate a key");
    let paired_with = TestKey::generate().expect("generate the paired key");
    let backend = backend_presenting(&key, 1, HEALTH_OK)
        .await
        .expect("start the TLS backend");
    crate::settings::set_current(active(
        &backend.url,
        Some(TOKEN),
        Some(&paired_with.pin_hex()),
    ));

    let health = ping_backend().await;
    let requests = backend.requests_within_bound().await;
    crate::settings::set_current(AppSettings::default());

    assert_eq!(
        health.map_err(|e| e.is_identity_changed()).map(|_| ()),
        Err(true),
        "another key is an identity change, not an unreachable backend"
    );
    assert_eq!(
        requests,
        Vec::<String>::new(),
        "nothing — the token least of all — may follow a refused handshake"
    );
}

// Criterion (guard, an identity change is not any failure): a refused
// connection stays a plain transport error. Near-miss: a classification on
// "any TLS or connect error", which passes the test above.
#[tokio::test]
async fn test_ping_backend_on_a_refused_connection_is_not_identity_changed() {
    let _guard = SETTINGS_GUARD.lock().await;
    let key = TestKey::generate().expect("generate a key");
    // Port 1 is privileged and unbound: the connection is refused at once.
    crate::settings::set_current(active(
        "https://127.0.0.1:1",
        Some(TOKEN),
        Some(&key.pin_hex()),
    ));

    let health = ping_backend().await;
    crate::settings::set_current(AppSettings::default());

    assert_eq!(
        health.map_err(|e| e.is_identity_changed()).map(|_| ()),
        Err(false),
        "an unreachable backend is red, never violet"
    );
}

// Criterion (guard, the token never travels unverified): the probe of an
// `https://` entry with a token and no pin — an HTTP-era entry whose scheme a
// discovery repair changed — runs unpinned, **without** `Authorization`.
// Near-miss: a probe that attaches the token whenever there is one, which
// passes every other probe test.
#[tokio::test]
async fn test_ping_backend_without_a_pin_runs_unpinned_and_sends_no_token() {
    let _guard = SETTINGS_GUARD.lock().await;
    let key = TestKey::generate().expect("generate a key");
    let backend = backend_presenting(&key, 1, HEALTH_OK)
        .await
        .expect("start the TLS backend");
    crate::settings::set_current(active(&backend.url, Some(TOKEN), None));

    let health = ping_backend().await;
    let requests = backend.requests_within_bound().await;
    crate::settings::set_current(AppSettings::default());

    assert!(
        health.is_ok(),
        "with no pin any key is accepted, so the backend reads as alive: {health:?}"
    );
    assert_eq!(requests.len(), 1, "the probe reached the backend");
    assert_eq!(
        requests
            .first()
            .and_then(|r| header_value(r, "authorization")),
        None,
        "no token on a connection no pin verified"
    );
}

// Criterion (guard, a malformed pin is no pin): an entry whose stored pin is
// empty, or not hex, is probed like one with no pin — unpinned, without the
// token. Pinned reading: "treated as no pin" — the probe answers, and the dot
// is amber (not paired) rather than violet. Near-miss: a probe that sends the
// token whenever `pin` is `Some(_)`, whatever it holds — the empty value is a
// wildcard (CLAUDE.md).
#[tokio::test]
async fn test_ping_backend_with_a_malformed_pin_sends_no_token() {
    let _guard = SETTINGS_GUARD.lock().await;
    let key = TestKey::generate().expect("generate a key");
    let mut carried = Vec::new();
    for stored in ["", "not-a-pin"] {
        let backend = backend_presenting(&key, 1, HEALTH_OK)
            .await
            .expect("start the TLS backend");
        crate::settings::set_current(active(&backend.url, Some(TOKEN), Some(stored)));

        let health = ping_backend().await;
        let requests = backend.requests_within_bound().await;
        crate::settings::set_current(AppSettings::default());

        carried.push((
            stored,
            health.is_ok(),
            requests.len(),
            requests
                .first()
                .and_then(|r| header_value(r, "authorization")),
        ));
    }

    assert_eq!(
        carried,
        vec![("", true, 1, None), ("not-a-pin", true, 1, None)],
        "(stored pin, probe answered, requests read, Authorization sent)"
    );
}

// ---- The settings page's `Test` ----

// Criterion: `test_backend` goes through the entry's pin: the pinned key
// answers…
#[tokio::test]
async fn test_test_backend_with_the_entry_pin_answers() {
    let key = TestKey::generate().expect("generate a key");
    let backend = backend_presenting(&key, 1, HEALTH_OK)
        .await
        .expect("start the TLS backend");

    let health = test_backend(&backend.url, Some(&key.pin_hex())).await;
    let requests = backend.requests_within_bound().await;

    assert!(
        health.is_ok(),
        "the pinned key must be accepted: {health:?}"
    );
    assert_eq!(requests.len(), 1);
}

// Criterion: …and another key is an identity change, not an unreachable
// backend.
#[tokio::test]
async fn test_test_backend_against_another_key_is_identity_changed() {
    let key = TestKey::generate().expect("generate a key");
    let backend = backend_presenting(&key, 1, HEALTH_OK)
        .await
        .expect("start the TLS backend");

    let health = test_backend(&backend.url, Some(&one_digit_off(&key.pin_hex()))).await;
    let requests = backend.requests_within_bound().await;

    assert_eq!(
        health.map_err(|e| e.is_identity_changed()).map(|_| ()),
        Err(true)
    );
    assert_eq!(requests, Vec::<String>::new());
}

// ---- Trust on first use: the pairing ----

// Criterion: `check_backend_protocol` records the SPKI of its `/health`
// handshake in the `CompatibleBackend` it returns. Compared with the pin
// `sha2` computes, never with another value of the code under test.
#[tokio::test]
async fn test_check_backend_protocol_records_the_key_of_its_health_handshake() {
    let key = TestKey::generate().expect("generate a key");
    let backend = backend_presenting(&key, 1, HEALTH_OK)
        .await
        .expect("start the TLS backend");
    let url = backend.url.clone();

    let checked = check_backend_protocol(&url).await;
    let _ = backend.requests_within_bound().await;

    assert_eq!(
        checked.map_err(|e| e.to_string()),
        Ok(CompatibleBackend {
            url,
            pin: Some(key.pin_hex()),
        })
    );
}

// Criterion: `pair` returns the token **and** the recorded pin, over a `/pair`
// connection that presented the same key — and still sends no bearer.
#[tokio::test]
async fn test_pair_returns_the_token_and_the_pin_of_the_checked_handshake() {
    let key = TestKey::generate().expect("generate a key");
    let presents = ordinary(&key).expect("present the key");
    let backend = TlsBackend::start(vec![
        Conn::ok(std::sync::Arc::clone(&presents), HEALTH_OK),
        Conn::ok(presents, PAIR_OK),
    ])
    .await
    .expect("start the TLS backend");

    let checked = check_backend_protocol(&backend.url).await;
    assert!(checked.is_ok(), "the check must pass: {checked:?}");
    let paired = match checked {
        Ok(compatible) => pair(&compatible, "K7M2QX").await,
        Err(e) => Err(e),
    };
    let requests = backend.requests_within_bound().await;

    assert_eq!(
        paired.map_err(|e| e.to_string()),
        Ok(Paired {
            token: "api-token-value".to_string(),
            pin: Some(key.pin_hex()),
        })
    );
    assert_eq!(
        requests
            .iter()
            .map(|r| method_and_path(r))
            .collect::<Vec<_>>(),
        vec![
            ("GET".to_string(), "/health".to_string()),
            ("POST".to_string(), "/api/pair".to_string()),
        ]
    );
    assert_eq!(
        requests
            .get(1)
            .and_then(|r| header_value(r, "authorization")),
        None,
        "pairing is the one call made without a bearer"
    );
}

// Criterion (guard, TOFU holds across the pairing): the key changes between the
// `/health` handshake and the `/pair` one — the pairing fails with the
// identity-changed error, the code never reaches the second key, and no token
// comes back. Near-miss: a `pair` that opens an unpinned connection, which
// passes the nominal test above.
#[tokio::test]
async fn test_pair_against_a_key_changed_since_the_health_check_is_identity_changed() {
    let key = TestKey::generate().expect("generate a key");
    let swapped = TestKey::generate().expect("generate another key");
    let backend = TlsBackend::start(vec![
        Conn::ok(ordinary(&key).expect("present the first key"), HEALTH_OK),
        Conn::ok(ordinary(&swapped).expect("present the second key"), PAIR_OK),
    ])
    .await
    .expect("start the TLS backend");

    let checked = check_backend_protocol(&backend.url).await;
    assert!(checked.is_ok(), "the check must pass: {checked:?}");
    let paired = match checked {
        Ok(compatible) => pair(&compatible, "K7M2QX").await,
        Err(e) => Err(e),
    };
    let requests = backend.requests_within_bound().await;

    assert_eq!(
        paired.map_err(|e| e.is_identity_changed()),
        Err(true),
        "a key change mid-pairing is an identity change, and yields no token"
    );
    assert_eq!(
        requests
            .iter()
            .map(|r| method_and_path(r))
            .collect::<Vec<_>>(),
        vec![("GET".to_string(), "/health".to_string())],
        "the code must never reach the second key"
    );
}

// ---- Paired, by target: the guarded calls fail fast ----

// Criterion (guard, the paired rule): in a debug build an `https://` entry with
// a token but no valid pin is not paired, so a guarded call fails fast with
// `not_paired` — no request reaches a backend that is listening. Near-misses:
// the token alone, which is today's rule; a pin that is `Some("")`, which an
// `is_some()` check takes for a pin.
#[tokio::test]
async fn test_a_guarded_call_on_an_https_entry_without_a_valid_pin_fails_fast_as_not_paired() {
    let _guard = SETTINGS_GUARD.lock().await;
    let key = TestKey::generate().expect("generate a key");
    let mut outcomes = Vec::new();
    for stored in [None, Some("")] {
        let backend = backend_presenting(&key, 1, r#"{"speakers":[],"routing":"idle"}"#)
            .await
            .expect("start the TLS backend");
        crate::settings::set_current(active(&backend.url, Some(TOKEN), stored));

        let outcome = fetch_targets().await;
        let requests = backend.requests_within_bound().await;
        crate::settings::set_current(AppSettings::default());

        outcomes.push((
            stored,
            outcome.err().map(|e| e.is_not_paired()),
            requests.len(),
        ));
    }

    assert_eq!(
        outcomes,
        vec![(None, Some(true), 0), (Some(""), Some(true), 0)],
        "(stored pin, failed as not paired, requests read)"
    );
}

// ---- Every call goes through the entry's pin ----

/// A guarded call, run against the settings it is handed (which are also the
/// process-wide ones), its result reduced to success or failure.
type Call = Box<dyn FnOnce(AppSettings) -> Running>;

/// A call under way.
type Running = std::pin::Pin<Box<dyn Future<Output = Result<(), BackendError>>>>;

/// One row of a forwarding table: the call, and the `(method, path)` and
/// `Authorization` it must send once the handshake is accepted.
struct Row {
    method: &'static str,
    path: &'static str,
    authorization: Option<&'static str>,
    call: Call,
}

macro_rules! row {
    ($method:literal, $path:literal, $authorization:expr, |$settings:pat_param| $call:expr) => {
        Row {
            method: $method,
            path: $path,
            authorization: $authorization,
            call: Box::new(|$settings: AppSettings| {
                Box::pin(async move { $call.await.map(|_| ()) }) as Running
            }),
        }
    };
}

/// Every call the app makes to its active backend, plus `test_backend` and the
/// config read and push, which take an explicit address or settings.
fn every_call() -> Vec<Row> {
    const BEARER: Option<&str> = Some("Bearer tok-123");
    vec![
        row!("GET", "/health", BEARER, |_s| ping_backend()),
        row!("GET", "/health", None, |s| {
            let url = s
                .backends
                .first()
                .map(|b| b.url.clone())
                .unwrap_or_default();
            let pin = s.backends.first().and_then(|b| b.pin.clone());
            async move { test_backend(&url, pin.as_deref()).await }
        }),
        row!(
            "POST",
            "/api/devices/11:22:33:44:55:66/connect",
            BEARER,
            |_s| { connect_device(DEVICE) }
        ),
        row!(
            "POST",
            "/api/devices/11:22:33:44:55:66/disconnect",
            BEARER,
            |_s| { disconnect_device(DEVICE) }
        ),
        row!("GET", "/api/devices", BEARER, |_s| fetch_devices()),
        row!("GET", "/api/scan", BEARER, |_s| scan_devices()),
        row!(
            "POST",
            "/api/devices/11:22:33:44:55:66/select",
            BEARER,
            |_s| { select_target(DEVICE) }
        ),
        row!(
            "POST",
            "/api/devices/11:22:33:44:55:66/deselect",
            BEARER,
            |_s| { deselect_target(DEVICE) }
        ),
        row!(
            "POST",
            "/api/devices/11:22:33:44:55:66/offset",
            BEARER,
            |_s| { set_offset(DEVICE, 120) }
        ),
        row!("POST", "/api/play", BEARER, |_s| play()),
        row!("POST", "/api/pause", BEARER, |_s| pause()),
        row!("POST", "/api/stop", BEARER, |_s| stop()),
        row!("GET", "/api/playback", BEARER, |_s| playback_state()),
        row!("POST", "/api/volume", BEARER, |_s| set_volume(0.5)),
        row!("GET", "/api/targets", BEARER, |_s| fetch_targets()),
        row!("POST", "/api/spotify/start", BEARER, |_s| start_spotify()),
        row!("POST", "/api/spotify/stop", BEARER, |_s| stop_spotify()),
        row!("GET", "/api/spotify/status", BEARER, |_s| spotify_status()),
        row!("GET", "/api/spotify/auth/url", BEARER, |_s| {
            spotify_auth_url()
        }),
        row!("POST", "/api/spotify/auth/callback", BEARER, |_s| {
            spotify_auth_callback("the-code", "the-state")
        }),
        row!("GET", "/api/spotify/auth/status", BEARER, |_s| {
            spotify_auth_status()
        }),
        row!("GET", "/api/spotify/now-playing", BEARER, |_s| {
            subscribe_now_playing(|_| {})
        }),
        row!("POST", "/api/spotify/previous", BEARER, |_s| {
            spotify_transport(SpotifyAction::Previous)
        }),
        row!("POST", "/api/spotify/play", BEARER, |_s| {
            spotify_transport(SpotifyAction::Play)
        }),
        row!("POST", "/api/spotify/pause", BEARER, |_s| {
            spotify_transport(SpotifyAction::Pause)
        }),
        row!("POST", "/api/spotify/next", BEARER, |_s| {
            spotify_transport(SpotifyAction::Next)
        }),
        row!("GET", "/api/config", BEARER, |s| async move {
            fetch_config(&s).await
        }),
        row!("POST", "/api/config", BEARER, |mut s| async move {
            if let Some(salon) = s.backends.first_mut() {
                salon.config_pending = true;
            }
            sync_config(&mut s).await
        }),
    ]
}

/// Run `call` against a TLS backend presenting `key`, the active entry paired
/// with [`TOKEN`] and pinned to `pin`: its outcome, and the requests the
/// backend read. The caller holds [`SETTINGS_GUARD`]; the settings are left
/// empty behind.
async fn over_tls(key: &TestKey, pin: &str, call: Call) -> (Result<(), BackendError>, Vec<String>) {
    let Some(backend) = backend_presenting(key, 1, "{}").await else {
        return (Err(BackendError::new("start the TLS backend")), Vec::new());
    };
    let settings = active(&backend.url, Some(TOKEN), Some(pin));
    // Owned copy: the cache and the call each hold the settings.
    crate::settings::set_current(settings.clone());
    let outcome = call(settings).await;
    let requests = backend.requests_within_bound().await;
    crate::settings::set_current(AppSettings::default());
    (outcome, requests)
}

// Criterion: every request the app makes goes through the client built for the
// entry it targets, with that entry's pin — one row per call, so no mapping is
// left unguarded. With the pinned key, each call reaches the backend with what
// it always sent. Near-miss: a call left on a client with no TLS at all, which
// never reaches an `https://` backend.
#[tokio::test]
async fn test_every_call_reaches_the_backend_through_the_pinned_key() {
    let _guard = SETTINGS_GUARD.lock().await;
    let key = TestKey::generate().expect("generate a key");
    let pin = key.pin_hex();

    let mut wrong = Vec::new();
    for row in every_call() {
        let (_, requests) = over_tls(&key, &pin, row.call).await;
        let sent: Vec<(String, String, Option<String>)> = requests
            .iter()
            .map(|r| {
                let (method, path) = method_and_path(r);
                (method, path, header_value(r, "authorization"))
            })
            .collect();
        let expected = vec![(
            row.method.to_string(),
            row.path.to_string(),
            row.authorization.map(str::to_string),
        )];
        if sent != expected {
            wrong.push(format!("expected {expected:?}, sent {sent:?}"));
        }
    }
    assert_eq!(wrong, Vec::<String>::new());
}

// Criterion: …and with another key, each call fails as `identity_changed`
// and sends nothing — the token never goes on a connection the entry's pin did
// not verify. Near-miss: a call on an unpinned client, which the table above
// accepts.
#[tokio::test]
async fn test_every_call_refuses_another_key_as_identity_changed() {
    let _guard = SETTINGS_GUARD.lock().await;
    let key = TestKey::generate().expect("generate a key");
    let pin = one_digit_off(&key.pin_hex());

    let mut wrong = Vec::new();
    for row in every_call() {
        let label = format!("{} {}", row.method, row.path);
        let (outcome, requests) = over_tls(&key, &pin, row.call).await;
        let identity_changed = outcome
            .as_ref()
            .err()
            .map(BackendError::is_identity_changed);
        if identity_changed != Some(true) || !requests.is_empty() {
            wrong.push(format!(
                "{label}: identity_changed {identity_changed:?}, {} request(s) read",
                requests.len()
            ));
        }
    }
    assert_eq!(wrong, Vec::<String>::new());
}

// ---- The calls aimed at the backend being left ----

/// Switch from a TLS backend presenting `key`, whose entry is pinned to
/// `leaving_pin`, to an entry nothing answers: what each connection to the
/// backend being left came to.
async fn switch_away(key: &TestKey, leaving_pin: &str) -> Vec<Seen> {
    let Some(leaving) = backend_presenting(key, 1, "{}").await else {
        return Vec::new();
    };
    let arriving = TestKey::generate().map(|k| k.pin_hex()).unwrap_or_default();
    let mut settings = AppSettings {
        backends: vec![
            entry(
                "Salon",
                &leaving.url,
                Some("leaving-token"),
                Some(leaving_pin),
            ),
            // Port 1: the sync with the backend switched to fails at once; the
            // pause of the one being left is what these tests read.
            entry(
                "Bureau",
                "https://127.0.0.1:1",
                Some("arriving-token"),
                Some(&arriving),
            ),
        ],
        active: Some(0),
        auto_repair_url: true,
        discovery_adds_backends: true,
    };
    let _ = activate_backend(&mut settings, 1).await;
    let seen = leaving.within_bound().await;
    crate::settings::set_current(AppSettings::default());
    seen
}

// Criterion: the pause sent to the backend being left goes through **its**
// entry's pin, with its own token.
#[tokio::test]
async fn test_activate_backend_pauses_the_previous_backend_through_its_own_pin() {
    let _guard = SETTINGS_GUARD.lock().await;
    let key = TestKey::generate().expect("generate a key");

    let requests: Vec<String> = switch_away(&key, &key.pin_hex())
        .await
        .into_iter()
        .filter_map(Result::ok)
        .collect();

    assert_eq!(
        requests
            .iter()
            .map(|r| (method_and_path(r), header_value(r, "authorization")))
            .collect::<Vec<_>>(),
        vec![(
            ("POST".to_string(), "/api/spotify/pause".to_string()),
            Some("Bearer leaving-token".to_string())
        )]
    );
}

// Criterion: …and a backend being left that presents another key gets nothing,
// its token least of all: the one connection attempted is refused at the
// handshake. Near-miss: a pause that never connects, which sends nothing too.
#[tokio::test]
async fn test_activate_backend_sends_nothing_to_a_previous_backend_with_another_key() {
    let _guard = SETTINGS_GUARD.lock().await;
    let key = TestKey::generate().expect("generate a key");

    let seen = switch_away(&key, &one_digit_off(&key.pin_hex())).await;

    assert_eq!(
        seen.iter().map(Result::is_ok).collect::<Vec<_>>(),
        vec![false],
        "one handshake, attempted and refused: {seen:?}"
    );
}

/// Delete the one entry, at a TLS backend presenting `key` and pinned to
/// `pin`: what each connection to the released backend came to.
async fn remove_the_only_entry(key: &TestKey, pin: &str) -> Vec<Seen> {
    let Some(backend) = backend_presenting(key, 3, "{}").await else {
        return Vec::new();
    };
    let mut settings = active(&backend.url, Some(TOKEN), Some(pin));
    let _ = remove_backend(&mut settings, 0).await;
    let seen = backend.within_bound().await;
    crate::settings::set_current(AppSettings::default());
    seen
}

// Criterion: releasing a deleted backend goes through the deleted entry's pin:
// all three steps reach it, with its token.
#[tokio::test]
async fn test_remove_backend_releases_the_backend_through_its_own_pin() {
    let _guard = SETTINGS_GUARD.lock().await;
    let key = TestKey::generate().expect("generate a key");

    let requests: Vec<String> = remove_the_only_entry(&key, &key.pin_hex())
        .await
        .into_iter()
        .filter_map(Result::ok)
        .collect();

    assert_eq!(
        requests
            .iter()
            .map(|r| (method_and_path(r).1, header_value(r, "authorization")))
            .collect::<Vec<_>>(),
        vec![
            (
                "/api/spotify/pause".to_string(),
                Some(format!("Bearer {TOKEN}"))
            ),
            ("/api/stop".to_string(), Some(format!("Bearer {TOKEN}"))),
            (
                "/api/spotify/stop".to_string(),
                Some(format!("Bearer {TOKEN}"))
            ),
        ]
    );
}

// Criterion: …and a deleted backend that presents another key gets nothing:
// each of the three steps is refused at the handshake. Near-miss: a release
// that never connects, which sends nothing too.
#[tokio::test]
async fn test_remove_backend_sends_nothing_to_a_backend_with_another_key() {
    let _guard = SETTINGS_GUARD.lock().await;
    let key = TestKey::generate().expect("generate a key");

    let seen = remove_the_only_entry(&key, &one_digit_off(&key.pin_hex())).await;

    assert_eq!(
        seen.iter().map(Result::is_ok).collect::<Vec<_>>(),
        vec![false, false, false],
        "three handshakes, each attempted and refused: {seen:?}"
    );
}
