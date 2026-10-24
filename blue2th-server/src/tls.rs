// SPDX-License-Identifier: MIT OR Apache-2.0

//! The API's TLS (#195): a self-signed certificate on a persistent ECDSA P-256
//! key kept in the state directory, the decision to reissue it, its
//! fingerprint, and the `rustls` configuration the listener serves it with.

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

pub use rcgen::KeyPair;
use sha2::{Digest as _, Sha256};
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use x509_parser::extensions::GeneralName;

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
pub struct KeyRefusal {
    /// The key file that could not be used.
    pub path: PathBuf,
    /// What was wrong with it.
    pub cause: String,
}

/// The message itself, not a struct dump: `main` returns the refusal, and the
/// runtime prints a returned error with `Debug`.
impl std::fmt::Debug for KeyRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

impl std::fmt::Display for KeyRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "refusing to start: the TLS key {} cannot be used ({}). Restore it if you \
             can; otherwise delete it to generate a new one, after which every client \
             must pair again",
            self.path.display(),
            self.cause
        )
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
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ReissueReason::Unusable => "the stored certificate is missing, unreadable or malformed",
            ReissueReason::IpChanged => "the advertised IP address changed",
            ReissueReason::NameChanged => "the .local host name changed",
            ReissueReason::KeyChanged => "the stored certificate is for another key",
            ReissueReason::ExpiringSoon => "the stored certificate expires within 30 days",
            ReissueReason::NotYetValid => "the stored certificate is not valid yet",
        })
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
///
/// Hashed rather than taken as it is: the id is base64url, whose `_` and upper
/// case a host label does not carry, and which may end in `-`.
pub fn local_host_label(id: &str) -> String {
    let digest = Sha256::digest(id.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("{LABEL_PREFIX}{}", &hex[..LABEL_HEX_LEN])
}

/// The prefix of [`local_host_label`].
const LABEL_PREFIX: &str = "blue2th-";

/// How many hex characters of the id's hash [`local_host_label`] keeps.
const LABEL_HEX_LEN: usize = 10;

/// How long a certificate is issued for.
const VALIDITY: Duration = Duration::from_secs(365 * 24 * 60 * 60);

/// A stored certificate with this much validity left, or less, is reissued.
const RENEW_BEFORE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Load the key at `path`, or generate one and write it there.
///
/// A key that exists but cannot be read or parsed refuses the start rather
/// than being replaced: it may be recoverable, and an overwritten one is not.
pub fn load_or_create_key(path: Option<&Path>) -> Result<KeyOutcome, KeyRefusal> {
    if let Some(path) = path {
        match std::fs::read_to_string(path) {
            Ok(pem) => {
                let key = KeyPair::from_pem(&pem).map_err(|e| KeyRefusal {
                    path: path.to_path_buf(),
                    cause: e.to_string(),
                })?;
                return Ok(KeyOutcome {
                    key,
                    minted: false,
                    persisted: true,
                });
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
            Err(e) => {
                return Err(KeyRefusal {
                    path: path.to_path_buf(),
                    cause: e.to_string(),
                })
            },
        }
    }

    let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).map_err(|e| KeyRefusal {
        path: path.map(Path::to_path_buf).unwrap_or_default(),
        cause: format!("could not generate a key: {e}"),
    })?;
    let persisted = match path {
        Some(path) => match write_replacing(path, key.serialize_pem().as_bytes(), 0o600) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(
                    "could not write the TLS key to {} ({e}); it is kept in memory, \
                     and the next start generates another",
                    path.display()
                );
                false
            },
        },
        None => {
            tracing::warn!("no state directory: the TLS key is kept in memory only");
            false
        },
    };
    Ok(KeyOutcome {
        key,
        minted: true,
        persisted,
    })
}

/// The facts of a stored PEM certificate, `None` when it does not parse.
pub fn parse_cert_facts(pem: &str) -> Option<CertFacts> {
    parse_pem_certificate(pem).map(|(facts, _)| facts)
}

/// The facts and the DER of a PEM certificate, `None` when it does not parse.
fn parse_pem_certificate(pem: &str) -> Option<(CertFacts, Vec<u8>)> {
    let (_, block) = x509_parser::pem::parse_x509_pem(pem.as_bytes()).ok()?;
    if block.label != "CERTIFICATE" {
        return None;
    }
    let (_, cert) = x509_parser::parse_x509_certificate(&block.contents).ok()?;
    let mut ips = Vec::new();
    let mut dns_names = Vec::new();
    if let Some(sans) = cert.subject_alternative_name().ok().flatten() {
        for name in &sans.value.general_names {
            match name {
                GeneralName::DNSName(dns) => dns_names.push((*dns).to_string()),
                GeneralName::IPAddress(bytes) => {
                    if let Ok(v4) = <[u8; 4]>::try_from(*bytes) {
                        ips.push(IpAddr::from(v4));
                    } else if let Ok(v6) = <[u8; 16]>::try_from(*bytes) {
                        ips.push(IpAddr::from(v6));
                    }
                },
                _ => {},
            }
        }
    }
    let facts = CertFacts {
        ips,
        dns_names,
        not_before: system_time_of(cert.validity().not_before.timestamp())?,
        not_after: system_time_of(cert.validity().not_after.timestamp())?,
        spki: cert.public_key().raw.to_vec(),
    };
    Some((facts, block.contents))
}

/// The instant `seconds` after (or before) the Unix epoch.
fn system_time_of(seconds: i64) -> Option<SystemTime> {
    let magnitude = Duration::from_secs(seconds.unsigned_abs());
    if seconds >= 0 {
        SystemTime::UNIX_EPOCH.checked_add(magnitude)
    } else {
        SystemTime::UNIX_EPOCH.checked_sub(magnitude)
    }
}

/// The Unix timestamp of `time`, in whole seconds.
fn unix_seconds(time: SystemTime) -> Option<i64> {
    match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_secs()).ok(),
        Err(before) => i64::try_from(before.duration().as_secs()).ok().map(|s| -s),
    }
}

/// Whether `stored` must be reissued, and why. `None` reuses it.
pub fn reissue_reason(
    stored: Option<&CertFacts>,
    key_spki: &[u8],
    ip: Option<IpAddr>,
    name: &str,
    now: SystemTime,
) -> Option<ReissueReason> {
    let Some(stored) = stored else {
        return Some(ReissueReason::Unusable);
    };
    if stored.spki != key_spki {
        return Some(ReissueReason::KeyChanged);
    }
    // Compared as sets: a certificate carrying an IP while none is advertised
    // no longer describes the backend either.
    let certified: BTreeSet<IpAddr> = stored.ips.iter().copied().collect();
    let advertised: BTreeSet<IpAddr> = ip.into_iter().collect();
    if certified != advertised {
        return Some(ReissueReason::IpChanged);
    }
    let names: BTreeSet<&str> = stored.dns_names.iter().map(String::as_str).collect();
    if names != BTreeSet::from([name]) {
        return Some(ReissueReason::NameChanged);
    }
    if stored.not_before > now {
        return Some(ReissueReason::NotYetValid);
    }
    // What is left, rather than `now + RENEW_BEFORE`: that sum can overflow,
    // and an overflow must not read as "reuse".
    match stored.not_after.duration_since(now) {
        Ok(left) if left > RENEW_BEFORE => None,
        _ => Some(ReissueReason::ExpiringSoon),
    }
}

/// Issue a self-signed certificate on `key` for `ip` and the DNS name `name`.
pub fn issue_certificate(
    key: &KeyPair,
    ip: Option<IpAddr>,
    name: &str,
    now: SystemTime,
) -> Result<IssuedCert, rcgen::Error> {
    let mut sans: Vec<String> = ip.iter().map(IpAddr::to_string).collect();
    sans.push(name.to_string());
    let mut params = rcgen::CertificateParams::new(sans)?;
    let start = unix_seconds(now).ok_or(rcgen::Error::Time)?;
    let end = unix_seconds(now.checked_add(VALIDITY).ok_or(rcgen::Error::Time)?)
        .ok_or(rcgen::Error::Time)?;
    params.not_before =
        time::OffsetDateTime::from_unix_timestamp(start).map_err(|_| rcgen::Error::Time)?;
    params.not_after =
        time::OffsetDateTime::from_unix_timestamp(end).map_err(|_| rcgen::Error::Time)?;
    params.is_ca = rcgen::IsCa::ExplicitNoCa;
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, name);
    // A random serial: rcgen derives one from the key otherwise, so a reissue
    // on the same key would repeat issuer and serial, which Firefox refuses.
    let mut serial: [u8; 16] = rand::random();
    serial[0] &= 0x7f;
    params.serial_number = Some(rcgen::SerialNumber::from_slice(&serial));
    let cert = params.self_signed(key)?;
    Ok(IssuedCert {
        der: cert.der().to_vec(),
        pem: cert.pem(),
    })
}

/// Reuse the certificate stored at `path`, or issue one on `key` and write it.
pub fn load_or_issue_certificate(
    path: Option<&Path>,
    key: &KeyPair,
    ip: Option<IpAddr>,
    name: &str,
    now: SystemTime,
) -> Result<CertOutcome, rcgen::Error> {
    let stored = path
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|pem| parse_pem_certificate(&pem).map(|(facts, der)| (facts, der, pem)));
    let spki = rcgen::PublicKeyData::subject_public_key_info(key);
    let reason = reissue_reason(
        stored.as_ref().map(|(facts, ..)| facts),
        &spki,
        ip,
        name,
        now,
    );
    match (reason, stored) {
        (None, Some((_, der, pem))) => Ok(CertOutcome {
            cert: IssuedCert { der, pem },
            reissued: None,
            persisted: true,
        }),
        // No stored certificate always has a reason, and `Unusable` is it.
        (reason, _) => load_or_issue_fresh(
            path,
            key,
            ip,
            name,
            now,
            reason.unwrap_or(ReissueReason::Unusable),
        ),
    }
}

/// Issue a certificate for `reason` and write it to `path` when there is one.
fn load_or_issue_fresh(
    path: Option<&Path>,
    key: &KeyPair,
    ip: Option<IpAddr>,
    name: &str,
    now: SystemTime,
    reason: ReissueReason,
) -> Result<CertOutcome, rcgen::Error> {
    let cert = issue_certificate(key, ip, name, now)?;
    let persisted = match path {
        Some(path) => match write_replacing(path, cert.pem.as_bytes(), 0o644) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(
                    "could not write the TLS certificate to {} ({e}); it is served from \
                     memory, and the next start issues another",
                    path.display()
                );
                false
            },
        },
        None => false,
    };
    Ok(CertOutcome {
        cert,
        reissued: Some(reason),
        persisted,
    })
}

/// Write `contents` to `path` with `mode`, through a temporary file in the same
/// directory renamed onto it, so that a crash never leaves half a file there.
///
/// A leftover temporary file is removed first: `OpenOptions::mode` applies on
/// creation only, so writing into a `0644` leftover would keep its mode.
fn write_replacing(path: &Path, contents: &[u8], mode: u32) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

    if let Some(parent) = path.parent() {
        crate::state_store::ensure_state_dir(parent)?;
    }
    let mut tmp_name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("no file name"))?
        .to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    match std::fs::remove_file(&tmp) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
        _ => {},
    }
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)
        .and_then(|mut file| {
            file.write_all(contents)?;
            file.sync_all()?;
            // The umask may have narrowed the creation mode.
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))
        })
        .and_then(|()| std::fs::rename(&tmp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// The log line of a reissue.
pub fn reissue_message(reason: ReissueReason) -> String {
    format!(
        "issuing a new TLS certificate: {reason}; a browser will warn again until \
         the new certificate is accepted"
    )
}

/// The SHA-256 fingerprint of a DER certificate, as a browser displays it.
pub fn certificate_fingerprint(cert_der: &[u8]) -> String {
    Sha256::digest(cert_der)
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// The `rustls` server configuration serving `cert` with `key`.
pub fn server_config(
    cert: &IssuedCert,
    key: &KeyPair,
) -> Result<rustls::ServerConfig, rustls::Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    // The configuration owns its chain; the caller keeps its certificate.
    let chain = vec![CertificateDer::from(cert.der.clone())];
    let private = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(chain, private)?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(config)
}

#[cfg(test)]
mod tests;
