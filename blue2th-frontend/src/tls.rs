// SPDX-License-Identifier: MIT OR Apache-2.0

//! The app's side of the backend's TLS (#196): the backend is trusted by the
//! SHA-256 of its certificate's public key (its SPKI), pinned per backend next
//! to its token, never by a certificate authority.
//!
//! Native targets only: the browser build reaches the backend through `fetch`,
//! and the browser does the TLS.

use std::sync::{Arc, Mutex};

/// The SHA-256 of a backend's `SubjectPublicKeyInfo`, the whole DER of it.
///
/// Stored in a [`crate::settings::BackendEntry`] as 64 lower-case hex
/// characters, read back through [`Pin::parse`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Pin([u8; 32]);

impl Pin {
    /// The pin a stored value names, or `None` when it names none: empty, not
    /// 64 characters, or not lower-case hex.
    pub fn parse(_stored: &str) -> Option<Pin> {
        // RED stub (#196): the parsing is the green phase's.
        None
    }

    /// The pin of a public key, from the DER of its whole `SubjectPublicKeyInfo`.
    pub fn of_spki(_spki_der: &[u8]) -> Pin {
        // RED stub (#196): the hashing is the green phase's.
        Pin([0; 32])
    }

    /// The stored form: 64 lower-case hex characters.
    pub fn to_hex(&self) -> String {
        // RED stub (#196).
        String::new()
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

/// The one constructor of every client the app talks to a backend with: for
/// `url`, accepting only `pin` when there is one, any key otherwise.
pub fn client_for(_url: &str, _pin: Option<Pin>) -> Result<TlsClient, ClientSetupError> {
    // RED stub (#196): a client with no TLS at all.
    let http = reqwest::Client::builder()
        .build()
        .map_err(|e| ClientSetupError(e.to_string()))?;
    Ok(TlsClient {
        http,
        presented: Arc::new(Mutex::new(None)),
    })
}

/// Whether `err` is a refused key: a [`PinMismatch`] somewhere in its source
/// chain, found by type.
pub fn is_pin_mismatch(_err: &reqwest::Error) -> bool {
    // RED stub (#196).
    false
}

#[cfg(test)]
pub(crate) mod testing;

#[cfg(test)]
mod tests;
