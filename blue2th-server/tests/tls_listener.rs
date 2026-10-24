// SPDX-License-Identifier: MIT OR Apache-2.0

//! The one listener of #195, over a real loopback socket (`127.0.0.1:0`): a
//! connection whose first byte is `0x16` (RFC 8446 §5.1, `ContentType
//! handshake(22)`) reaches the full router over TLS, anything else the plain
//! router; a silent connection and a stalled handshake are closed once their
//! (short, test-set) timeout passes; the shutdown signal stops the server.
//!
//! The TLS client pins the test's own certificate by its DER, the way the app
//! pins the backend's key: no CA, no host name check. The full router is the
//! store-free one every route test uses, with a known token and no hardware.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use blue2th_proto::HealthStatus;
use blue2th_server::listener::{self, Limits};
use blue2th_server::{auth::AuthStore, spotify_auth::SpotifyAuth, tls};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_rustls::{client::TlsStream, TlsAcceptor, TlsConnector};

/// The API token of the full router (in memory only).
const TOKEN: &str = "test-api-token";

/// The DNS name of the test certificate.
const NAME: &str = "blue2th-test.local";

/// The bound on any single exchange, past which the test fails rather than
/// hangs.
const BOUND: Duration = Duration::from_secs(5);

/// A short timeout, for the step a test exercises.
const SHORT: Duration = Duration::from_millis(300);

/// A long timeout, for the step a test does not exercise: longer than any
/// bound below, so a closure within the bound is the short timer's doing.
const LONG: Duration = Duration::from_secs(30);

/// A served listener and what the tests need to reach and stop it.
struct Server {
    addr: SocketAddr,
    cert_der: Vec<u8>,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<io::Result<()>>,
}

/// The store-free full router, with [`TOKEN`].
fn full_router() -> axum::Router {
    blue2th_server::app_with_auth_store(
        SpotifyAuth::with_config(None, "blue2th://spotify-callback".to_string()),
        AuthStore::with_token(TOKEN),
    )
}

/// Serve the full and the plain router on `127.0.0.1:0` with `limits`, the
/// certificate made here (with `rcgen` directly, so these tests depend on the
/// listener, not on the issuance) and the production `tls::server_config`.
/// `None` when any step of the set-up fails.
async fn start(limits: Limits) -> Option<Server> {
    let key = tls::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).ok()?;
    let params = rcgen::CertificateParams::new(vec![NAME.to_string()]).ok()?;
    let made = params.self_signed(&key).ok()?;
    let cert = tls::IssuedCert {
        der: made.der().to_vec(),
        pem: made.pem(),
    };
    let config = tls::server_config(&cert, &key).ok()?;
    let acceptor = TlsAcceptor::from(Arc::new(config));

    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.ok()?;
    let addr = socket.local_addr().ok()?;
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(listener::serve(
        socket,
        full_router(),
        listener::plain_router(),
        acceptor,
        limits,
        async move {
            let _ = stopped.await;
        },
    ));
    Some(Server {
        addr,
        cert_der: cert.der,
        stop: Some(stop),
        task,
    })
}

/// Accepts exactly the pinned certificate.
#[derive(Debug)]
struct Pinned {
    der: Vec<u8>,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if end_entity.as_ref() == self.der.as_slice() {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::UnknownIssuer,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// A TLS connection to `server` offering `alpn`, `None` when it fails.
async fn tls_connect(server: &Server, alpn: &[&[u8]]) -> Option<TlsStream<TcpStream>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .ok()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pinned {
            der: server.cert_der.clone(),
            provider,
        }))
        .with_no_client_auth();
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    let tcp = timeout(BOUND, TcpStream::connect(server.addr))
        .await
        .ok()?
        .ok()?;
    let name = ServerName::try_from(NAME).ok()?;
    timeout(
        BOUND,
        TlsConnector::from(Arc::new(config)).connect(name, tcp),
    )
    .await
    .ok()?
    .ok()
}

/// A plain TCP connection to `server`.
async fn plain_connect(server: &Server) -> Option<TcpStream> {
    timeout(BOUND, TcpStream::connect(server.addr))
        .await
        .ok()?
        .ok()
}

/// Read until the peer closes (or errs), within [`BOUND`]. `None` when the
/// peer was still open at the bound.
async fn read_until_closed<S: AsyncRead + Unpin>(stream: &mut S) -> Option<Vec<u8>> {
    let mut all = Vec::new();
    let mut chunk = [0_u8; 4096];
    let deadline = Instant::now() + BOUND;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match timeout(left, stream.read(&mut chunk)).await {
            Err(_) => return None,
            Ok(Ok(0) | Err(_)) => return Some(all),
            Ok(Ok(n)) => all.extend_from_slice(&chunk[..n]),
        }
    }
}

/// `GET path` over `stream` in HTTP/1.1, with an optional bearer: the status
/// and the body, `None` when no response came back.
async fn get<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    path: &str,
    bearer: Option<&str>,
) -> Option<(u16, String)> {
    let auth = bearer
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    let request =
        format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}Connection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.ok()?;
    let raw = read_until_closed(&mut stream).await?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text.split(' ').nth(1)?.parse::<u16>().ok()?;
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    Some((status, body))
}

/// `GET /health` over TLS.
async fn tls_health(server: &Server) -> Option<(u16, String)> {
    get(tls_connect(server, &[b"http/1.1"]).await?, "/health", None).await
}

/// `GET /health` in plain HTTP.
async fn plain_health(server: &Server) -> Option<(u16, String)> {
    get(plain_connect(server).await?, "/health", None).await
}

/// The `HealthStatus` of a `/health` answer.
fn health_of(answer: &Option<(u16, String)>) -> Option<HealthStatus> {
    answer
        .as_ref()
        .and_then(|(_, body)| serde_json::from_str(body).ok())
}

/// Connect, send `first`, and measure how long until the server closes;
/// `None` when it was still open at [`BOUND`] or the connection failed.
async fn time_to_close(server: &Server, first: &[u8]) -> Option<Duration> {
    let mut stream = plain_connect(server).await?;
    let started = Instant::now();
    if !first.is_empty() {
        stream.write_all(first).await.ok()?;
    }
    read_until_closed(&mut stream).await?;
    Some(started.elapsed())
}

// Criterion (listener): a TLS client reaches the full router — `/health`
// carries the real release version.
#[tokio::test]
async fn test_serve_tls_client_reaches_the_full_router() {
    let server = start(Limits {
        first_byte: LONG,
        handshake: LONG,
    })
    .await;
    assert!(server.is_some(), "the test server starts");
    let Some(server) = server else { return };
    let answer = tls_health(&server).await;
    assert_eq!(
        answer.as_ref().map(|(s, _)| *s),
        Some(200),
        "got {answer:?}"
    );
    let health = health_of(&answer);
    assert_eq!(
        health.as_ref().map(|h| h.version.as_str()),
        Some(env!("CARGO_PKG_VERSION")),
        "the full /health, got {answer:?}"
    );
    assert_eq!(
        health.map(|h| h.protocol),
        Some(blue2th_proto::PROTOCOL_VERSION)
    );
}

// Criterion (listener): the TLS side negotiates ALPN `h2` when the client
// offers it first, and `http/1.1` when that is all it offers.
#[tokio::test]
async fn test_serve_tls_negotiates_h2_and_http_1_1() {
    let server = start(Limits {
        first_byte: LONG,
        handshake: LONG,
    })
    .await;
    assert!(server.is_some(), "the test server starts");
    let Some(server) = server else { return };
    let h2 = tls_connect(&server, &[b"h2", b"http/1.1"]).await;
    assert!(h2.is_some(), "the TLS handshake completes");
    assert_eq!(
        h2.as_ref().and_then(|s| s.get_ref().1.alpn_protocol()),
        Some(b"h2".as_slice())
    );
    let http1 = tls_connect(&server, &[b"http/1.1"]).await;
    assert_eq!(
        http1.as_ref().and_then(|s| s.get_ref().1.alpn_protocol()),
        Some(b"http/1.1".as_slice())
    );
}

// Criterion (listener): any other first byte (`G`) is served by the plain
// router — the reduced `/health`, version `0.0.0`.
#[tokio::test]
async fn test_serve_plain_client_reaches_the_reduced_health() {
    let server = start(Limits {
        first_byte: LONG,
        handshake: LONG,
    })
    .await;
    assert!(server.is_some(), "the test server starts");
    let Some(server) = server else { return };
    let answer = plain_health(&server).await;
    assert_eq!(
        answer.as_ref().map(|(s, _)| *s),
        Some(200),
        "got {answer:?}"
    );
    assert_eq!(
        health_of(&answer).map(|h| h.version),
        Some("0.0.0".to_string()),
        "the reduced /health, got {answer:?}"
    );
}

// Criterion (plain-HTTP router), guard: a request under `/api` with a **valid**
// token answers 426 in plain HTTP. Near-miss: the very same request over TLS
// answers 200 — a dispatcher that handed plain HTTP to the full router would
// answer 200 to both.
#[tokio::test]
async fn test_serve_plain_api_request_with_a_valid_token_is_refused_426() {
    let server = start(Limits {
        first_byte: LONG,
        handshake: LONG,
    })
    .await;
    assert!(server.is_some(), "the test server starts");
    let Some(server) = server else { return };
    let over_tls = match tls_connect(&server, &[b"http/1.1"]).await {
        Some(stream) => get(stream, "/api/config", Some(TOKEN)).await,
        None => None,
    };
    assert_eq!(
        over_tls.as_ref().map(|(s, _)| *s),
        Some(200),
        "the token is valid: over TLS it is served, got {over_tls:?}"
    );
    let in_clear = match plain_connect(&server).await {
        Some(stream) => get(stream, "/api/config", Some(TOKEN)).await,
        None => None,
    };
    assert_eq!(
        in_clear.as_ref().map(|(s, _)| *s),
        Some(426),
        "in clear it is never served, got {in_clear:?}"
    );
}

// Criterion (listener): a connection that sends nothing is closed once the
// first-byte timeout passes — not before it, and well before the (long)
// handshake timeout.
#[tokio::test]
async fn test_serve_closes_a_silent_connection_after_the_first_byte_timeout() {
    let server = start(Limits {
        first_byte: SHORT,
        handshake: LONG,
    })
    .await;
    assert!(server.is_some(), "the test server starts");
    let Some(server) = server else { return };
    let closed = time_to_close(&server, &[]).await;
    assert!(closed.is_some(), "a silent connection must be closed");
    let closed = closed.unwrap_or_default();
    assert!(
        closed >= SHORT - Duration::from_millis(50),
        "closed after {closed:?}, before the {SHORT:?} timeout"
    );
    assert!(closed < Duration::from_secs(3), "closed after {closed:?}");
}

// Criterion (listener): a TLS handshake that does not complete — a client
// that sends `0x16` and stalls — is closed once the handshake timeout passes.
// The first byte came, so the (long) first-byte timeout is not what closes it.
#[tokio::test]
async fn test_serve_closes_a_stalled_tls_handshake_after_the_handshake_timeout() {
    let server = start(Limits {
        first_byte: LONG,
        handshake: SHORT,
    })
    .await;
    assert!(server.is_some(), "the test server starts");
    let Some(server) = server else { return };
    let closed = time_to_close(&server, &[0x16]).await;
    assert!(closed.is_some(), "a stalled handshake must be closed");
    let closed = closed.unwrap_or_default();
    assert!(
        closed >= SHORT - Duration::from_millis(50),
        "closed after {closed:?}, before the {SHORT:?} timeout"
    );
    assert!(closed < Duration::from_secs(3), "closed after {closed:?}");
}

// Criterion (constraints): a failed TLS handshake closes that connection and
// never stops the accept loop — plain and TLS clients are served after it.
#[tokio::test]
async fn test_serve_a_failed_tls_handshake_does_not_stop_the_accept_loop() {
    let server = start(Limits {
        first_byte: LONG,
        handshake: Duration::from_secs(2),
    })
    .await;
    assert!(server.is_some(), "the test server starts");
    let Some(server) = server else { return };
    // A handshake record header, then bytes no ClientHello carries.
    let garbage = [0x16, 0x03, 0x01, 0x00, 0x04, 0xff, 0xff, 0xff, 0xff];
    let closed = time_to_close(&server, &garbage).await;
    assert!(closed.is_some(), "the failed handshake is closed");

    let plain = plain_health(&server).await;
    assert_eq!(plain.as_ref().map(|(s, _)| *s), Some(200), "got {plain:?}");
    let over_tls = tls_health(&server).await;
    assert_eq!(
        over_tls.as_ref().map(|(s, _)| *s),
        Some(200),
        "got {over_tls:?}"
    );
}

// Criterion (listener): the shutdown signal stops the server. Before it, the
// server answers; after it, `serve` returns `Ok` and the socket is closed.
#[tokio::test]
async fn test_serve_stops_on_the_shutdown_signal() {
    let server = start(Limits {
        first_byte: LONG,
        handshake: LONG,
    })
    .await;
    assert!(server.is_some(), "the test server starts");
    let Some(mut server) = server else { return };
    let before = plain_health(&server).await;
    assert_eq!(
        before.as_ref().map(|(s, _)| *s),
        Some(200),
        "the server must be serving before the signal, got {before:?}"
    );

    if let Some(stop) = server.stop.take() {
        let _ = stop.send(());
    }
    let returned = timeout(BOUND, &mut server.task).await;
    assert!(
        matches!(returned, Ok(Ok(Ok(())))),
        "serve returns Ok once the signal resolves, got {returned:?}"
    );
    assert!(
        TcpStream::connect(server.addr).await.is_err(),
        "the listening socket is closed once serve returned"
    );
}
