// SPDX-License-Identifier: MIT OR Apache-2.0

//! The API's TLS (#195): a self-signed certificate on a persistent ECDSA P-256
//! key kept in the state directory, the decision to reissue it, its
//! fingerprint, and the `rustls` configuration the listener serves it with.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub use rcgen::KeyPair;
use tokio_rustls::rustls;

/// The key file in the state directory.
pub const KEY_FILE: &str = "tls-key.pem";

/// The certificate file in the state directory.
pub const CERT_FILE: &str = "tls-cert.pem";

/// What [`load_or_create_key`] found or made.
pub struct KeyOutcome {
    /// The key the certificate is built on.
    pub key: KeyPair,
    /// Whether the key was generated at this start rather than reloaded.
    pub minted: bool,
    /// Whether the key is on disk (reloaded, or generated and written).
    pub persisted: bool,
}

/// Why a start was refused over the key file.
#[derive(Debug)]
pub struct KeyRefusal {
    /// The key file that could not be used.
    pub path: PathBuf,
    /// What was wrong with it.
    pub cause: String,
}

impl std::fmt::Display for KeyRefusal {
    fn fmt(&self, _f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Ok(())
    }
}

impl std::error::Error for KeyRefusal {}

/// What a stored certificate says, as the reissue decision reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertFacts {
    /// The IP addresses among its subject alternative names.
    pub ips: Vec<IpAddr>,
    /// The DNS names among its subject alternative names.
    pub dns_names: Vec<String>,
    /// Start of its validity.
    pub not_before: SystemTime,
    /// End of its validity.
    pub not_after: SystemTime,
    /// Its `SubjectPublicKeyInfo`, DER.
    pub spki: Vec<u8>,
}

/// Why a stored certificate is not reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReissueReason {
    /// Missing, unreadable or malformed.
    Unusable,
    /// Its IP addresses are not the advertised one.
    IpChanged,
    /// Its DNS names are not the derived `.local` name.
    NameChanged,
    /// It certifies another key than the stored one.
    KeyChanged,
    /// 30 days or fewer left.
    ExpiringSoon,
    /// Its `notBefore` is in the future.
    NotYetValid,
}

impl std::fmt::Display for ReissueReason {
    fn fmt(&self, _f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Ok(())
    }
}

/// A certificate, as served and as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuedCert {
    /// DER, what the listener serves and the fingerprint is taken over.
    pub der: Vec<u8>,
    /// PEM, what the certificate file holds.
    pub pem: String,
}

/// What [`load_or_issue_certificate`] served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertOutcome {
    /// The certificate to serve.
    pub cert: IssuedCert,
    /// Why it was issued at this start, `None` when the stored one was reused.
    pub reissued: Option<ReissueReason>,
    /// Whether the served certificate is the one on disk.
    pub persisted: bool,
}

/// The host label announced over mDNS and carried by the certificate, derived
/// from the backend id.
pub fn local_host_label(_id: &str) -> String {
    String::new()
}

/// Load the key at `path`, or generate one and write it there.
pub fn load_or_create_key(path: Option<&Path>) -> Result<KeyOutcome, KeyRefusal> {
    Err(KeyRefusal {
        path: path.map(Path::to_path_buf).unwrap_or_default(),
        cause: String::new(),
    })
}

/// The facts of a stored PEM certificate, `None` when it does not parse.
pub fn parse_cert_facts(_pem: &str) -> Option<CertFacts> {
    None
}

/// Whether `stored` must be reissued, and why. `None` reuses it.
pub fn reissue_reason(
    _stored: Option<&CertFacts>,
    _key_spki: &[u8],
    _ip: Option<IpAddr>,
    _name: &str,
    _now: SystemTime,
) -> Option<ReissueReason> {
    None
}

/// Issue a self-signed certificate on `key` for `ip` and the DNS name `name`.
pub fn issue_certificate(
    _key: &KeyPair,
    _ip: Option<IpAddr>,
    _name: &str,
    _now: SystemTime,
) -> Result<IssuedCert, rcgen::Error> {
    Err(rcgen::Error::CouldNotParseCertificate)
}

/// Reuse the certificate stored at `path`, or issue one on `key` and write it.
pub fn load_or_issue_certificate(
    _path: Option<&Path>,
    _key: &KeyPair,
    _ip: Option<IpAddr>,
    _name: &str,
    _now: SystemTime,
) -> Result<CertOutcome, rcgen::Error> {
    Err(rcgen::Error::CouldNotParseCertificate)
}

/// The log line of a reissue.
pub fn reissue_message(_reason: ReissueReason) -> String {
    String::new()
}

/// The SHA-256 fingerprint of a DER certificate, as a browser displays it.
pub fn certificate_fingerprint(_cert_der: &[u8]) -> String {
    String::new()
}

/// The `rustls` server configuration serving `cert` with `key`.
pub fn server_config(
    _cert: &IssuedCert,
    _key: &KeyPair,
) -> Result<rustls::ServerConfig, rustls::Error> {
    Err(rustls::Error::General(String::new()))
}

#[cfg(test)]
mod tests;
