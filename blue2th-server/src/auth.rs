// SPDX-License-Identifier: MIT OR Apache-2.0

//! Authentication for the LAN API (phase 6.4).
//!
//! Until now anything on the LAN — and, through the permissive CORS layer, any
//! web page the user opened — could drive the backend. Every route now requires
//! `Authorization: Bearer <token>`, except `GET /health` (so a wrong token reads
//! as "not paired" rather than "offline") and `POST /pair`.
//!
//! `POST /pair` is **the one open door**. Everything protecting the API rests on
//! the pairing code being armed only briefly, one-shot, and rate limited: a
//! six-character code with unlimited attempts is not a secret. The pure halves —
//! [`verify_code`], [`bearer_token`], [`is_authorised`] — are unit-tested here;
//! [`AuthStore`] keeps a disk-free constructor so no test reads or writes the
//! real `~/.local/state/blue2th/`.

use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use base64::Engine as _;
use rand::Rng as _;

/// How long a minted pairing code stays armed. Short by design: the code is the
/// only thing standing between the LAN and the API.
pub const PAIRING_TTL: Duration = Duration::from_secs(300);

/// How many failed attempts, counted across every armed code, invalidate them
/// all. The code is short, so an attempt cap is not a nicety — it is what makes
/// it a secret at all.
pub const MAX_PAIRING_ATTEMPTS: u32 = 5;

/// Number of characters in a minted pairing code (short enough to type).
pub const PAIRING_CODE_LEN: usize = 6;

/// Minimum entropy of the long-lived API token, in bytes.
pub const TOKEN_ENTROPY_BYTES: usize = 32;

/// File holding the persisted API token under the app-scoped state directory.
const TOKEN_STORE_FILE: &str = "auth.json";

/// Path of the token store, or `None` when no state home can be resolved (the
/// token then stays in memory, which invalidates clients on every restart).
pub fn auth_store_path() -> Option<PathBuf> {
    crate::state_store::state_store_path(TOKEN_STORE_FILE)
}

/// Why a pairing attempt was refused.
///
/// **One variant on purpose**: an unknown code, an expired one, an already-used
/// one and "no code armed at all" must be indistinguishable to the caller, or
/// the reply would help an attacker enumerate. Adding a variant is a security
/// regression, and the tests below fail if one appears.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairError {
    /// The submitted code was not accepted. That is all the client learns.
    Rejected,
}

impl std::fmt::Display for PairError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PairError::Rejected => write!(f, "pairing refused — ask for a fresh pairing code"),
        }
    }
}

impl std::error::Error for PairError {}

/// Mint a long-lived, URL-safe API token carrying at least
/// [`TOKEN_ENTROPY_BYTES`] bytes of entropy, different on every call.
pub fn generate_token() -> String {
    let mut raw = [0u8; TOKEN_ENTROPY_BYTES];
    rand::thread_rng().fill(&mut raw[..]);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw)
}

/// Alphabet of a typed pairing code: unambiguous on a terminal and in a deep
/// link. `0`/`O` and `1`/`I`/`l` are left out — the code is read off a screen and
/// typed on a phone, where a misread costs an attempt against the cap.
const PAIRING_CODE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";

/// A pairing code armed by the server: short, URL-safe (it rides in a deep link
/// and is typed by hand) and short-lived. [`AuthStore`] makes it one-shot and
/// rate limited: it removes a redeemed code and counts failures across all of
/// them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingCode {
    /// The code itself.
    code: String,
    /// When it stops being accepted.
    expires_at: SystemTime,
}

impl PairingCode {
    /// Mint a fresh code, armed for [`PAIRING_TTL`] from `now`.
    pub fn mint(now: SystemTime) -> Self {
        let mut rng = rand::thread_rng();
        let code: String = (0..PAIRING_CODE_LEN)
            .map(|_| {
                let index = rng.gen_range(0..PAIRING_CODE_ALPHABET.len());
                // Indexed inside its own length, so the byte is always there.
                PAIRING_CODE_ALPHABET.get(index).copied().unwrap_or(b'A') as char
            })
            .collect();
        Self::armed(code, now + PAIRING_TTL)
    }

    /// An explicitly armed code — the fixture constructor for the pure
    /// verification tests, and the shape `mint` produces.
    pub fn armed(code: impl Into<String>, expires_at: SystemTime) -> Self {
        Self {
            code: code.into(),
            expires_at,
        }
    }

    /// The code the operator reads off the terminal.
    pub fn code(&self) -> &str {
        &self.code
    }

    /// When the code stops being accepted.
    pub fn expires_at(&self) -> SystemTime {
        self.expires_at
    }
}

/// Whether `submitted` matches the armed code. Pure.
///
/// Accepts only an exactly-matching, unexpired code. Every refusal — no code
/// armed, unknown code, expired code — returns the same [`PairError`], on
/// purpose. A spent code and the attempt cap never reach this check:
/// [`AuthStore::redeem`] removes a redeemed code, and clears every code once
/// [`MAX_PAIRING_ATTEMPTS`] failures add up.
pub fn verify_code(
    stored: Option<&PairingCode>,
    submitted: &str,
    now: SystemTime,
) -> Result<(), PairError> {
    // Every branch below returns the same error on purpose: telling "expired"
    // from "unknown" would help an attacker enumerate.
    let stored = stored.ok_or(PairError::Rejected)?;
    let submitted = blue2th_proto::normalize_pairing_code(submitted);
    if now >= stored.expires_at() || !constant_time_eq(stored.code(), &submitted) {
        return Err(PairError::Rejected);
    }
    Ok(())
}

/// Compare two secrets without an early exit on the first differing byte.
///
/// The pairing code is short-lived and attempt-capped, so a timing oracle is of
/// little use against it — but the same helper guards the long-lived API token,
/// where it matters, and one comparison for both leaves no wrong path to pick.
fn constant_time_eq(expected: &str, presented: &str) -> bool {
    let expected = expected.as_bytes();
    let presented = presented.as_bytes();
    // The length itself is not a secret; only the contents are compared blindly.
    if expected.len() != presented.len() {
        return false;
    }
    expected
        .iter()
        .zip(presented)
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

/// Extract the credential from an `Authorization` header value. Pure.
///
/// `None` for a missing header, a non-`Bearer` scheme or an empty credential.
/// The scheme is compared case-insensitively, as HTTP requires.
pub fn bearer_token(header: Option<&str>) -> Option<&str> {
    let (scheme, credential) = header?.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return None;
    }
    // Leading spaces are dropped — HTTP allows several between the scheme and
    // the credential — but trailing ones are not: the token is opaque, so
    // "s3cret " is a different credential from "s3cret" and must be refused
    // rather than quietly repaired.
    let credential = credential.trim_start();
    (!credential.is_empty()).then_some(credential)
}

/// Whether an `Authorization` header carries exactly `expected`. Pure.
pub fn is_authorised(expected: &str, header: Option<&str>) -> bool {
    bearer_token(header).is_some_and(|presented| constant_time_eq(expected, presented))
}

/// Read the persisted token. `None` for a missing, unreadable or malformed
/// store — all three mean "no usable token", and the caller mints a new one.
fn load_token(path: Option<&Path>) -> Option<String> {
    let raw = std::fs::read_to_string(path?).ok()?;
    let stored: StoredToken = serde_json::from_str(&raw).ok()?;
    (!stored.token.is_empty()).then_some(stored.token)
}

/// Persist the token with owner-only permissions. A failure is logged, never
/// propagated: the server still runs, it simply unpairs clients on restart.
fn persist_token(path: &Path, token: &str) {
    if let Err(e) = write_token(path, token) {
        tracing::warn!("could not persist the API token: {e}");
    }
}

fn write_token(path: &Path, token: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string(&StoredToken {
        // Owned copy: the on-disk shape is serialized from its own value.
        token: token.to_string(),
    })
    .map_err(std::io::Error::other)?;
    std::fs::write(path, body)?;
    // The token is a credential: keep it readable by its owner only, set after
    // writing so it applies to an existing file too.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// On-disk shape of the auth store.
#[derive(serde::Serialize, serde::Deserialize)]
struct StoredToken {
    token: String,
}

/// The backend's API token and the pairing codes currently armed, optionally
/// persisted.
///
/// Following the `state_store` pattern, [`AuthStore::new`] and
/// [`AuthStore::with_token`] are disk-free so tests never read or write the real
/// `~/.local/state/blue2th/` — here that matters twice over: minting a token in
/// a test run would silently unpair the operator's phone.
pub struct AuthStore {
    /// The long-lived bearer token every guarded route requires.
    token: String,
    /// The pairing codes currently armed, unconsumed ones only (#160).
    pairing: Vec<PairingCode>,
    /// Failed redemptions since the codes were armed, counted once for all of
    /// them: n codes must not multiply the attempts an attacker gets.
    failures: u32,
    /// Where the token is persisted, or `None` to stay in memory only.
    store: Option<PathBuf>,
    /// Whether the token was minted rather than reloaded. A minted token
    /// invalidates every paired client, so the caller must open a pairing window
    /// — see [`AuthStore::minted_a_new_token`].
    minted: bool,
}

impl AuthStore {
    /// A fresh token with no store: performs no I/O.
    pub fn new() -> Self {
        Self {
            token: generate_token(),
            pairing: Vec::new(),
            failures: 0,
            store: None,
            minted: true,
        }
    }

    /// A store-free instance around an explicit token — the fixture constructor
    /// for tests, which must never touch the operator's real token.
    pub fn with_token(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            pairing: Vec::new(),
            failures: 0,
            store: None,
            // Handed in, not minted: nothing was invalidated.
            minted: false,
        }
    }

    /// A token backed by a store, reloaded on construction so a restart keeps
    /// paired clients working.
    ///
    /// A missing, unreadable or malformed store mints (and persists) a new
    /// token, which invalidates existing clients — safer than starting with no
    /// authentication at all.
    pub fn with_store(store: Option<PathBuf>) -> Self {
        // Missing, unreadable and malformed all take this one path: mint a new
        // token and persist it. Starting with no authentication because a file
        // could not be read would be the one unacceptable outcome.
        let reloaded = load_token(store.as_deref());
        let minted = reloaded.is_none();
        let token = reloaded.unwrap_or_else(|| {
            let fresh = generate_token();
            if let Some(path) = store.as_deref() {
                persist_token(path, &fresh);
            }
            fresh
        });
        Self {
            token,
            pairing: Vec::new(),
            failures: 0,
            store,
            minted,
        }
    }

    /// Whether the token was minted rather than reloaded from the store.
    ///
    /// The caller **must** open a pairing window when this is true: a minted
    /// token invalidates every paired client, and a store that was merely
    /// unreadable looks exactly like a paired backend from the outside. Without
    /// it the operator's phone would stop working with no code armed to fix it.
    pub fn minted_a_new_token(&self) -> bool {
        self.minted
    }

    /// The current API token.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// The first pairing code still armed, if any.
    pub fn armed(&self) -> Option<&PairingCode> {
        self.pairing.first()
    }

    /// Mint a new API token, persist it and return it. Every paired client must
    /// pair again.
    pub fn rotate(&mut self) -> &str {
        self.token = generate_token();
        if let Some(path) = self.store.as_deref() {
            persist_token(path, &self.token);
        }
        &self.token
    }

    /// Arm `count` fresh pairing codes and return them, replacing any previous
    /// ones. Every armed code redeems once; failed attempts are counted once
    /// for all of them.
    pub fn arm_pairing(&mut self, count: u32, now: SystemTime) -> Vec<String> {
        self.arm_codes(count, || PairingCode::mint(now))
    }

    /// [`AuthStore::arm_pairing`], drawing its codes from `mint`: the seam
    /// through which a test hands it the same code twice.
    fn arm_codes(&mut self, count: u32, mut mint: impl FnMut() -> PairingCode) -> Vec<String> {
        self.pairing.clear();
        self.failures = 0;
        while self.pairing.len() < count as usize {
            let minted = mint();
            // Two equal codes would be one code redeemable twice.
            if self.pairing.iter().all(|c| c.code() != minted.code()) {
                self.pairing.push(minted);
            }
        }
        self.pairing
            .iter()
            // Owned copies: the codes are handed to the operator while the
            // store keeps its own to verify against.
            .map(|c| c.code().to_string())
            .collect()
    }

    /// Exchange a submitted code for the API token.
    ///
    /// Any armed code redeems; on success that code alone is consumed
    /// (one-shot). On failure the attempt is recorded once for all codes, and
    /// reaching [`MAX_PAIRING_ATTEMPTS`] invalidates every armed code. Every
    /// failure returns the same [`PairError`].
    pub fn redeem(&mut self, submitted: &str, now: SystemTime) -> Result<String, PairError> {
        let matched = self
            .pairing
            .iter()
            .position(|code| verify_code(Some(code), submitted, now).is_ok());
        match matched {
            Some(index) => {
                // One-shot: a code that worked once must never work again.
                self.pairing.remove(index);
                Ok(self.token.clone())
            },
            None => {
                if !self.pairing.is_empty() {
                    self.failures = self.failures.saturating_add(1);
                    // The cap is what makes a six-character secret viable at
                    // all; past it the codes are not merely refused, they are
                    // gone.
                    if self.failures >= MAX_PAIRING_ATTEMPTS {
                        self.pairing.clear();
                    }
                }
                Err(PairError::Rejected)
            },
        }
    }

    /// Whether an `Authorization` header value authorises a guarded route.
    pub fn authorises(&self, header: Option<&str>) -> bool {
        is_authorised(&self.token, header)
    }
}

impl Default for AuthStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
