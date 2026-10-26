// SPDX-License-Identifier: MIT OR Apache-2.0

//! The pin and the verifier (#196), over real TLS handshakes against a
//! loopback backend whose certificates are made per test (`super::testing`).

use std::time::Duration;

use super::testing::{
    certificate, hex, one_digit_off, ordinary, presenting, Conn, TestKey, TlsBackend, Validity,
    Versions, HEALTH_OK, LOOPBACK_NAMES,
};
use super::*;

/// How long one request of these tests may take. A loopback handshake takes
/// milliseconds; the bound only turns a hang into a failure.
const CALL_BOUND: Duration = Duration::from_secs(5);

/// A well-formed pin: 64 lower-case hex characters.
const SOME_PIN: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

/// `GET {url}/health` through `client`.
async fn get_health(client: &TlsClient, url: &str) -> Result<reqwest::Response, reqwest::Error> {
    client
        .http()
        .get(format!("{url}/health"))
        .timeout(CALL_BOUND)
        .send()
        .await
}

/// The `rustls::Error` somewhere in `err`'s source chain, looked for by type:
/// either a link of the chain itself, or the payload of an `io::Error` link
/// (whose own `source()` skips its payload).
fn rustls_error_in<'a>(err: &'a (dyn std::error::Error + 'static)) -> Option<&'a rustls::Error> {
    let mut current: Option<&'a (dyn std::error::Error + 'static)> = Some(err);
    while let Some(link) = current {
        if let Some(found) = link.downcast_ref::<rustls::Error>() {
            return Some(found);
        }
        let wrapped = link
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::get_ref)
            .and_then(|inner| inner.downcast_ref::<rustls::Error>());
        if wrapped.is_some() {
            return wrapped;
        }
        current = link.source();
    }
    None
}

// ---- Pin ----

// Criterion: the pin is stored as 64 lower-case hex characters, and reads back
// to the same text.
#[test]
fn test_pin_parse_round_trips_64_lower_case_hex() {
    assert_eq!(
        Pin::parse(SOME_PIN).map(|pin| pin.to_hex()),
        Some(SOME_PIN.to_string())
    );
}

// Criterion (guard, "a pin that is empty, not 64 characters, or not hex is no
// pin"): each value below fails exactly one of the rules. Near-misses: 63 and
// 65 hex digits pass a hex check and only the length refuses them; a `g` in 64
// characters passes the length and only the hex check refuses it; 32 `é` are
// 64 *bytes*, which a byte-length check accepts; `0x` + 62 digits passes the
// length. The empty value is the wildcard (CLAUDE.md). Upper case is refused
// rather than normalised: the stored form is lower case, and anything else in
// a blob was not written by the app.
#[test]
fn test_pin_parse_refuses_what_names_no_pin() {
    let sixty_three = &SOME_PIN[..63];
    let sixty_five = format!("{SOME_PIN}0");
    let with_a_g = format!("g{}", &SOME_PIN[1..]);
    let accented = "é".repeat(32);
    let prefixed = format!("0x{}", &SOME_PIN[2..]);
    let upper = SOME_PIN.to_uppercase();
    let padded = format!(" {} ", &SOME_PIN[..62]);
    let refused: Vec<&str> = vec![
        "",
        sixty_three,
        &sixty_five,
        &with_a_g,
        &accented,
        &prefixed,
        &upper,
        &padded,
    ];
    // The near-misses are what they claim to be: the right length, or hex.
    assert_eq!(with_a_g.len(), 64);
    assert_eq!(accented.len(), 64);
    assert_eq!(prefixed.len(), 64);
    assert_eq!(upper.len(), 64);

    let accepted: Vec<&str> = refused
        .iter()
        .copied()
        .filter(|stored| Pin::parse(stored).is_some())
        .collect();
    assert_eq!(accepted, Vec::<&str>::new(), "none of these names a pin");
    // …while the value they are built from does: the refusals above are not
    // a parser that refuses everything.
    assert!(Pin::parse(SOME_PIN).is_some(), "the well-formed pin parses");
}

// Criterion: the pin is the SHA-256 of the bytes it is given — the whole DER
// of the SPKI. Known vector: SHA-256("abc") (FIPS 180-2, appendix B.1).
#[test]
fn test_pin_of_spki_is_the_sha256_of_the_der() {
    assert_eq!(
        Pin::of_spki(b"abc").to_hex(),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

// Criterion: the pin of a real key is the SHA-256 of its whole SPKI DER,
// computed here with `sha2`, apart from the code under test. Near-miss: a
// hash of the key bits alone (the BIT STRING inside the SPKI), which the
// vector above cannot tell from the right one.
#[test]
fn test_pin_of_spki_of_a_backend_key_is_the_hash_of_its_spki() {
    let key = TestKey::generate().expect("generate a key");
    assert_eq!(Pin::of_spki(&key.spki_der()).to_hex(), key.pin_hex());
}

// ---- The verifier, through `client_for` ----

// Criterion: the verifier accepts the end-entity certificate when the SHA-256
// of its SPKI equals the pin — over TLS 1.3.
#[tokio::test]
async fn test_a_pinned_client_accepts_the_pinned_key_over_tls13() {
    let key = TestKey::generate().expect("generate a key");
    let backend = TlsBackend::start(vec![Conn::ok(
        ordinary(&key).expect("present the key"),
        HEALTH_OK,
    )])
    .await
    .expect("start the TLS backend");
    let pin = Pin::parse(&key.pin_hex());
    assert!(pin.is_some(), "the key's pin parses");

    let client = client_for(&backend.url, pin).expect("build the client");
    let answer = get_health(&client, &backend.url).await;
    let requests = backend.requests_within_bound().await;

    assert_eq!(
        answer
            .map(|r| r.status().as_u16())
            .map_err(|e| e.to_string()),
        Ok(200)
    );
    assert_eq!(
        requests.len(),
        1,
        "the request went over the pinned handshake"
    );
}

// Criterion: the same over TLS 1.2 — both handshake versions are verified,
// neither is refused outright.
#[tokio::test]
async fn test_a_pinned_client_accepts_the_pinned_key_over_tls12() {
    let key = TestKey::generate().expect("generate a key");
    let backend = TlsBackend::start(vec![Conn {
        versions: Versions::Tls12,
        ..Conn::ok(ordinary(&key).expect("present the key"), HEALTH_OK)
    }])
    .await
    .expect("start the TLS backend");

    let client = client_for(&backend.url, Pin::parse(&key.pin_hex())).expect("build the client");
    let answer = get_health(&client, &backend.url).await;
    let requests = backend.requests_within_bound().await;

    assert_eq!(
        answer
            .map(|r| r.status().as_u16())
            .map_err(|e| e.to_string()),
        Ok(200)
    );
    assert_eq!(requests.len(), 1);
}

// Criterion (guard, "if and only if"): a key whose SPKI hash differs from the
// pin in one byte is refused, as a `PinMismatch`. Near-miss: everything else
// about the certificate is valid — names, dates, the handshake signature — so
// only the pin comparison can refuse it.
#[tokio::test]
async fn test_a_pinned_client_refuses_a_key_one_byte_off_the_pin() {
    let key = TestKey::generate().expect("generate a key");
    let backend = TlsBackend::start(vec![Conn::ok(
        ordinary(&key).expect("present the key"),
        HEALTH_OK,
    )])
    .await
    .expect("start the TLS backend");
    let off = Pin::parse(&one_digit_off(&key.pin_hex()));
    assert!(off.is_some(), "the altered pin is still a well-formed pin");

    let client = client_for(&backend.url, off).expect("build the client");
    let answer = get_health(&client, &backend.url).await;
    let requests = backend.requests_within_bound().await;

    assert!(answer.is_err(), "another key must be refused: {answer:?}");
    assert_eq!(
        answer.as_ref().err().map(is_pin_mismatch),
        Some(true),
        "the refusal is a pin mismatch, found by type: {answer:?}"
    );
    assert_eq!(
        requests,
        Vec::<String>::new(),
        "no request may follow a refused handshake"
    );
}

// Criterion (guard, the verifier ignores the server name): the pinned key on a
// certificate issued for another name than the address reached. Near-miss: a
// verifier that delegates to webpki's name check refuses it.
#[tokio::test]
async fn test_a_pinned_client_accepts_the_pinned_key_on_a_certificate_for_another_name() {
    let key = TestKey::generate().expect("generate a key");
    let cert =
        certificate(&key, &["other.example"], Validity::Current).expect("make the certificate");
    let backend = TlsBackend::start(vec![Conn::ok(
        presenting(cert, &key).expect("present the key"),
        HEALTH_OK,
    )])
    .await
    .expect("start the TLS backend");

    let client = client_for(&backend.url, Pin::parse(&key.pin_hex())).expect("build the client");
    let answer = get_health(&client, &backend.url).await;
    let requests = backend.requests_within_bound().await;

    assert_eq!(
        answer
            .map(|r| r.status().as_u16())
            .map_err(|e| e.to_string()),
        Ok(200),
        "the name is not checked: the IP changes with every DHCP lease"
    );
    assert_eq!(requests.len(), 1);
}

// Criterion (guard, the verifier ignores the validity dates): the pinned key on
// a certificate that expired in 2001. Near-miss: a verifier that checks the
// dates refuses it; an app with a wrong clock would refuse a valid one.
#[tokio::test]
async fn test_a_pinned_client_accepts_the_pinned_key_on_an_expired_certificate() {
    let key = TestKey::generate().expect("generate a key");
    let cert = certificate(&key, &LOOPBACK_NAMES, Validity::Expired).expect("make the certificate");
    let backend = TlsBackend::start(vec![Conn::ok(
        presenting(cert, &key).expect("present the key"),
        HEALTH_OK,
    )])
    .await
    .expect("start the TLS backend");

    let client = client_for(&backend.url, Pin::parse(&key.pin_hex())).expect("build the client");
    let answer = get_health(&client, &backend.url).await;
    let requests = backend.requests_within_bound().await;

    assert_eq!(
        answer
            .map(|r| r.status().as_u16())
            .map_err(|e| e.to_string()),
        Ok(200),
        "the dates are not checked"
    );
    assert_eq!(requests.len(), 1);
}

/// The backend presenting `key`'s certificate while signing its handshakes
/// with `impostor`, offering `versions` only.
async fn impostor_backend(key: &TestKey, impostor: &TestKey, versions: Versions) -> TlsBackend {
    let cert = certificate(key, &LOOPBACK_NAMES, Validity::Current).expect("make the certificate");
    TlsBackend::start(vec![Conn {
        versions,
        ..Conn::ok(
            presenting(cert, impostor).expect("present the certificate"),
            HEALTH_OK,
        )
    }])
    .await
    .expect("start the TLS backend")
}

// Criterion (guard, the TLS 1.3 handshake signature is still verified): the
// pinned certificate, its handshake signed by another key. Near-miss: a
// verifier that only compares the hash accepts it — the certificate is the
// pinned one. And the refusal is a transport error, not an identity change.
#[tokio::test]
async fn test_a_pinned_client_refuses_a_tls13_handshake_signed_by_another_key() {
    let key = TestKey::generate().expect("generate a key");
    let impostor = TestKey::generate().expect("generate another key");
    let backend = impostor_backend(&key, &impostor, Versions::Tls13).await;

    let client = client_for(&backend.url, Pin::parse(&key.pin_hex())).expect("build the client");
    let answer = get_health(&client, &backend.url).await;
    let seen = backend.within_bound().await;

    assert!(
        answer.is_err(),
        "a bad signature must be refused: {answer:?}"
    );
    assert_eq!(
        seen.iter().map(Result::is_ok).collect::<Vec<_>>(),
        vec![false],
        "one handshake, attempted and refused — not a client that never connected: {seen:?}"
    );
    assert_eq!(
        answer.as_ref().err().map(is_pin_mismatch),
        Some(false),
        "a bad signature is not an identity change"
    );
}

// Criterion (guard): the same over TLS 1.2, whose signature goes through
// another method of the verifier. Near-miss: a verifier that checks 1.3 and
// passes 1.2 through, which the test above accepts.
#[tokio::test]
async fn test_a_pinned_client_refuses_a_tls12_handshake_signed_by_another_key() {
    let key = TestKey::generate().expect("generate a key");
    let impostor = TestKey::generate().expect("generate another key");
    let backend = impostor_backend(&key, &impostor, Versions::Tls12).await;

    let client = client_for(&backend.url, Pin::parse(&key.pin_hex())).expect("build the client");
    let answer = get_health(&client, &backend.url).await;
    let seen = backend.within_bound().await;

    assert!(
        answer.is_err(),
        "a bad signature must be refused: {answer:?}"
    );
    assert_eq!(
        seen.iter().map(Result::is_ok).collect::<Vec<_>>(),
        vec![false],
        "one handshake, attempted and refused — not a client that never connected: {seen:?}"
    );
    assert_eq!(
        answer.as_ref().err().map(is_pin_mismatch),
        Some(false),
        "a bad signature is not an identity change"
    );
}

// Criterion (guard): an unpinned client — the first handshake of a pairing —
// still verifies the signature. Near-miss: an "accept anything" verifier for
// the unpinned case, which would record a key the peer does not hold.
#[tokio::test]
async fn test_an_unpinned_client_refuses_a_handshake_signed_by_another_key() {
    let key = TestKey::generate().expect("generate a key");
    let impostor = TestKey::generate().expect("generate another key");
    let backend = impostor_backend(&key, &impostor, Versions::Tls13).await;

    let client = client_for(&backend.url, None).expect("build the client");
    let answer = get_health(&client, &backend.url).await;
    let seen = backend.within_bound().await;

    assert!(
        answer.is_err(),
        "a bad signature must be refused: {answer:?}"
    );
    assert_eq!(
        seen.iter().map(Result::is_ok).collect::<Vec<_>>(),
        vec![false],
        "one handshake, attempted and refused: {seen:?}"
    );
    assert_eq!(
        client.presented_pin(),
        None,
        "a refused key is not recorded"
    );
}

// Criterion: with no pin the client accepts any key — on a certificate for
// another name, expired — and records the pin of the key presented, which is
// what trust on first use stores.
#[tokio::test]
async fn test_an_unpinned_client_accepts_any_key_and_records_its_pin() {
    let key = TestKey::generate().expect("generate a key");
    let cert =
        certificate(&key, &["other.example"], Validity::Expired).expect("make the certificate");
    let backend = TlsBackend::start(vec![Conn::ok(
        presenting(cert, &key).expect("present the key"),
        HEALTH_OK,
    )])
    .await
    .expect("start the TLS backend");

    let client = client_for(&backend.url, None).expect("build the client");
    let answer = get_health(&client, &backend.url).await;
    let _ = backend.requests_within_bound().await;

    assert_eq!(
        answer
            .map(|r| r.status().as_u16())
            .map_err(|e| e.to_string()),
        Ok(200)
    );
    // Compared with the pin computed by `sha2`, never with another value the
    // code under test produced: two absent values compare equal.
    assert_eq!(
        client.presented_pin().map(|pin| pin.to_hex()),
        Some(key.pin_hex())
    );
}

// Criterion: a refused key is a `PinMismatch` carried inside
// `rustls::Error::InvalidCertificate(CertificateError::Other(..))`, reachable
// by type through the `reqwest` error's source chain.
#[tokio::test]
async fn test_a_refused_key_is_a_pin_mismatch_inside_a_rustls_invalid_certificate() {
    let key = TestKey::generate().expect("generate a key");
    let other = TestKey::generate().expect("generate another key");
    let backend = TlsBackend::start(vec![Conn::ok(
        ordinary(&key).expect("present the key"),
        HEALTH_OK,
    )])
    .await
    .expect("start the TLS backend");

    let client = client_for(&backend.url, Pin::parse(&other.pin_hex())).expect("build the client");
    let answer = get_health(&client, &backend.url).await;
    let _ = backend.requests_within_bound().await;

    let error = answer.err();
    assert!(error.is_some(), "another machine's key must be refused");
    let carried = error.as_ref().and_then(|e| rustls_error_in(e));
    assert!(
        matches!(
            carried,
            Some(rustls::Error::InvalidCertificate(rustls::CertificateError::Other(other)))
                if other.0.downcast_ref::<PinMismatch>().is_some()
        ),
        "expected InvalidCertificate(Other(PinMismatch)), got {carried:?} in {error:?}"
    );
}

// Criterion (guard, an identity change is recognised by type, not as "any
// failure"): a refused connection is not a pin mismatch. Near-miss: an
// `is_pin_mismatch` that answers `true` for every transport error.
#[tokio::test]
async fn test_is_pin_mismatch_is_false_for_a_refused_connection() {
    // Port 1 is privileged and unbound: the connection is refused at once.
    let client = client_for("https://127.0.0.1:1", Pin::parse(SOME_PIN)).expect("build the client");
    let answer = get_health(&client, "https://127.0.0.1:1").await;

    assert_eq!(answer.as_ref().err().map(is_pin_mismatch), Some(false));
}

// Criterion (guard, no global verifier state): two clients built for two
// backends keep their own pins. Near-miss: a verifier reading a pin shared by
// every client — "the last one built wins" — refuses the first backend here.
#[tokio::test]
async fn test_two_clients_keep_their_own_pins() {
    let first_key = TestKey::generate().expect("generate a key");
    let second_key = TestKey::generate().expect("generate another key");
    let first = TlsBackend::start(vec![Conn::ok(
        ordinary(&first_key).expect("present the key"),
        HEALTH_OK,
    )])
    .await
    .expect("start the first backend");
    let second = TlsBackend::start(vec![
        Conn::ok(ordinary(&second_key).expect("present the key"), HEALTH_OK),
        Conn::ok(ordinary(&second_key).expect("present the key"), HEALTH_OK),
    ])
    .await
    .expect("start the second backend");

    let first_client =
        client_for(&first.url, Pin::parse(&first_key.pin_hex())).expect("build the first client");
    let second_client = client_for(&second.url, Pin::parse(&second_key.pin_hex()))
        .expect("build the second client");

    let at_first = get_health(&first_client, &first.url).await;
    let at_second = get_health(&second_client, &second.url).await;
    let crossed = get_health(&first_client, &second.url).await;
    let _ = first.requests_within_bound().await;
    let _ = second.requests_within_bound().await;

    assert_eq!(
        (
            at_first
                .map(|r| r.status().as_u16())
                .map_err(|e| e.to_string()),
            at_second
                .map(|r| r.status().as_u16())
                .map_err(|e| e.to_string()),
        ),
        (Ok(200), Ok(200)),
        "each client accepts its own backend"
    );
    assert_eq!(
        crossed.as_ref().err().map(is_pin_mismatch),
        Some(true),
        "the first client refuses the second backend's key"
    );
}

// The fixture's own claim: `hex` is lower-case, two digits per byte — the
// stored form every comparison above is made against.
#[test]
fn test_the_fixture_hex_is_two_lower_case_digits_per_byte() {
    assert_eq!(hex(&[0x00, 0x0f, 0xab, 0xff]), "000fabff");
}
