//! The backend's stable identity (phase 6.6).
//!
//! A backend used to be identified by its URL alone, so a DHCP lease change made
//! the app create a *second* entry for the same machine. It now mints a stable id
//! once, publishes it over mDNS, and the app matches on that instead.
//!
//! The id lives in its own store, **separate from the token** (`auth.rs`): wiping
//! `auth.json` must re-mint the token without changing who the machine is, or
//! re-pairing would still duplicate the entry. Following the `state_store`
//! pattern, [`BackendIdentity::new`] is disk-free so tests never read or write
//! the real `~/.local/state/blue2th/`.

use std::path::PathBuf;

/// File holding the persisted backend id under the app-scoped state directory.
const IDENTITY_STORE_FILE: &str = "identity.json";

/// Path of the identity store, or `None` when no state home can be resolved (the
/// id then stays in memory, which re-mints it on every restart).
pub fn identity_store_path() -> Option<PathBuf> {
    crate::state_store::state_store_path(IDENTITY_STORE_FILE)
}

/// The stable id this backend advertises, optionally persisted.
pub struct BackendIdentity {
    /// The current id: reloaded from the store, or freshly minted.
    #[allow(dead_code)]
    id: String,
    /// Where the id is persisted, or `None` to stay in memory only.
    #[allow(dead_code)]
    store: Option<PathBuf>,
    /// Whether the id was minted rather than reloaded.
    #[allow(dead_code)]
    minted: bool,
}

/// Mint a URL-safe stable id, different on every call. It rides in a TXT record
/// and in the app's settings blob, so it stays in the URL-safe alphabet.
pub fn generate_id() -> String {
    todo!("phase 6.6: mint a URL-safe stable backend id")
}

impl BackendIdentity {
    /// A fresh id with no store: performs no I/O.
    pub fn new() -> Self {
        todo!("phase 6.6: mint a store-free identity")
    }

    /// An id backed by a store, reloaded on construction so a restart keeps the
    /// same identity. A missing, unreadable or malformed store mints (and
    /// persists) a new one.
    pub fn with_store(store: Option<PathBuf>) -> Self {
        let _ = store;
        todo!("phase 6.6: reload or mint and persist the backend id")
    }

    /// The current stable id.
    pub fn id(&self) -> &str {
        todo!("phase 6.6: expose the stable id")
    }

    /// Whether the id was minted rather than reloaded from the store. A minted
    /// id makes this machine look brand new to every app that knew it, so it is
    /// worth a log line — unlike a reloaded one.
    pub fn minted_a_new_id(&self) -> bool {
        todo!("phase 6.6: report whether the id was minted")
    }
}

impl Default for BackendIdentity {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A private, per-test state directory under the system temp dir. Never the
    /// real `~/.local/state/blue2th/`: a test run must not be able to change the
    /// operator's backend identity.
    fn temp_state_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("blue2th-test-identity-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    // Criterion: `BackendIdentity` mints a URL-safe stable id — it travels in an
    // mDNS TXT record and in the app's settings blob.
    #[test]
    fn test_generate_id_is_url_safe_and_non_empty() {
        let id = generate_id();
        assert!(!id.is_empty(), "an empty id identifies nothing");
        assert!(
            id.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "the id must be URL-safe (TXT record + settings blob), got {id}"
        );
    }

    // Criterion: the id is minted, not derived from something guessable — two
    // backends on the same LAN must never collide.
    #[test]
    fn test_generate_id_differs_on_every_call() {
        let ids: Vec<String> = (0..8).map(|_| generate_id()).collect();
        let mut unique = ids.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), ids.len(), "ids repeated: {ids:?}");
    }

    // Criterion: a store-free identity performs no I/O and still carries an id
    // (the fixture constructor, as `AuthStore::new` is).
    #[test]
    fn test_new_identity_has_an_id_and_no_store() {
        let identity = BackendIdentity::new();
        assert!(!identity.id().is_empty());
        assert!(
            identity.minted_a_new_id(),
            "a store-free identity was minted, not reloaded"
        );
    }

    // Criterion: `BackendIdentity` persists the minted id to
    // `$XDG_STATE_HOME/blue2th/identity.json` and reloads the same id afterwards.
    #[test]
    fn test_identity_round_trips_through_its_store() {
        let store = temp_state_dir("round-trip").join(IDENTITY_STORE_FILE);

        let first = BackendIdentity::with_store(Some(store.clone()));
        assert!(
            first.minted_a_new_id(),
            "a missing store must mint a fresh id"
        );
        // Owned copy: the first instance is dropped before the second is built.
        let minted = first.id().to_string();
        assert!(
            store.exists(),
            "the minted id must be persisted to {store:?}"
        );

        let reloaded = BackendIdentity::with_store(Some(store.clone()));
        assert_eq!(
            reloaded.id(),
            minted,
            "a restart must keep the same identity"
        );
        assert!(
            !reloaded.minted_a_new_id(),
            "a reloaded id was not minted, so nothing looks new to the app"
        );
    }

    // Criterion: a missing store mints a new id (and persists it) rather than
    // leaving the backend without an identity.
    #[test]
    fn test_missing_store_mints_and_persists_a_new_id() {
        let store = temp_state_dir("missing").join(IDENTITY_STORE_FILE);
        assert!(!store.exists(), "the fixture starts with no store");

        let identity = BackendIdentity::with_store(Some(store.clone()));
        assert!(identity.minted_a_new_id());
        assert!(!identity.id().is_empty());

        let raw = std::fs::read_to_string(&store).expect("the store must have been written");
        assert!(
            raw.contains(identity.id()),
            "the persisted store must carry the minted id, got {raw}"
        );
    }

    // Criterion: a malformed store mints a new id — an unreadable file must not
    // stop the backend from advertising itself.
    #[test]
    fn test_malformed_store_mints_a_new_id() {
        for (case, body) in [
            ("not-json", "{ this is not json"),
            ("wrong-shape", r#"{"nope":1}"#),
            ("blank-id", r#"{"id":""}"#),
            ("empty", ""),
        ] {
            let dir = temp_state_dir(case);
            std::fs::create_dir_all(&dir).expect("create the temp state dir");
            let store = dir.join(IDENTITY_STORE_FILE);
            std::fs::write(&store, body).expect("write the malformed store");

            let identity = BackendIdentity::with_store(Some(store.clone()));
            assert!(
                identity.minted_a_new_id(),
                "{case}: a malformed store must mint a new id"
            );
            assert!(!identity.id().is_empty(), "{case}: the id must be usable");

            let reloaded = BackendIdentity::with_store(Some(store));
            assert_eq!(
                reloaded.id(),
                identity.id(),
                "{case}: the repaired store must reload the same id"
            );
        }
    }

    // Criterion: the id store is **independent of the token store** — wiping
    // `auth.json` re-mints the token but leaves the id unchanged, so re-pairing
    // repairs the known entry instead of creating a second one.
    #[test]
    fn test_identity_survives_a_token_store_wipe() {
        let dir = temp_state_dir("token-wipe");
        let identity_store = dir.join(IDENTITY_STORE_FILE);
        let auth_store = dir.join("auth.json");

        let identity = BackendIdentity::with_store(Some(identity_store.clone()));
        // Owned copy: compared after the store has been rebuilt.
        let id = identity.id().to_string();
        let auth = crate::auth::AuthStore::with_store(Some(auth_store.clone()));
        let token = auth.token().to_string();

        // The operator wipes the token store to unpair every client.
        std::fs::remove_file(&auth_store).expect("the token store must exist to be wiped");

        let re_auth = crate::auth::AuthStore::with_store(Some(auth_store));
        assert_ne!(
            re_auth.token(),
            token,
            "wiping auth.json re-mints the token"
        );

        let reloaded = BackendIdentity::with_store(Some(identity_store));
        assert_eq!(
            reloaded.id(),
            id,
            "the machine's identity must not depend on the token store"
        );
        assert!(!reloaded.minted_a_new_id());
    }

    // Criterion: the id is persisted under `$XDG_STATE_HOME/blue2th/identity.json`
    // — app-scoped, next to the other state files. Asserted on the shape only,
    // since mutating `XDG_STATE_HOME` is process-wide (see `state_store.rs`).
    #[test]
    fn test_identity_store_path_is_app_scoped() {
        if let Some(path) = identity_store_path() {
            assert!(
                path.ends_with("blue2th/identity.json"),
                "the id must live next to the other state files, got {path:?}"
            );
        }
    }
}
