// SPDX-License-Identifier: MIT OR Apache-2.0

//! Integration tests for #197 — `POST /api/pair/start` and
//! `POST /api/pair/finish`, the PAKE the native app pairs through, bound to
//! the backend's TLS key; and `POST /api/pair`, which the browser keeps, now
//! held to the active code.
//!
//! The app's side is the real client half from `blue2th_proto::pake`, so the
//! routes are tested against the bytes the app computes. The peer address is
//! the `ConnectInfo` the listener attaches, set here by hand on each request.
//! The router is built store-free with an explicit token, like every route
//! test: no test may reach the operator's real one.

use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{HeaderMap, Request, StatusCode},
};
use blue2th_proto::pake::{
    check_confirm_b, confirm_a, from_hex, to_hex, Binding, ClientHalf, SharedKey, CONFIRM_LEN,
    MESSAGE_LEN,
};
use blue2th_proto::{
    PairFinishRequest, PairFinishResponse, PairRequest, PairStartRequest, PairStartResponse,
};
use blue2th_server::{
    auth::{AuthStore, PAIR_START_WINDOW},
    spotify_auth::SpotifyAuth,
};
use tower::ServiceExt; // for `oneshot`

/// The API token a pairing releases.
const TOKEN: &str = "test-api-token";

/// The backend's own key hash.
const OWN: Binding = Binding::Spki([0x11; 32]);

/// A relay's key hash: well-formed, and not the backend's.
const RELAY: Binding = Binding::Spki([0x22; 32]);

/// The phone's address, with the port its connection came from.
const PHONE: &str = "192.168.1.42:51234";

/// A router checking confirmations against `binding`, around `store`.
fn app_with(store: AuthStore, binding: Binding) -> axum::Router {
    blue2th_server::app_with_auth_store_and_binding(
        SpotifyAuth::with_config(None, "blue2th://spotify-callback".to_string()),
        store,
        binding,
    )
}

/// A TLS router (binding [`OWN`]) with `count` codes armed now, and the codes.
fn armed(count: u32) -> (axum::Router, Vec<String>) {
    let mut store = AuthStore::with_token(TOKEN);
    let codes = store.arm_pairing(count, std::time::SystemTime::now());
    (app_with(store, OWN), codes)
}

/// The first armed code, or an empty string no exchange pairs with.
fn first(codes: &[String]) -> String {
    codes.first().cloned().unwrap_or_default()
}

fn addr(text: &str) -> Option<SocketAddr> {
    text.parse().ok()
}

/// A request's answer: status, headers, body.
struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

/// `POST path` with a raw `body`, from `peer` when there is one, no bearer.
async fn post(
    app: &axum::Router,
    path: &str,
    body: &str,
    peer: Option<SocketAddr>,
) -> Result<Answer, String> {
    let mut request = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .map_err(|e| format!("build request: {e}"))?;
    if let Some(peer) = peer {
        request.extensions_mut().insert(ConnectInfo(peer));
    }
    // Cloned: the router shares its state, so every call sees what the
    // previous ones left.
    let response = app
        .clone()
        .oneshot(request)
        .await
        .map_err(|e| format!("router response: {e}"))?;
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .map_err(|e| format!("read body: {e}"))?;
    Ok(Answer {
        status,
        headers,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    })
}

fn json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

/// `POST /api/pair/start` with the app's message.
async fn start(app: &axum::Router, msg: &[u8], peer: Option<SocketAddr>) -> Result<Answer, String> {
    let body = json(&PairStartRequest { msg: to_hex(msg) });
    post(app, "/api/pair/start", &body, peer).await
}

/// `POST /api/pair/finish` with a session and a confirmation.
async fn finish(
    app: &axum::Router,
    session: &str,
    confirm: &[u8],
    peer: Option<SocketAddr>,
) -> Result<Answer, String> {
    let body = json(&PairFinishRequest {
        session: session.to_string(),
        confirm: to_hex(confirm),
    });
    post(app, "/api/pair/finish", &body, peer).await
}

/// `POST /api/pair` with a code, as the browser does.
async fn pair_code(
    app: &axum::Router,
    code: &str,
    peer: Option<SocketAddr>,
) -> Result<Answer, String> {
    let body = json(&PairRequest {
        code: code.to_string(),
    });
    post(app, "/api/pair", &body, peer).await
}

/// The app's side of one exchange once `start` answered 200.
struct Session {
    id: String,
    key: SharedKey,
}

/// Run the app's half on `code` through `start`. `Err` carries the refusal.
async fn open(
    app: &axum::Router,
    code: &str,
    peer: Option<SocketAddr>,
) -> Result<Result<Session, Answer>, String> {
    let (client, msg) = ClientHalf::start(code);
    let answer = start(app, &msg, peer).await?;
    if answer.status != StatusCode::OK {
        return Ok(Err(answer));
    }
    let started: PairStartResponse =
        serde_json::from_str(&answer.body).map_err(|e| format!("parse start: {e}"))?;
    let server_msg = from_hex(&started.msg, MESSAGE_LEN).ok_or("start msg is not hex")?;
    let key = client
        .finish(&server_msg)
        .map_err(|e| format!("finish the app's half: {e}"))?;
    Ok(Ok(Session {
        id: started.session,
        key,
    }))
}

/// A whole exchange on `code`, the confirmation computed over `binding`:
/// the `finish` answer, or the `start` refusal.
async fn exchange(
    app: &axum::Router,
    code: &str,
    binding: &Binding,
    peer: Option<SocketAddr>,
) -> Result<(Answer, Option<SharedKey>), String> {
    match open(app, code, peer).await? {
        Ok(session) => {
            let confirm = confirm_a(&session.key, binding);
            let answer = finish(app, &session.id, &confirm, peer).await?;
            Ok((answer, Some(session.key)))
        },
        Err(refused) => Ok((refused, None)),
    }
}

/// The status of a whole exchange.
async fn exchange_status(
    app: &axum::Router,
    code: &str,
    binding: &Binding,
    peer: Option<SocketAddr>,
) -> Result<StatusCode, String> {
    Ok(exchange(app, code, binding, peer).await?.0.status)
}

// ---- The exchange ----

// Criterion: the right code, no relay — `finish` answers `{confirm, token}`,
// and the backend's confirmation checks out on the app's side.
#[tokio::test]
async fn test_start_then_finish_with_the_right_code_answers_confirm_and_token() {
    let (app, codes) = armed(1);
    let (answer, key) = exchange(&app, &first(&codes), &OWN, addr(PHONE))
        .await
        .expect("run the exchange");
    assert_eq!(answer.status, StatusCode::OK, "body: {}", answer.body);

    let finished: PairFinishResponse =
        serde_json::from_str(&answer.body).expect("parse PairFinishResponse");
    assert_eq!(finished.token, TOKEN);
    let confirm = from_hex(&finished.confirm, CONFIRM_LEN);
    assert!(
        key.zip(confirm)
            .is_some_and(|(key, confirm)| check_confirm_b(&key, &OWN, &confirm)),
        "the backend's confirmation must check out over its own key hash"
    );
}

// Criterion: the PAKE password is the normalized code — a code typed in
// lower case pairs.
#[tokio::test]
async fn test_a_lower_case_code_pairs_through_the_exchange() {
    let (app, codes) = armed(1);
    let status = exchange_status(&app, &first(&codes).to_lowercase(), &OWN, addr(PHONE))
        .await
        .expect("run the exchange");
    assert_eq!(status, StatusCode::OK);
}

// Criterion: in the plain-HTTP debug mode both sides bind the exchange to
// the label, and it pairs.
#[tokio::test]
async fn test_a_plain_http_backend_pairs_over_the_label() {
    let mut store = AuthStore::with_token(TOKEN);
    let codes = store.arm_pairing(1, std::time::SystemTime::now());
    let app = app_with(store, Binding::PlainHttp);
    let status = exchange_status(&app, &first(&codes), &Binding::PlainHttp, addr(PHONE))
        .await
        .expect("run the exchange");
    assert_eq!(status, StatusCode::OK);
}

// Criterion (guard, relay): the right code, a confirmation over another key
// hash — 401, no token, and one failure counted: with one more failure the
// code still pairs (two < 3)...
#[tokio::test]
async fn test_a_relayed_finish_is_refused_and_counts_one_failure() {
    let (app, codes) = armed(1);
    let code = first(&codes);

    let (relayed, _) = exchange(&app, &code, &RELAY, addr(PHONE))
        .await
        .expect("relayed exchange");
    assert_eq!(relayed.status, StatusCode::UNAUTHORIZED);
    assert!(!relayed.body.contains(TOKEN), "no token: {}", relayed.body);

    let wrong = exchange_status(&app, "QQQQQQ", &OWN, addr(PHONE))
        .await
        .expect("wrong-code exchange");
    assert_eq!(wrong, StatusCode::UNAUTHORIZED);
    let right = exchange_status(&app, &code, &OWN, addr(PHONE))
        .await
        .expect("right exchange");
    assert_eq!(right, StatusCode::OK, "two failures leave the code active");
}

// ...and with two more failures it is gone: the relay counted.
#[tokio::test]
async fn test_a_relayed_finish_and_two_wrong_codes_cancel_the_code() {
    let (app, codes) = armed(1);
    let code = first(&codes);
    for binding_and_code in [(&RELAY, code.as_str()), (&OWN, "QQQQQQ"), (&OWN, "QQQQQQ")] {
        let (binding, tried) = binding_and_code;
        let status = exchange_status(&app, tried, binding, addr(PHONE))
            .await
            .expect("refused exchange");
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    let right = exchange_status(&app, &code, &OWN, addr(PHONE))
        .await
        .expect("right exchange, too late");
    assert_eq!(right, StatusCode::UNAUTHORIZED, "the third failure cancels");
}

// Criterion: a `finish` computed with a wrong code is refused with 401 and
// no token.
#[tokio::test]
async fn test_a_finish_with_a_wrong_code_is_unauthorised() {
    let (app, _codes) = armed(1);
    let (answer, _) = exchange(&app, "QQQQQQ", &OWN, addr(PHONE))
        .await
        .expect("wrong-code exchange");
    assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
    assert!(!answer.body.contains(TOKEN));
}

// Criterion (guard, the label): a confirmation over `blue2th-plain-http`
// sent to a TLS backend is refused.
#[tokio::test]
async fn test_a_tls_backend_refuses_a_confirmation_over_the_plain_label() {
    let (app, codes) = armed(1);
    let (answer, _) = exchange(&app, &first(&codes), &Binding::PlainHttp, addr(PHONE))
        .await
        .expect("label exchange");
    assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
    assert!(!answer.body.contains(TOKEN));
}

// Criterion: every refusal of `start` and `finish` answers the same status
// (401) and the same body as `POST /api/pair`'s refusal — wrong code,
// relay, unknown session, no active code: no caller can tell them apart.
#[tokio::test]
async fn test_every_refusal_reads_like_the_pair_route_refusal() {
    let (app, codes) = armed(1);
    let code = first(&codes);
    let pair = pair_code(&app, "QQQQQQ", addr(PHONE)).await.expect("pair");
    assert_eq!(pair.status, StatusCode::UNAUTHORIZED);

    let (wrong, _) = exchange(&app, "QQQQQQ", &OWN, addr(PHONE))
        .await
        .expect("wrong");
    let unknown = finish(&app, "deadbeefdeadbeef", &[0u8; CONFIRM_LEN], addr(PHONE))
        .await
        .expect("unknown session");

    let (app, codes) = armed(1);
    let (relay, _) = exchange(&app, &first(&codes), &RELAY, addr(PHONE))
        .await
        .expect("relay");

    let (spent, codes) = armed(1);
    let used = pair_code(&spent, &first(&codes), addr(PHONE))
        .await
        .expect("use");
    assert_eq!(used.status, StatusCode::OK);
    let (_, no_code_msg) = ClientHalf::start(&code);
    let no_code = start(&spent, &no_code_msg, addr(PHONE))
        .await
        .expect("start");

    for (what, answer) in [
        ("wrong code", &wrong),
        ("relay", &relay),
        ("unknown session", &unknown),
        ("no active code", &no_code),
    ] {
        assert_eq!(
            (answer.status, answer.body.as_str()),
            (StatusCode::UNAUTHORIZED, pair.body.as_str()),
            "{what} must read like POST /api/pair's refusal"
        );
    }
}

// ---- Failures that are not guesses ----

// Criterion: a `finish` on an unknown or replaced session counts no failure
// — three of them, then a valid exchange, pairs.
#[tokio::test]
async fn test_three_unknown_or_replaced_sessions_then_a_valid_exchange_pairs() {
    let (app, codes) = armed(1);
    let code = first(&codes);
    let replaced = open(&app, &code, addr(PHONE)).await.expect("first start");
    assert!(replaced.is_ok(), "the first start opens a session");
    let current = open(&app, &code, addr(PHONE)).await.expect("second start");
    assert!(current.is_ok(), "the second start replaces it");
    let (Ok(replaced), Ok(current)) = (replaced, current) else {
        return;
    };

    let attempts = [
        (replaced.id.clone(), confirm_a(&replaced.key, &OWN)),
        (
            "deadbeefdeadbeef".to_string(),
            confirm_a(&current.key, &OWN),
        ),
        (String::new(), confirm_a(&current.key, &OWN)),
    ];
    for (session, confirm) in attempts {
        let answer = finish(&app, &session, &confirm, addr(PHONE))
            .await
            .expect("finish");
        assert_eq!(
            answer.status,
            StatusCode::UNAUTHORIZED,
            "session {session:?}"
        );
    }

    let answer = finish(
        &app,
        &current.id,
        &confirm_a(&current.key, &OWN),
        addr(PHONE),
    )
    .await
    .expect("finish the open session");
    assert_eq!(answer.status, StatusCode::OK, "body: {}", answer.body);
}

// Criterion: `start` with no active code answers 401.
#[tokio::test]
async fn test_start_without_an_active_code_is_unauthorised() {
    let app = app_with(AuthStore::with_token(TOKEN), OWN);
    let (_, msg) = ClientHalf::start("K7M2QX");
    let answer = start(&app, &msg, addr(PHONE)).await.expect("start");
    assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
}

// Criterion: malformed `start` and `finish` bodies — not JSON, wrong hex,
// wrong length — answer 400 and count no failure: after every one of them
// and two real failures, the code still pairs.
#[tokio::test]
async fn test_malformed_start_and_finish_bodies_answer_400_and_count_nothing() {
    let (app, codes) = armed(1);
    let short_msg = "ab".repeat(MESSAGE_LEN - 1);
    let starts = [
        "{ not json }".to_string(),
        "{}".to_string(),
        r#"{"msg":"zz"}"#.to_string(),
        format!(r#"{{"msg":"{short_msg}"}}"#),
    ];
    for (n, body) in starts.iter().enumerate() {
        let peer = addr(&format!("10.0.1.{n}:4000"));
        let answer = post(&app, "/api/pair/start", body, peer)
            .await
            .expect("start");
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "start {body}");
    }
    let short_confirm = "ab".repeat(CONFIRM_LEN - 1);
    let finishes = [
        "{ not json }".to_string(),
        r#"{"session":"deadbeef"}"#.to_string(),
        format!(r#"{{"session":"deadbeef","confirm":"{short_confirm}"}}"#),
        r#"{"session":"deadbeef","confirm":"not hex"}"#.to_string(),
    ];
    for body in &finishes {
        let answer = post(&app, "/api/pair/finish", body, addr(PHONE))
            .await
            .expect("finish");
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "finish {body}");
    }

    for _ in 0..2 {
        let wrong = exchange_status(&app, "QQQQQQ", &OWN, addr(PHONE))
            .await
            .expect("wrong-code exchange");
        assert_eq!(wrong, StatusCode::UNAUTHORIZED);
    }
    let right = exchange_status(&app, &first(&codes), &OWN, addr("10.0.2.1:4000"))
        .await
        .expect("right exchange");
    assert_eq!(right, StatusCode::OK, "malformed bodies are not guesses");
}

// ---- The rate limit ----

// Criterion: from one source address, 5 `start` pass and the sixth answers
// 429 with a `Retry-After` in whole seconds — checked before the body is
// read, so a malformed sixth body is a 429 too, not a 400 — while another
// address is unaffected.
#[tokio::test]
async fn test_the_sixth_start_from_one_address_is_429_with_retry_after() {
    let (app, codes) = armed(1);
    let code = first(&codes);
    for n in 1..=5 {
        let opened = open(&app, &code, addr(PHONE)).await.expect("start");
        assert!(opened.is_ok(), "start {n} from the phone passes");
    }

    let sixth = post(&app, "/api/pair/start", "{ not json }", addr(PHONE))
        .await
        .expect("sixth start");
    assert_eq!(sixth.status, StatusCode::TOO_MANY_REQUESTS);
    let retry = sixth
        .headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    assert!(
        retry.is_some_and(|s| (1..=PAIR_START_WINDOW.as_secs()).contains(&s)),
        "Retry-After must be whole seconds within the window, got {:?}",
        sixth.headers.get("retry-after")
    );

    let other = open(&app, &code, addr("192.168.1.43:40000"))
        .await
        .expect("start from another address");
    assert!(other.is_ok(), "another address is not affected");
}

// Criterion (guard, no address is no exemption): requests with no known
// peer address are limited under one shared key.
#[tokio::test]
async fn test_starts_without_a_peer_address_share_one_limit() {
    let (app, codes) = armed(1);
    let code = first(&codes);
    for n in 1..=5 {
        let opened = open(&app, &code, None).await.expect("start");
        assert!(opened.is_ok(), "start {n} without an address passes");
    }
    let (_, msg) = ClientHalf::start(&code);
    let sixth = start(&app, &msg, None).await.expect("sixth start");
    assert_eq!(sixth.status, StatusCode::TOO_MANY_REQUESTS);
}

// Criterion: a 429 counts no failure — two real failures, a 429, and the
// code still pairs.
#[tokio::test]
async fn test_a_rate_limited_start_counts_no_failure() {
    let (app, codes) = armed(1);
    let flooder = addr("192.168.1.77:40000");
    for _ in 0..5 {
        let (_, msg) = ClientHalf::start("QQQQQQ");
        let _ = start(&app, &msg, flooder).await.expect("start");
    }
    let (_, msg) = ClientHalf::start("QQQQQQ");
    let limited = start(&app, &msg, flooder).await.expect("sixth start");
    assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);

    for _ in 0..2 {
        let wrong = exchange_status(&app, "QQQQQQ", &OWN, addr(PHONE))
            .await
            .expect("wrong-code exchange");
        assert_eq!(wrong, StatusCode::UNAUTHORIZED);
    }
    let right = exchange_status(&app, &first(&codes), &OWN, addr(PHONE))
        .await
        .expect("right exchange");
    assert_eq!(right, StatusCode::OK);
}

// ---- The browser's route ----

// Criterion (guard, only the active code): with two codes armed, `POST
// /api/pair` refuses the second while the first is active, then accepts
// both in turn.
#[tokio::test]
async fn test_pair_route_accepts_only_the_active_code() {
    let (app, codes) = armed(2);
    let second = codes.get(1).cloned().unwrap_or_default();

    let early = pair_code(&app, &second, addr(PHONE)).await.expect("pair");
    assert_eq!(
        early.status,
        StatusCode::UNAUTHORIZED,
        "code 2 is not active yet"
    );
    let one = pair_code(&app, &first(&codes), addr(PHONE))
        .await
        .expect("pair");
    assert_eq!(one.status, StatusCode::OK);
    let two = pair_code(&app, &second, addr(PHONE)).await.expect("pair");
    assert_eq!(
        two.status,
        StatusCode::OK,
        "code 2 activated with code 1's use"
    );
}

// Criterion: the browser takes the active code during an app's session —
// that session's `finish` is refused, and the exchange runs on the next.
#[tokio::test]
async fn test_a_session_whose_code_the_browser_took_is_refused() {
    let (app, codes) = armed(2);
    let session = open(&app, &first(&codes), addr(PHONE))
        .await
        .expect("start");
    assert!(session.is_ok(), "start opens a session");
    let Ok(session) = session else { return };

    let browser = pair_code(&app, &first(&codes), addr("192.168.1.50:40000"))
        .await
        .expect("pair");
    assert_eq!(browser.status, StatusCode::OK);
    let answer = finish(
        &app,
        &session.id,
        &confirm_a(&session.key, &OWN),
        addr(PHONE),
    )
    .await
    .expect("finish");
    assert_eq!(answer.status, StatusCode::UNAUTHORIZED);

    let second = codes.get(1).cloned().unwrap_or_default();
    let status = exchange_status(&app, &second, &OWN, addr(PHONE))
        .await
        .expect("exchange on code 2");
    assert_eq!(status, StatusCode::OK);
}

// ---- The log lines ----

/// Everything a `fmt` subscriber writes, for the log-wiring tests.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Captured {
    fn text(&self) -> String {
        self.0
            .lock()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default()
    }
}

impl io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Ok(mut bytes) = self.0.lock() {
            bytes.extend_from_slice(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Captured;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// The lines about a pairing attempt: those naming `peer`, or saying a
/// client paired or an attempt was refused. The banner of the next code is
/// left out — it shows that code by design — and so is nothing else: at
/// least the success and the refusal lines are returned.
fn pairing_lines<'a>(logs: &'a str, peer: &str) -> Vec<&'a str> {
    let lines: Vec<&str> = logs
        .lines()
        .filter(|line| {
            line.contains(peer)
                || line.contains("client paired")
                || line.contains("pairing attempt refused")
        })
        .collect();
    assert!(lines.len() >= 2, "a success and a refusal line: {logs}");
    lines
}

/// A subscriber writing every event up to `level` into `captured`.
fn capturing(captured: &Captured, level: tracing::Level) -> impl tracing::Subscriber {
    tracing_subscriber::fmt()
        .with_writer(captured.clone())
        .with_ansi(false)
        // No timestamps: their digits could spell the port the tests look for.
        .without_time()
        .with_max_level(level)
        .finish()
}

// Criterion: the source address is logged — a refused `finish` logs
// `pairing attempt refused (1/3) from <ip>` and a success `client paired
// from <ip> (code 1/2)`, `<ip>` being the `ConnectInfo` peer without its
// port; neither the armed code nor the token appears in any line.
#[tokio::test]
async fn test_the_exchange_logs_the_peer_address_and_never_the_code() {
    let captured = Captured::default();
    let _guard = tracing::subscriber::set_default(capturing(&captured, tracing::Level::TRACE));
    let (app, codes) = armed(2);

    let wrong = exchange_status(&app, "QQQQQQ", &OWN, addr(PHONE))
        .await
        .expect("wrong-code exchange");
    assert_eq!(wrong, StatusCode::UNAUTHORIZED);
    let right = exchange_status(&app, &first(&codes), &OWN, addr(PHONE))
        .await
        .expect("right exchange");
    assert_eq!(right, StatusCode::OK);

    let logs = captured.text();
    assert!(
        logs.contains("pairing attempt refused (1/3) from 192.168.1.42"),
        "the refusal names the peer: {logs}"
    );
    assert!(
        logs.contains("client paired from 192.168.1.42 (code 1/2)"),
        "the success names the peer and the code's position: {logs}"
    );
    assert!(!logs.contains(TOKEN), "the token is never logged: {logs}");
    for line in pairing_lines(&logs, "192.168.1.42") {
        for secret in codes.iter().map(String::as_str).chain(["51234"]) {
            assert!(
                !line.contains(secret),
                "{secret:?} (a code or the port) must not be in {line:?}"
            );
        }
    }
}

// Criterion: the browser's route logs the same lines, with its peer — and
// never the code it submitted nor the armed one.
#[tokio::test]
async fn test_the_pair_route_logs_the_peer_address_and_never_the_code() {
    let captured = Captured::default();
    let _guard = tracing::subscriber::set_default(capturing(&captured, tracing::Level::TRACE));
    let (app, codes) = armed(1);
    let browser = addr("10.0.0.9:40000");

    let wrong = pair_code(&app, "WRNGCD", browser).await.expect("pair");
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    let right = pair_code(&app, &first(&codes), browser)
        .await
        .expect("pair");
    assert_eq!(right.status, StatusCode::OK);

    let logs = captured.text();
    assert!(
        logs.contains("pairing attempt refused (1/3) from 10.0.0.9"),
        "{logs}"
    );
    assert!(logs.contains("client paired from 10.0.0.9"), "{logs}");
    assert!(!logs.contains(TOKEN), "the token is never logged: {logs}");
    assert!(
        !logs.contains("WRNGCD"),
        "the submitted code is never logged: {logs}"
    );
    for line in pairing_lines(&logs, "10.0.0.9") {
        assert!(
            !line.contains(&first(&codes)),
            "the armed code must not be in {line:?}"
        );
    }
}

// Criterion: a 429 is logged at `debug` only — with the terminal at `info`,
// the refused sixth `start` adds no line naming its address.
#[tokio::test]
async fn test_a_rate_limited_start_logs_nothing_at_info() {
    let captured = Captured::default();
    let _guard = tracing::subscriber::set_default(capturing(&captured, tracing::Level::INFO));
    let (app, _codes) = armed(1);
    let flooder = addr("192.168.1.77:40000");
    for _ in 0..5 {
        let (_, msg) = ClientHalf::start("QQQQQQ");
        let _ = start(&app, &msg, flooder).await.expect("start");
    }
    let before = captured.text().len();

    let (_, msg) = ClientHalf::start("QQQQQQ");
    let limited = start(&app, &msg, flooder).await.expect("sixth start");
    assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);

    let logs = captured.text();
    let after = logs.get(before..).unwrap_or_default();
    assert!(
        !after.contains("192.168.1.77") && !after.contains("429"),
        "a 429 must not reach the terminal at info: {after}"
    );
}
