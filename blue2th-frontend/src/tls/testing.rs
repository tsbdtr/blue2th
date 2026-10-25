// SPDX-License-Identifier: MIT OR Apache-2.0

//! Test support for #196: a loopback TLS backend, and the keys and certificates
//! it presents, made per test with `rcgen`. No real backend and no hardware.
//!
//! Every helper returns an `Option` or a `Result` rather than panicking: the
//! `expect` belongs to the test that calls it.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::{CertifiedKey, SigningKey};
use sha2::Digest;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// How long a test waits, once the call under test has returned, for the
/// loopback backend to settle what it saw. The handshake has happened or failed
/// by then, so a longer bound buys nothing.
pub(crate) const SERVED_BOUND: Duration = Duration::from_secs(1);

/// A healthy backend's `/health`, speaking this app's contract.
pub(crate) const HEALTH_OK: &str =
    r#"{"status":"ok","version":"0.2.0","auth_required":true,"protocol":3,"protocol_min":3}"#;

/// An ECDSA P-256 key, like the backend's (#195).
pub(crate) struct TestKey(rcgen::KeyPair);

impl TestKey {
    /// A fresh key.
    pub(crate) fn generate() -> Option<Self> {
        rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
            .ok()
            .map(Self)
    }

    /// The DER of the key's whole `SubjectPublicKeyInfo`.
    pub(crate) fn spki_der(&self) -> Vec<u8> {
        rcgen::PublicKeyData::subject_public_key_info(&self.0)
    }

    /// The pin of this key as the app stores it — 64 lower-case hex characters
    /// of the SHA-256 of the SPKI — computed here with `sha2`, apart from the
    /// code under test.
    pub(crate) fn pin_hex(&self) -> String {
        hex(&sha2::Sha256::digest(self.spki_der()))
    }

    /// The `rustls` signer of this key.
    fn signer(&self) -> Option<Arc<dyn SigningKey>> {
        let der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.0.serialize_der()));
        rustls::crypto::ring::sign::any_supported_type(&der).ok()
    }
}

/// Lower-case hex of `bytes`.
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `pin` with its first hex digit changed: the SPKI hash one nibble off, a pin
/// that is well-formed and matches no key.
pub(crate) fn one_digit_off(pin: &str) -> String {
    let mut chars: Vec<char> = pin.chars().collect();
    if let Some(first) = chars.first_mut() {
        *first = if *first == '0' { '1' } else { '0' };
    }
    chars.into_iter().collect()
}

/// When a certificate is valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Validity {
    /// `rcgen`'s default window, which holds today.
    Current,
    /// From 2000-01-01 to 2001-01-01: long expired.
    Expired,
}

/// A self-signed certificate on `key` for `names`, valid as `validity` says.
pub(crate) fn certificate(
    key: &TestKey,
    names: &[&str],
    validity: Validity,
) -> Option<CertificateDer<'static>> {
    let names: Vec<String> = names.iter().map(|n| (*n).to_string()).collect();
    let mut params = rcgen::CertificateParams::new(names).ok()?;
    if validity == Validity::Expired {
        params.not_before = rcgen::date_time_ymd(2000, 1, 1);
        params.not_after = rcgen::date_time_ymd(2001, 1, 1);
    }
    let made = params.self_signed(&key.0).ok()?;
    Some(made.der().clone())
}

/// What the backend presents: `certificate`, with handshakes signed by
/// `signer`. The two are normally the same key; a test that wants a handshake
/// signed by another key than the certificate's passes another one.
pub(crate) fn presenting(
    certificate: CertificateDer<'static>,
    signer: &TestKey,
) -> Option<Arc<CertifiedKey>> {
    // `CertifiedKey::new`, not `from_der`: the latter refuses a signer that
    // does not match the certificate, which is the very case one test needs.
    Some(Arc::new(CertifiedKey::new(
        vec![certificate],
        signer.signer()?,
    )))
}

/// The names every test URL is reached at: the loopback IP, and `localhost`.
pub(crate) const LOOPBACK_NAMES: [&str; 2] = ["127.0.0.1", "localhost"];

/// A certificate valid in every way a web PKI would check — for the loopback
/// address the tests connect to, and currently valid — on `key`, its
/// handshakes signed by `key`.
pub(crate) fn ordinary(key: &TestKey) -> Option<Arc<CertifiedKey>> {
    presenting(certificate(key, &LOOPBACK_NAMES, Validity::Current)?, key)
}

/// Which TLS versions the backend offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Versions {
    /// TLS 1.3 only.
    Tls13,
    /// TLS 1.2 only.
    Tls12,
}

/// One connection the loopback backend accepts, in order: what it presents,
/// which versions it offers, and what it answers to the one request it reads.
pub(crate) struct Conn {
    pub(crate) presents: Arc<CertifiedKey>,
    pub(crate) versions: Versions,
    pub(crate) status_line: &'static str,
    pub(crate) payload: &'static str,
}

impl Conn {
    /// A TLS 1.3 connection presenting `presents` and answering `200 OK` with
    /// `payload`.
    pub(crate) fn ok(presents: Arc<CertifiedKey>, payload: &'static str) -> Self {
        Self {
            presents,
            versions: Versions::Tls13,
            status_line: "200 OK",
            payload,
        }
    }
}

/// Always presents the one key it holds, matching or not.
struct Always(Arc<CertifiedKey>);

impl std::fmt::Debug for Always {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Always")
    }
}

impl ResolvesServerCert for Always {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        // Clone of the `Arc`: each handshake takes its own handle.
        Some(Arc::clone(&self.0))
    }
}

/// The server configuration of one planned connection.
fn server_config(conn: &Conn) -> Option<rustls::ServerConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let versions: &[&'static rustls::SupportedProtocolVersion] = match conn.versions {
        Versions::Tls13 => &[&rustls::version::TLS13],
        Versions::Tls12 => &[&rustls::version::TLS12],
    };
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(versions)
        .ok()?
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(Always(Arc::clone(&conn.presents))));
    Some(config)
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
pub(crate) fn header_value(raw: &str, name: &str) -> Option<String> {
    raw.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name)
            .then(|| value.trim().to_string())
    })
}

/// The method and path of a raw request's first line.
pub(crate) fn method_and_path(raw: &str) -> (String, String) {
    let mut words = raw.lines().next().unwrap_or_default().split(' ');
    (
        words.next().unwrap_or_default().to_string(),
        words.next().unwrap_or_default().to_string(),
    )
}

/// What one planned connection came to: the raw request the backend read, or
/// why it read none (the handshake failed, the client went away).
pub(crate) type Seen = Result<String, String>;

/// A loopback TLS backend serving its planned connections in order.
pub(crate) struct TlsBackend {
    /// `https://127.0.0.1:<port>`.
    pub(crate) url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
    task: tokio::task::JoinHandle<()>,
}

impl TlsBackend {
    /// Start serving `plan` on `127.0.0.1:0`. `None` when any step of the
    /// set-up fails.
    pub(crate) async fn start(plan: Vec<Conn>) -> Option<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.ok()?;
        let addr = listener.local_addr().ok()?;
        let configs: Vec<(Arc<rustls::ServerConfig>, &'static str, &'static str)> = plan
            .iter()
            .map(|conn| server_config(conn).map(|c| (Arc::new(c), conn.status_line, conn.payload)))
            .collect::<Option<_>>()?;
        let seen: Arc<Mutex<Vec<Seen>>> = Arc::new(Mutex::new(Vec::new()));
        // Clone of the `Arc`: the task records, the test reads.
        let record = Arc::clone(&seen);
        let task = tokio::spawn(async move {
            for (config, status_line, payload) in configs {
                let Ok((tcp, _)) = listener.accept().await else {
                    return;
                };
                let outcome = serve_one(tcp, config, status_line, payload).await;
                if let Ok(mut seen) = record.lock() {
                    seen.push(outcome);
                }
            }
        });
        Some(Self {
            url: format!("https://{addr}"),
            seen,
            task,
        })
    }

    /// What each connection came to, once every planned one was served or
    /// [`SERVED_BOUND`] elapsed — whichever comes first. The listener is
    /// aborted on the bound, so nothing is left accepting.
    pub(crate) async fn within_bound(mut self) -> Vec<Seen> {
        if tokio::time::timeout(SERVED_BOUND, &mut self.task)
            .await
            .is_err()
        {
            self.task.abort();
        }
        self.seen
            .lock()
            .map(|seen| seen.clone())
            .unwrap_or_default()
    }

    /// The requests the backend actually read, handshake failures left out.
    pub(crate) async fn requests_within_bound(self) -> Vec<String> {
        self.within_bound()
            .await
            .into_iter()
            .filter_map(Result::ok)
            .collect()
    }
}

/// Run one TLS connection: handshake, read one request, answer it, close.
async fn serve_one(
    tcp: tokio::net::TcpStream,
    config: Arc<rustls::ServerConfig>,
    status_line: &'static str,
    payload: &'static str,
) -> Seen {
    let acceptor = tokio_rustls::TlsAcceptor::from(config);
    let mut stream = acceptor
        .accept(tcp)
        .await
        .map_err(|e| format!("handshake: {e}"))?;
    let mut raw = Vec::new();
    let mut chunk = [0u8; 1024];
    let request = loop {
        if let Some(request) = request_complete(&raw) {
            break request;
        }
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|e| format!("read the request: {e}"))?;
        if read == 0 {
            return Err("the client closed before sending a request".to_string());
        }
        raw.extend_from_slice(&chunk[..read]);
    };
    let response = format!(
        "HTTP/1.1 {status_line}\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{payload}",
        payload.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .map_err(|e| format!("write the response: {e}"))?;
    let _ = stream.flush().await;
    let _ = stream.shutdown().await;
    Ok(request)
}
