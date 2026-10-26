// SPDX-License-Identifier: MIT OR Apache-2.0

//! The app's side of the backend's TLS (#196): the backend is trusted by the
//! SHA-256 of its certificate's public key (its SPKI), pinned per backend next
//! to its token, never by a certificate authority.
//!
//! Native targets only: the browser build reaches the backend through `fetch`,
//! and the browser does the TLS.

use std::sync::{Arc, Mutex};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, DigitallySignedStruct, SignatureScheme};

/// The SHA-256 of a backend's `SubjectPublicKeyInfo`, the whole DER of it.
///
/// Stored in a [`crate::settings::BackendEntry`] as 64 lower-case hex
/// characters, read back through [`Pin::parse`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Pin([u8; 32]);

impl Pin {
    /// The pin a stored value names, or `None` when it names none: empty, not
    /// 64 characters, or not lower-case hex.
    pub fn parse(stored: &str) -> Option<Pin> {
        if !crate::settings::is_stored_pin(stored) {
            return None;
        }
        let mut bytes = [0u8; 32];
        for (byte, pair) in bytes.iter_mut().zip(stored.as_bytes().chunks(2)) {
            let text = std::str::from_utf8(pair).ok()?;
            *byte = u8::from_str_radix(text, 16).ok()?;
        }
        Some(Pin(bytes))
    }

    /// The pin of a public key, from the DER of its whole `SubjectPublicKeyInfo`.
    pub fn of_spki(spki_der: &[u8]) -> Pin {
        let digest = ring::digest::digest(&ring::digest::SHA256, spki_der);
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(digest.as_ref());
        Pin(bytes)
    }

    /// The stored form: 64 lower-case hex characters.
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The pin of the key a certificate carries.
    fn of_certificate(certificate: &CertificateDer<'_>) -> Result<Pin, rustls::Error> {
        let parsed = webpki::EndEntityCert::try_from(certificate)
            .map_err(|_| rustls::Error::InvalidCertificate(CertificateError::BadEncoding))?;
        Ok(Pin::of_spki(parsed.subject_public_key_info().as_ref()))
    }
}

/// A backend presented a key other than the one pinned for it: its identity
/// changed. Carried inside `rustls::Error::InvalidCertificate(CertificateError::Other(..))`
/// and recognised by type, never by its message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinMismatch;

impl std::fmt::Display for PinMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the backend presented a key other than the pinned one")
    }
}

impl std::error::Error for PinMismatch {}

/// Why a client could not be built.
#[derive(Debug)]
pub struct ClientSetupError(String);

impl std::fmt::Display for ClientSetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ClientSetupError {}

/// An HTTP client for one backend, and the key its TLS handshakes presented.
#[derive(Debug, Clone)]
pub struct TlsClient {
    http: reqwest::Client,
    presented: Arc<Mutex<Option<Pin>>>,
}

impl TlsClient {
    /// The client every request to that backend goes through.
    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// The pin of the key the last verified handshake presented, `None` before
    /// any handshake (and for a plain `http://` client).
    pub fn presented_pin(&self) -> Option<Pin> {
        self.presented.lock().ok().and_then(|seen| *seen)
    }
}

/// Trusts a backend by its key alone.
///
/// The server name, the IP, the validity dates and any intermediate are
/// ignored: the backend's certificate is self-signed, its IP changes with every
/// DHCP lease, and a phone with a wrong clock must not refuse a known key. The
/// handshake signature is still verified, so a peer that merely replays the
/// pinned certificate without holding its private key is refused.
#[derive(Debug)]
struct PinVerifier {
    /// The only key accepted; any key when `None` (the first pairing handshake).
    pin: Option<Pin>,
    /// Where a verified handshake's key is recorded, for trust on first use.
    presented: Arc<Mutex<Option<Pin>>>,
    /// The provider's signature algorithms.
    algorithms: WebPkiSupportedAlgorithms,
}

impl PinVerifier {
    /// Record the key of a handshake whose signature was verified. Recorded
    /// only then: a key whose holder did not sign the handshake proves nothing.
    fn record(&self, certificate: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        let pin = Pin::of_certificate(certificate)?;
        if let Ok(mut seen) = self.presented.lock() {
            *seen = Some(pin);
        }
        Ok(())
    }
}

impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let presented = Pin::of_certificate(end_entity)?;
        match self.pin {
            Some(pinned) if pinned != presented => Err(rustls::Error::InvalidCertificate(
                CertificateError::Other(rustls::OtherError(Arc::new(PinMismatch))),
            )),
            _ => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        let valid = rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)?;
        self.record(cert)?;
        Ok(valid)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        let valid = rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)?;
        self.record(cert)?;
        Ok(valid)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// The one constructor of every client the app talks to a backend with: for
/// `url`, accepting only `pin` when there is one, any key otherwise.
///
/// One client per call, its pin held by its own verifier: no verifier state is
/// shared between two backends. A plain `http://` URL never reaches the
/// verifier, so its client records no key.
pub fn client_for(_url: &str, pin: Option<Pin>) -> Result<TlsClient, ClientSetupError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let presented = Arc::new(Mutex::new(None));
    let verifier = Arc::new(PinVerifier {
        pin,
        // Clone of the `Arc`: the verifier records, the client reads.
        presented: Arc::clone(&presented),
        algorithms: provider.signature_verification_algorithms,
    });
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| ClientSetupError(e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    let http = reqwest::Client::builder()
        .use_preconfigured_tls(config)
        .connector_layer(tower::util::MapErrLayer::new(
            lift_tls_error as LiftTlsError,
        ))
        .build()
        .map_err(|e| ClientSetupError(e.to_string()))?;
    Ok(TlsClient { http, presented })
}

/// A connection error, as reqwest's connector boxes it.
type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The type of [`lift_tls_error`], named for the connector layer.
type LiftTlsError = fn(BoxError) -> BoxError;

/// Unwrap the `io::Error` hyper-rustls wraps around tokio-rustls' own.
///
/// tokio-rustls reports a refused handshake as an `io::Error` carrying the
/// `rustls::Error`; hyper-rustls wraps that in a second `io::Error`. An
/// `io::Error`'s `source()` skips its payload, so through two of them the
/// `rustls::Error` — and the [`PinMismatch`] in it — is out of reach of the
/// source chain. One layer removed, it is the payload of the error the chain
/// yields.
fn lift_tls_error(error: BoxError) -> BoxError {
    let outer = match error.downcast::<std::io::Error>() {
        Ok(outer) => outer,
        Err(other) => return other,
    };
    if !outer
        .get_ref()
        .is_some_and(|inner| inner.is::<std::io::Error>())
    {
        return outer;
    }
    let kind = outer.kind();
    // `get_ref` saw a payload, so `into_inner` yields it; the bare kind is
    // only what an error with none would come to.
    outer
        .into_inner()
        .unwrap_or_else(|| Box::new(std::io::Error::from(kind)))
}

/// Whether `err` is a refused key: a [`PinMismatch`] somewhere in its source
/// chain, found by type.
pub fn is_pin_mismatch(err: &reqwest::Error) -> bool {
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(link) = current {
        // An `io::Error` link carries the rustls error as its payload, which its
        // own `source()` skips: the payload is looked at directly.
        let payload = link
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::get_ref)
            .map(|inner| inner as &(dyn std::error::Error + 'static));
        if [Some(link), payload]
            .into_iter()
            .flatten()
            .any(carries_pin_mismatch)
        {
            return true;
        }
        current = link.source();
    }
    false
}

/// Whether `link` is the rustls error that carries a [`PinMismatch`].
fn carries_pin_mismatch(link: &(dyn std::error::Error + 'static)) -> bool {
    matches!(
        link.downcast_ref::<rustls::Error>(),
        Some(rustls::Error::InvalidCertificate(CertificateError::Other(other)))
            if other.0.downcast_ref::<PinMismatch>().is_some()
    )
}

#[cfg(test)]
pub(crate) mod testing;

#[cfg(test)]
mod tests;
