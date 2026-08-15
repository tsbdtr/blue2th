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

/// How many failed attempts invalidate the armed code. The code is short, so an
/// attempt cap is not a nicety — it is what makes it a secret at all.
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
/// and is typed by hand), short-lived, one-shot and rate limited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingCode {
    /// The code itself.
    code: String,
    /// When it stops being accepted.
    expires_at: SystemTime,
    /// Failed attempts so far; [`MAX_PAIRING_ATTEMPTS`] invalidate it.
    attempts: u32,
    /// Whether it was already exchanged for the token (one-shot).
    consumed: bool,
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
            attempts: 0,
            consumed: false,
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

    /// Failed attempts recorded so far.
    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    /// Whether the code was already exchanged for the token.
    pub fn is_consumed(&self) -> bool {
        self.consumed
    }

    /// Mark the code as spent (one-shot).
    pub fn consume(&mut self) {
        self.consumed = true;
    }

    /// Record a failed attempt. The cap itself is applied by [`verify_code`].
    pub fn register_failure(&mut self) {
        self.attempts = self.attempts.saturating_add(1);
    }
}

/// Whether `submitted` matches the armed code. Pure.
///
/// Accepts only an exactly-matching, unexpired, unconsumed code whose attempt
/// count is still under [`MAX_PAIRING_ATTEMPTS`]. Every refusal — no code armed,
/// unknown code, expired code, spent code, attempt cap reached — returns the
/// same [`PairError`], on purpose.
pub fn verify_code(
    stored: Option<&PairingCode>,
    submitted: &str,
    now: SystemTime,
) -> Result<(), PairError> {
    // Every branch below returns the same error on purpose: telling "expired"
    // from "unknown" would help an attacker enumerate.
    let stored = stored.ok_or(PairError::Rejected)?;
    if stored.is_consumed()
        || stored.attempts() >= MAX_PAIRING_ATTEMPTS
        || now >= stored.expires_at()
        || !constant_time_eq(stored.code(), submitted)
    {
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

/// The backend's API token and the pairing code currently armed, optionally
/// persisted.
///
/// Following the `state_store` pattern, [`AuthStore::new`] and
/// [`AuthStore::with_token`] are disk-free so tests never read or write the real
/// `~/.local/state/blue2th/` — here that matters twice over: minting a token in
/// a test run would silently unpair the operator's phone.
pub struct AuthStore {
    /// The long-lived bearer token every guarded route requires.
    token: String,
    /// The pairing code currently armed, if any.
    pairing: Option<PairingCode>,
    /// Where the token is persisted, or `None` to stay in memory only.
    store: Option<PathBuf>,
}

impl AuthStore {
    /// A fresh token with no store: performs no I/O.
    pub fn new() -> Self {
        Self {
            token: generate_token(),
            pairing: None,
            store: None,
        }
    }

    /// A store-free instance around an explicit token — the fixture constructor
    /// for tests, which must never touch the operator's real token.
    pub fn with_token(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            pairing: None,
            store: None,
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
        let token = load_token(store.as_deref()).unwrap_or_else(|| {
            let minted = generate_token();
            if let Some(path) = store.as_deref() {
                persist_token(path, &minted);
            }
            minted
        });
        Self {
            token,
            pairing: None,
            store,
        }
    }

    /// The current API token.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// The pairing code currently armed, if any.
    pub fn armed(&self) -> Option<&PairingCode> {
        self.pairing.as_ref()
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

    /// Arm a fresh pairing code and return it, replacing any previous one.
    pub fn arm_pairing(&mut self, now: SystemTime) -> String {
        let minted = PairingCode::mint(now);
        // Owned copy: the code is handed to the operator while the store keeps
        // its own to verify against.
        let code = minted.code().to_string();
        self.pairing = Some(minted);
        code
    }

    /// Exchange a submitted code for the API token.
    ///
    /// On success the code is consumed (one-shot); on failure the attempt is
    /// recorded, and reaching [`MAX_PAIRING_ATTEMPTS`] invalidates the armed
    /// code. Every failure returns the same [`PairError`].
    pub fn redeem(&mut self, submitted: &str, now: SystemTime) -> Result<String, PairError> {
        match verify_code(self.pairing.as_ref(), submitted, now) {
            Ok(()) => {
                if let Some(code) = self.pairing.as_mut() {
                    // One-shot: a code that worked once must never work again.
                    code.consume();
                }
                Ok(self.token.clone())
            },
            Err(err) => {
                if let Some(code) = self.pairing.as_mut() {
                    code.register_failure();
                    // The cap is what makes a six-character secret viable at
                    // all; past it the code is not merely refused, it is gone.
                    if code.attempts() >= MAX_PAIRING_ATTEMPTS {
                        self.pairing = None;
                    }
                }
                Err(err)
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
mod tests {
    use super::*;

    /// A private, per-test store path under the system temp dir. Never the real
    /// `~/.local/state/blue2th/`: a test run must not be able to unpair the
    /// operator's phone.
    fn temp_store(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("blue2th-test-auth-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("auth.json")
    }

    /// A fixed instant to anchor the expiry tests, so they never depend on the
    /// wall clock.
    fn t0() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    // Criterion: `generate_token()` yields a URL-safe token of at least 32 bytes
    // of entropy. Base64url/hex carry at most 6/4 bits per character, so a token
    // shorter than 43 characters cannot hold 32 bytes.
    #[test]
    fn test_generate_token_is_long_and_url_safe() {
        let token = generate_token();
        assert!(
            token.len() >= 43,
            "a {TOKEN_ENTROPY_BYTES}-byte token cannot be encoded in {} characters: {token}",
            token.len()
        );
        assert!(
            token
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "the token must be URL-safe (it travels in a header and a settings blob), got {token}"
        );
    }

    // Criterion: the token is different on every call — a predictable token is
    // no token at all.
    #[test]
    fn test_generate_token_differs_on_every_call() {
        let tokens: Vec<String> = (0..8).map(|_| generate_token()).collect();
        let mut unique = tokens.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), tokens.len(), "tokens repeated: {tokens:?}");
    }

    // Criterion: `PairingCode::mint()` is short and URL-safe — it is typed by
    // hand and rides inside a `blue2th://pair?…` deep link.
    #[test]
    fn test_pairing_code_mint_is_short_and_url_safe() {
        let code = PairingCode::mint(t0());
        assert_eq!(code.code().chars().count(), PAIRING_CODE_LEN);
        assert!(
            code.code().chars().all(|c| c.is_ascii_alphanumeric()),
            "a typed code must stay alphanumeric, got {}",
            code.code()
        );
        assert_eq!(code.attempts(), 0);
        assert!(!code.is_consumed());
    }

    // Criterion: a minted code is different every time, or the "one-shot" rule
    // would protect nothing.
    #[test]
    fn test_pairing_code_mint_differs_on_every_call() {
        let codes: Vec<String> = (0..8)
            .map(|_| PairingCode::mint(t0()).code().to_string())
            .collect();
        let mut unique = codes.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), codes.len(), "codes repeated: {codes:?}");
    }

    // Criterion: `PairingCode::mint()` expires after `PAIRING_TTL`.
    #[test]
    fn test_pairing_code_mint_expires_after_the_ttl() {
        let code = PairingCode::mint(t0());
        assert_eq!(code.expires_at(), t0() + PAIRING_TTL);
    }

    // Criterion: `verify_code` accepts an unexpired, unconsumed, exactly
    // matching code.
    #[test]
    fn test_verify_code_accepts_the_armed_code() {
        let stored = PairingCode::armed("K7M2QX", t0() + PAIRING_TTL);
        assert_eq!(verify_code(Some(&stored), "K7M2QX", t0()), Ok(()));
    }

    // Criterion: only an *exactly* matching code is accepted — not a prefix, not
    // a code with something appended, not a different one.
    #[test]
    fn test_verify_code_rejects_anything_but_an_exact_match() {
        let stored = PairingCode::armed("K7M2QX", t0() + PAIRING_TTL);
        for submitted in ["K7M2Q", "K7M2QX7", "AAAAAA", "", " K7M2QX"] {
            assert_eq!(
                verify_code(Some(&stored), submitted, t0()),
                Err(PairError::Rejected),
                "{submitted:?} is not the armed code"
            );
        }
    }

    // Criterion (non-nominal, security): an expired code and an unknown one are
    // **indistinguishable** — same error value and same message, so the reply
    // cannot be used to enumerate. "No code armed" reads the same way.
    #[test]
    fn test_verify_code_cannot_tell_expired_from_unknown_or_unarmed() {
        let expired = PairingCode::armed("K7M2QX", t0());
        let armed = PairingCode::armed("K7M2QX", t0() + PAIRING_TTL);
        let after_ttl = t0() + PAIRING_TTL + Duration::from_secs(1);

        let expired_err = verify_code(Some(&expired), "K7M2QX", after_ttl)
            .expect_err("an expired code must be refused");
        let unknown_err =
            verify_code(Some(&armed), "AAAAAA", t0()).expect_err("an unknown code must be refused");
        let unarmed_err =
            verify_code(None, "K7M2QX", t0()).expect_err("with no code armed nothing is accepted");

        assert_eq!(expired_err, unknown_err);
        assert_eq!(expired_err, unarmed_err);
        assert_eq!(expired_err.to_string(), unknown_err.to_string());
        assert_eq!(expired_err.to_string(), unarmed_err.to_string());
    }

    // Criterion: the code expires after `PAIRING_TTL` — accepted right up to the
    // deadline, refused past it.
    #[test]
    fn test_verify_code_rejects_a_code_past_its_expiry() {
        let stored = PairingCode::armed("K7M2QX", t0() + PAIRING_TTL);
        assert_eq!(
            verify_code(
                Some(&stored),
                "K7M2QX",
                t0() + PAIRING_TTL - Duration::from_secs(1)
            ),
            Ok(())
        );
        assert_eq!(
            verify_code(
                Some(&stored),
                "K7M2QX",
                t0() + PAIRING_TTL + Duration::from_secs(1)
            ),
            Err(PairError::Rejected)
        );
    }

    // Criterion: a code is one-shot — once consumed, verifying it again fails.
    #[test]
    fn test_verify_code_rejects_a_consumed_code() {
        let mut stored = PairingCode::armed("K7M2QX", t0() + PAIRING_TTL);
        stored.consume();
        assert_eq!(
            verify_code(Some(&stored), "K7M2QX", t0()),
            Err(PairError::Rejected)
        );
    }

    // Criterion: after `MAX_PAIRING_ATTEMPTS` failures the armed code is
    // invalidated — even the right code no longer works.
    #[test]
    fn test_verify_code_rejects_the_right_code_once_the_attempt_cap_is_reached() {
        let mut stored = PairingCode::armed("K7M2QX", t0() + PAIRING_TTL);
        for _ in 0..MAX_PAIRING_ATTEMPTS {
            stored.register_failure();
        }
        assert_eq!(
            verify_code(Some(&stored), "K7M2QX", t0()),
            Err(PairError::Rejected),
            "a short code with unlimited attempts is not a secret"
        );
    }

    // Criterion: the cap is not tripped early — one attempt short of it, the
    // right code is still accepted (a typo must not lock the operator out).
    #[test]
    fn test_verify_code_still_accepts_the_right_code_below_the_attempt_cap() {
        let mut stored = PairingCode::armed("K7M2QX", t0() + PAIRING_TTL);
        for _ in 0..MAX_PAIRING_ATTEMPTS - 1 {
            stored.register_failure();
        }
        assert_eq!(verify_code(Some(&stored), "K7M2QX", t0()), Ok(()));
    }

    // Criterion: `POST /pair` with a valid armed code returns the token — at the
    // store level, redeeming returns exactly the stored token.
    #[test]
    fn test_redeem_returns_the_stored_token() {
        let mut store = AuthStore::with_token("stored-token-value");
        let code = store.arm_pairing(t0());
        assert_eq!(
            store.redeem(&code, t0()),
            Ok("stored-token-value".to_string())
        );
    }

    // Criterion: a code is one-shot — the second redemption of the same code
    // fails, even straight away and even though it has not expired.
    #[test]
    fn test_redeem_consumes_the_code_so_a_second_use_fails() {
        let mut store = AuthStore::with_token("stored-token-value");
        let code = store.arm_pairing(t0());
        assert!(store.redeem(&code, t0()).is_ok());
        assert_eq!(store.redeem(&code, t0()), Err(PairError::Rejected));
    }

    // Criterion: after `MAX_PAIRING_ATTEMPTS` failures the armed code is
    // invalidated — brute force on the one open door is capped.
    #[test]
    fn test_redeem_invalidates_the_code_after_the_attempt_cap() {
        let mut store = AuthStore::with_token("stored-token-value");
        let code = store.arm_pairing(t0());
        for _ in 0..MAX_PAIRING_ATTEMPTS {
            assert_eq!(store.redeem("AAAAAA", t0()), Err(PairError::Rejected));
        }
        assert_eq!(
            store.redeem(&code, t0()),
            Err(PairError::Rejected),
            "the armed code must be dead once the cap is reached"
        );
    }

    // Criterion (non-nominal): pairing while no code is armed is refused, with
    // the same error as a wrong code.
    #[test]
    fn test_redeem_without_an_armed_code_is_refused() {
        let mut store = AuthStore::with_token("stored-token-value");
        assert!(store.armed().is_none());
        assert_eq!(store.redeem("K7M2QX", t0()), Err(PairError::Rejected));
    }

    // Criterion: arming a code replaces the previous one — the operator restarts
    // the server (or runs `--pair`) to mint a fresh code, and the old one dies.
    #[test]
    fn test_arming_a_new_code_retires_the_previous_one() {
        let mut store = AuthStore::with_token("stored-token-value");
        let first = store.arm_pairing(t0());
        let second = store.arm_pairing(t0());
        assert_ne!(first, second);
        assert_eq!(store.redeem(&first, t0()), Err(PairError::Rejected));
        assert!(store.redeem(&second, t0()).is_ok());
    }

    // Criterion: the auth token is persisted and reloaded on restart, so a
    // paired phone keeps working across a server restart.
    #[test]
    fn test_auth_store_round_trips_the_token_through_its_store() {
        let path = temp_store("round-trip");
        let minted = AuthStore::with_store(Some(path.clone()))
            .token()
            .to_string();
        let reloaded = AuthStore::with_store(Some(path.clone()));
        assert_eq!(reloaded.token(), minted);
        let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
    }

    // Criterion: the token store is written `0600` — it is the credential to the
    // whole API, and every user on the box can otherwise read it.
    #[cfg(unix)]
    #[test]
    fn test_auth_store_persists_the_token_with_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let path = temp_store("permissions");
        let _store = AuthStore::with_store(Some(path.clone()));
        let mode = std::fs::metadata(&path)
            .expect("the token store must have been written")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "got {mode:o}");
        let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
    }

    // Criterion (non-nominal): a malformed store mints a new token rather than
    // disabling authentication — and persists it, so the next start is stable.
    #[test]
    fn test_auth_store_with_a_malformed_store_mints_a_new_token() {
        let path = temp_store("malformed");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create the store directory");
        }
        std::fs::write(&path, "{ not json at all").expect("write a malformed store");

        let store = AuthStore::with_store(Some(path.clone()));
        assert!(
            !store.token().trim().is_empty(),
            "a malformed store must never leave the API unauthenticated"
        );
        let reloaded = AuthStore::with_store(Some(path.clone()));
        assert_eq!(
            reloaded.token(),
            store.token(),
            "the newly minted token must have been persisted"
        );
        let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
    }

    // Criterion: a missing store mints and persists a token on the first run —
    // authentication is mandatory from the very first start, no migration.
    #[test]
    fn test_auth_store_with_a_missing_store_mints_and_persists() {
        let path = temp_store("missing");
        let store = AuthStore::with_store(Some(path.clone()));
        assert!(!store.token().trim().is_empty());
        assert!(
            path.exists(),
            "the token must have been written to {path:?}"
        );
        let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
    }

    // Criterion (test isolation): `new()` is disk-free — two instances hold
    // different tokens, which they could not if either had read a shared file.
    #[test]
    fn test_auth_store_new_is_disk_free() {
        assert_ne!(AuthStore::new().token(), AuthStore::new().token());
    }

    // Criterion: `rotate` mints a new token and persists it, so an operator can
    // revoke every paired client.
    #[test]
    fn test_rotate_replaces_and_persists_the_token() {
        let path = temp_store("rotate");
        let mut store = AuthStore::with_store(Some(path.clone()));
        let before = store.token().to_string();
        let after = store.rotate().to_string();
        assert_ne!(before, after);
        assert_eq!(
            AuthStore::with_store(Some(path.clone())).token(),
            after,
            "the rotated token must be the one a restart reloads"
        );
        let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
    }

    // Criterion: every route requires `Authorization: Bearer <token>` — the
    // header parser reads the credential, case-insensitively on the scheme.
    #[test]
    fn test_bearer_token_extracts_the_credential() {
        assert_eq!(bearer_token(Some("Bearer abc123")), Some("abc123"));
        assert_eq!(bearer_token(Some("bearer abc123")), Some("abc123"));
        assert_eq!(bearer_token(Some("Bearer   abc123")), Some("abc123"));
    }

    // Criterion: a missing or malformed `Authorization` header carries no
    // credential at all (and must therefore be refused).
    #[test]
    fn test_bearer_token_rejects_a_missing_or_malformed_header() {
        for header in [
            None,
            Some(""),
            Some("Bearer"),
            Some("Bearer "),
            Some("abc123"),
            Some("Basic abc123"),
        ] {
            assert_eq!(bearer_token(header), None, "{header:?} carries no bearer");
        }
    }

    // Criterion: the guard accepts exactly the stored token and nothing else.
    #[test]
    fn test_is_authorised_accepts_only_the_stored_token() {
        assert!(is_authorised("s3cret", Some("Bearer s3cret")));
        assert!(!is_authorised("s3cret", Some("Bearer s3cre")));
        assert!(!is_authorised("s3cret", Some("Bearer s3cret ")));
        assert!(!is_authorised("s3cret", Some("Bearer S3CRET")));
        assert!(!is_authorised("s3cret", None));
    }

    // Criterion: the store applies the same rule as the pure helper — the guard
    // has one implementation, not two.
    #[test]
    fn test_auth_store_authorises_its_own_token_only() {
        let store = AuthStore::with_token("s3cret");
        assert!(store.authorises(Some("Bearer s3cret")));
        assert!(!store.authorises(Some("Bearer other")));
        assert!(!store.authorises(None));
    }
}
