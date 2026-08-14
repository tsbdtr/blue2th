//! The backend's own name (phase 6.2).
//!
//! The app is the source of truth for the name and pushes it over `POST /config`;
//! the server persists it so a restart keeps advertising the same Spotify Connect
//! device — and so the Web API device lookup keeps matching the running
//! `librespot` even before the app talks to it again.
//!
//! `POST /config` is unauthenticated on the LAN, so the server re-validates with
//! the *shared* `blue2th_proto::validate_backend_name` rule rather than trusting
//! the client. Following the `state_store` pattern, `new()` is disk-free so tests
//! never read or write the real `~/.local/state/blue2th/`.

use blue2th_proto::NameError;

/// File holding the configured name under the app-scoped state directory.
const NAME_STORE_FILE: &str = "name.json";

/// Path of the name store, or `None` when no state home can be resolved (the
/// name then simply stays in memory).
pub fn name_store_path() -> Option<std::path::PathBuf> {
    crate::state_store::state_store_path(NAME_STORE_FILE)
}

/// The name this backend answers to, optionally persisted.
pub struct ServerName {
    /// The current name; the default until the app configures another one.
    name: String,
    /// Where the name is persisted, or `None` to stay in memory only.
    store: Option<std::path::PathBuf>,
}

impl ServerName {
    /// The default name, with no store: performs no I/O (used by tests and by
    /// the store-free router constructors).
    pub fn new() -> Self {
        todo!("phase 6.2: a disk-free ServerName holding the default name")
    }

    /// A name backed by a store, reloaded on construction so a restart keeps the
    /// configured name. A missing or malformed store yields the default.
    pub fn with_store(_store: Option<std::path::PathBuf>) -> Self {
        todo!("phase 6.2: load the persisted name, falling back to the default")
    }

    /// The current name.
    pub fn name(&self) -> &str {
        todo!("phase 6.2: expose the configured name")
    }

    /// Validate (shared proto rule), trim, store and persist a new name,
    /// returning what was actually stored.
    pub fn set_name(&mut self, _raw: &str) -> Result<String, NameError> {
        todo!("phase 6.2: re-validate, store and persist the pushed name")
    }
}

impl Default for ServerName {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use blue2th_proto::{DEFAULT_BACKEND_NAME, MAX_BACKEND_NAME_LEN};

    use super::*;

    /// A private, per-test store path under the system temp dir. Never the real
    /// user state directory (mirrors `targets::tests::store_path`).
    fn store_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("blue2th-name-test-{name}"));
        std::fs::create_dir_all(&dir).expect("create the test store dir");
        dir.join("name.json")
    }

    // Criterion: a fresh server returns the default `blue2th-PC`, and `new()`
    // holds no store (test isolation: it must never touch the disk).
    #[test]
    fn test_new_holds_the_default_name_and_no_store() {
        let config = ServerName::new();
        assert_eq!(config.name(), DEFAULT_BACKEND_NAME);
        assert!(config.store.is_none(), "new() must stay off-disk");
    }

    // Criterion: the default name satisfies its own validator — the server never
    // holds a name its own rule would refuse.
    #[test]
    fn test_default_name_satisfies_the_shared_validator() {
        assert_eq!(
            blue2th_proto::validate_backend_name(ServerName::new().name()),
            Ok(DEFAULT_BACKEND_NAME.to_string())
        );
    }

    // Criterion: `POST /config` rejects an invalid name — the server re-validates
    // rather than trusting the client, since the route is unauthenticated on the LAN.
    #[test]
    fn test_set_name_rejects_a_blank_name() {
        let mut config = ServerName::new();
        assert_eq!(config.set_name(""), Err(NameError::Empty));
        assert_eq!(config.set_name("   "), Err(NameError::Empty));
        assert_eq!(
            config.name(),
            DEFAULT_BACKEND_NAME,
            "a rejected name must not replace the current one"
        );
    }

    // Criterion: the server applies the *shared* rule, not a looser one of its own
    // — the value ends up as a `librespot --name` argv entry.
    #[test]
    fn test_set_name_rejects_names_the_shared_rule_refuses() {
        let mut config = ServerName::new();
        assert_eq!(config.set_name("2salon"), Err(NameError::BadStart));
        assert_eq!(config.set_name("salon tv"), Err(NameError::BadChar));
        assert_eq!(config.set_name("séjour"), Err(NameError::BadChar));

        let too_long: String = std::iter::repeat('a')
            .take(MAX_BACKEND_NAME_LEN + 1)
            .collect();
        assert_eq!(config.set_name(&too_long), Err(NameError::TooLong));
        assert_eq!(config.name(), DEFAULT_BACKEND_NAME);
    }

    // Criterion: `POST /config` stores a trimmed, valid name.
    #[test]
    fn test_set_name_trims_and_stores_a_valid_name() {
        let mut config = ServerName::new();
        assert_eq!(config.set_name("  Salon \n"), Ok("Salon".to_string()));
        assert_eq!(config.name(), "Salon");
    }

    // Criterion: the configured name is persisted server-side and reloaded on
    // restart (a new `ServerName` over the same store sees it).
    #[test]
    fn test_name_round_trips_through_the_store() {
        let path = store_path("roundtrip");
        {
            let mut config = ServerName::with_store(Some(path.clone()));
            config.set_name("Salon").expect("store a valid name");
        }

        let reloaded = ServerName::with_store(Some(path.clone()));
        assert_eq!(reloaded.name(), "Salon");

        let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
    }

    // Criterion: a missing store yields the default name, no error.
    #[test]
    fn test_missing_store_yields_the_default_name() {
        let path = store_path("missing");
        let _ = std::fs::remove_file(&path);

        let config = ServerName::with_store(Some(path.clone()));
        assert_eq!(config.name(), DEFAULT_BACKEND_NAME);

        let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
    }

    // Criterion: a malformed store yields the default name rather than failing —
    // a broken blob must never prevent the backend from starting, and must never
    // resurrect a name the shared rule would refuse.
    #[test]
    fn test_malformed_store_yields_the_default_name() {
        let path = store_path("malformed");
        for blob in ["", "not json", r#"{"name": "#, "{}", r#"{"name":"2salon"}"#] {
            std::fs::write(&path, blob).expect("write the test store");
            assert_eq!(
                ServerName::with_store(Some(path.clone())).name(),
                DEFAULT_BACKEND_NAME,
                "blob {blob:?} must fall back to the default"
            );
        }

        let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
    }

    // Criterion: `with_store(None)` holds no store and reads nothing — the
    // store-free constructor used by the test router.
    #[test]
    fn test_with_store_none_holds_no_store() {
        let config = ServerName::with_store(None);
        assert!(config.store.is_none());
        assert_eq!(config.name(), DEFAULT_BACKEND_NAME);
    }

    // Criterion: the store is app-scoped, next to the other blue2th state files.
    #[test]
    fn test_name_store_path_is_app_scoped() {
        if let Some(path) = name_store_path() {
            assert!(
                path.ends_with("blue2th/name.json"),
                "the name store must be app-scoped, got {path:?}"
            );
        }
    }
}
