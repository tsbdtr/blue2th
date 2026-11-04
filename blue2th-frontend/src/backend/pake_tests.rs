// SPDX-License-Identifier: MIT OR Apache-2.0

//! The native `pair()` of #197: SPAKE2 on the pairing code, bound to the key
//! the `/health` handshake presented, against a loopback backend that runs
//! the server half of `blue2th_proto::pake` (`crate::tls::testing`). No real
//! backend and no hardware.
//!
//! What `pair()` returns is what the screens store (`views/app.rs`,
//! `views/app_settings.rs`): an `Err` stores nothing.

use std::sync::Arc;

use blue2th_proto::pake::Binding;

use super::*;
use crate::tls::testing::{
    binding_of, header_value, method_and_path, ordinary, pake_responder, Conn, PakeFake,
    PlainBackend, TestKey, TlsBackend, HEALTH_V3, PAKE_TOKEN,
};

/// The code the fake backend's half runs on.
const CODE: &str = "K7M2QX";

/// The method and path of each request.
fn routes(requests: &[String]) -> Vec<(String, String)> {
    requests.iter().map(|r| method_and_path(r)).collect()
}

fn route(method: &str, path: &str) -> (String, String) {
    (method.to_string(), path.to_string())
}

/// A TLS backend presenting `key` on `connections` connections, each
/// answered by a PAKE fake behaving as `behaviour`, its confirmations over
/// `binding`.
async fn pake_backend(
    key: &TestKey,
    binding: Binding,
    behaviour: PakeFake,
    connections: usize,
) -> Option<TlsBackend> {
    let respond = pake_responder(CODE, binding, behaviour);
    let plan = (0..connections)
        .map(|_| ordinary(key).map(|presents| Conn::responding(presents, Arc::clone(&respond))))
        .collect::<Option<Vec<_>>>()?;
    TlsBackend::start(plan).await
}

/// Check the backend's contract, then pair with `code`.
async fn check_then_pair(url: &str, code: &str) -> Result<Paired, BackendError> {
    let compatible = check_backend_protocol(url).await?;
    pair(&compatible, code).await
}

// ---- The nominal exchange ----

// Criterion: the right code, no relay — the app runs `start` then `finish`
// over TLS, checks the backend's confirmation and returns the token **and**
// the pin of the handshake it ran the exchange over (64 lower-case hex). The
// fake checks the app's confirmation over that key's hash, so an app bound
// to anything else would be refused. No bearer on either call, and no
// `POST /api/pair`.
#[tokio::test]
async fn test_pair_over_tls_runs_start_and_finish_and_returns_the_token_and_the_pin() {
    let key = TestKey::generate().expect("generate a key");
    let backend = pake_backend(&key, binding_of(&key), PakeFake::Honest, 3)
        .await
        .expect("start the TLS backend");

    let paired = check_then_pair(&backend.url, CODE).await;
    let requests = backend.requests_within_bound().await;

    assert_eq!(
        paired.map_err(|e| e.to_string()),
        Ok(Paired {
            token: PAKE_TOKEN.to_string(),
            pin: Some(key.pin_hex()),
        })
    );
    assert_eq!(
        routes(&requests),
        vec![
            route("GET", "/health"),
            route("POST", "/api/pair/start"),
            route("POST", "/api/pair/finish"),
        ]
    );
    for request in requests.iter().skip(1) {
        assert_eq!(
            header_value(request, "authorization"),
            None,
            "pairing carries no bearer"
        );
    }
}

// Criterion: the code itself never goes on the wire — only SPAKE2 messages
// and a confirmation do. Near-miss: `POST /api/pair {code}`, which the
// relay of #197 reads in clear.
#[tokio::test]
async fn test_pair_over_tls_never_sends_the_code() {
    let key = TestKey::generate().expect("generate a key");
    let backend = pake_backend(&key, binding_of(&key), PakeFake::Honest, 3)
        .await
        .expect("start the TLS backend");

    let paired = check_then_pair(&backend.url, CODE).await;
    let requests = backend.requests_within_bound().await;

    assert!(paired.is_ok(), "the pairing must succeed: {paired:?}");
    assert_eq!(requests.len(), 3, "health, start, finish: {requests:?}");
    for request in &requests {
        assert!(
            !request.contains(CODE),
            "the code must not travel: {request}"
        );
    }
}

// Criterion: the PAKE password is the normalized code — a code typed in
// lower case, with spaces around it, pairs.
#[tokio::test]
async fn test_pair_with_a_lower_case_code_pairs() {
    let key = TestKey::generate().expect("generate a key");
    let backend = pake_backend(&key, binding_of(&key), PakeFake::Honest, 3)
        .await
        .expect("start the TLS backend");

    let paired = check_then_pair(&backend.url, " k7m2qx ").await;
    let _ = backend.requests_within_bound().await;

    assert_eq!(
        paired.map(|p| p.token).map_err(|e| e.to_string()),
        Ok(PAKE_TOKEN.to_string())
    );
}

// Criterion (debug, plain HTTP): over `http://` both sides bind the exchange
// to `blue2th-plain-http` — the fake checks the app's confirmation over the
// label — and the pairing yields the token with no pin.
#[tokio::test]
async fn test_pair_over_plain_http_binds_to_the_label_and_yields_no_pin() {
    let backend = PlainBackend::start(
        pake_responder(CODE, Binding::PlainHttp, PakeFake::Honest),
        2,
    )
    .await
    .expect("start the plain backend");
    let compatible = CompatibleBackend {
        url: backend.url.clone(),
        pin: None,
    };

    let paired = pair(&compatible, CODE).await;
    let requests = backend.requests_within_bound().await;

    assert_eq!(
        paired.map_err(|e| e.to_string()),
        Ok(Paired {
            token: PAKE_TOKEN.to_string(),
            pin: None,
        })
    );
    assert_eq!(
        routes(&requests),
        vec![
            route("POST", "/api/pair/start"),
            route("POST", "/api/pair/finish")
        ]
    );
}

// ---- Refusals ----

// Criterion: a backend `confirm` that does not match — the app returns no
// `Paired`, so it stores neither token nor pin, and reports a pairing
// refusal.
#[tokio::test]
async fn test_pair_with_a_wrong_backend_confirm_is_refused_and_yields_nothing() {
    let key = TestKey::generate().expect("generate a key");
    let backend = pake_backend(&key, binding_of(&key), PakeFake::WrongConfirm, 3)
        .await
        .expect("start the TLS backend");

    let paired = check_then_pair(&backend.url, CODE).await;
    let requests = backend.requests_within_bound().await;

    assert_eq!(
        routes(&requests).last(),
        Some(&route("POST", "/api/pair/finish")),
        "the exchange ran to its end"
    );
    assert_eq!(
        paired.map_err(|e| e.is_pairing_refused()),
        Err(true),
        "a confirmation that does not check out is a refusal"
    );
}

// Criterion: a 401 on `finish` — what a relay, or a wrong code, earns — is
// the pairing refusal.
#[tokio::test]
async fn test_pair_with_a_401_on_finish_is_the_pairing_refusal() {
    let key = TestKey::generate().expect("generate a key");
    let backend = pake_backend(&key, binding_of(&key), PakeFake::RefuseFinish, 3)
        .await
        .expect("start the TLS backend");

    let paired = check_then_pair(&backend.url, CODE).await;
    let _ = backend.requests_within_bound().await;

    assert_eq!(paired.map_err(|e| e.is_pairing_refused()), Err(true));
}

// Criterion: a 401 on `start` — no code active — is the same pairing
// refusal, and no `finish` follows.
#[tokio::test]
async fn test_pair_with_a_401_on_start_is_the_pairing_refusal() {
    let key = TestKey::generate().expect("generate a key");
    let backend = pake_backend(&key, binding_of(&key), PakeFake::RefuseStart, 3)
        .await
        .expect("start the TLS backend");

    let paired = check_then_pair(&backend.url, CODE).await;
    let requests = backend.requests_within_bound().await;

    assert_eq!(paired.map_err(|e| e.is_pairing_refused()), Err(true));
    assert_eq!(
        routes(&requests),
        vec![route("GET", "/health"), route("POST", "/api/pair/start")]
    );
}

// Criterion: a 429 on `start` is the rate-limit error, not the refusal, and
// no `finish` follows.
#[tokio::test]
async fn test_pair_with_a_429_on_start_is_the_rate_limit_error() {
    let key = TestKey::generate().expect("generate a key");
    let backend = pake_backend(&key, binding_of(&key), PakeFake::RateLimitStart, 3)
        .await
        .expect("start the TLS backend");

    let paired = check_then_pair(&backend.url, CODE).await;
    let requests = backend.requests_within_bound().await;

    assert_eq!(
        paired.map_err(|e| (e.is_pairing_rate_limited(), e.is_pairing_refused())),
        Err((true, false))
    );
    assert_eq!(
        routes(&requests),
        vec![route("GET", "/health"), route("POST", "/api/pair/start")]
    );
}

// ---- Guards ----

// Criterion (guard, the empty binding): an `https://` entry whose handshake
// presented no pin — the app refuses to run the exchange and sends nothing,
// to a backend that is listening and would answer.
#[tokio::test]
async fn test_pair_on_https_without_a_presented_pin_sends_nothing() {
    let key = TestKey::generate().expect("generate a key");
    let backend = pake_backend(&key, binding_of(&key), PakeFake::Honest, 2)
        .await
        .expect("start the TLS backend");
    let compatible = CompatibleBackend {
        url: backend.url.clone(),
        pin: None,
    };

    let paired = pair(&compatible, CODE).await;
    let requests = backend.requests_within_bound().await;

    assert!(paired.is_err(), "no exchange without a binding: {paired:?}");
    assert_eq!(requests, Vec::<String>::new(), "nothing may be sent");
}

// Criterion (guard, no fallback): a backend on protocol 3 (no
// `start`/`finish`) — the app says the backend must be updated, from the
// `/health` check, and issues no pairing call at all.
#[tokio::test]
async fn test_a_protocol_3_backend_is_too_old_and_receives_no_pairing_call() {
    let key = TestKey::generate().expect("generate a key");
    let backend = TlsBackend::start(vec![
        Conn::ok(ordinary(&key).expect("present the key"), HEALTH_V3),
        Conn::ok(
            ordinary(&key).expect("present the key"),
            r#"{"token":"legacy"}"#,
        ),
    ])
    .await
    .expect("start the TLS backend");

    let checked = check_backend_protocol(&backend.url).await;
    let requests = backend.requests_within_bound().await;

    assert_eq!(
        checked.err().and_then(|e| e.protocol_mismatch()),
        Some(ProtocolMismatch::BackendTooOld)
    );
    assert_eq!(routes(&requests), vec![route("GET", "/health")]);
}

// Criterion: the binding comes from the entry's scheme and the handshake's
// presented pin — the label over `http://`, the key hash over `https://` —
// and an `https://` entry with no valid pin has none: never an empty one.
#[test]
fn test_binding_for_follows_the_scheme_and_refuses_an_https_entry_without_a_pin() {
    let pin = "11".repeat(32);
    assert_eq!(
        binding_for("http://127.0.0.1:4000", None),
        Some(Binding::PlainHttp)
    );
    assert_eq!(
        binding_for("https://192.168.1.20:4000", Some(&pin)),
        Some(Binding::Spki([0x11; 32]))
    );
    for missing in [None, Some(""), Some(&pin[..63])] {
        assert_eq!(
            binding_for("https://192.168.1.20:4000", missing),
            None,
            "pin {missing:?} binds nothing"
        );
    }
}
