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

use blue2th_proto::{NameError, ServerConfig, DEFAULT_BACKEND_NAME};

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
    /// Whether a remembered speaker coming back mid-playback is re-selected
    /// straight away (phase 6.3). Persisted next to the name; defaults to on.
    restore_during_playback: bool,
    /// Where the name is persisted, or `None` to stay in memory only.
    store: Option<std::path::PathBuf>,
}

/// Read the persisted name. A missing, unreadable, malformed — or invalid —
/// blob simply means "never configured": a broken file must never prevent a
/// start, and must never resurrect a name the shared rule would refuse.
fn load_name(path: Option<&std::path::Path>) -> Option<String> {
    let raw = std::fs::read_to_string(path?).ok()?;
    let config: ServerConfig = serde_json::from_str(&raw).ok()?;
    blue2th_proto::validate_backend_name(&config.name).ok()
}

/// Persist the configured name. Failures are reported to the caller, which logs
/// them: losing persistence must never turn a rename into an error.
fn save_name(path: &std::path::Path, name: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string(&ServerConfig {
        // Owned copy: `ServerConfig` is a plain DTO built for serialization.
        name: name.to_string(),
        // STUB (phase 6.3): the flag must be persisted next to the name.
        restore_during_playback: false,
    })
    .map_err(std::io::Error::other)?;
    std::fs::write(path, body)
}

impl ServerName {
    /// The default name, with no store: performs no I/O (used by tests and by
    /// the store-free router constructors).
    pub fn new() -> Self {
        Self {
            name: DEFAULT_BACKEND_NAME.to_string(),
            // STUB (phase 6.3): the setting defaults to on.
            restore_during_playback: false,
            store: None,
        }
    }

    /// A name backed by a store, reloaded on construction so a restart keeps the
    /// configured name. A missing or malformed store yields the default.
    pub fn with_store(store: Option<std::path::PathBuf>) -> Self {
        Self {
            name: load_name(store.as_deref()).unwrap_or_else(|| DEFAULT_BACKEND_NAME.to_string()),
            // STUB (phase 6.3): reload the flag from the store here.
            restore_during_playback: false,
            store,
        }
    }

    /// The current name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether a returning speaker may be restored while playback runs.
    pub fn restore_during_playback(&self) -> bool {
        self.restore_during_playback
    }

    /// Store and persist the restore-during-playback setting.
    pub fn set_restore_during_playback(&mut self, _enabled: bool) {
        // STUB (phase 6.3).
        todo!("phase 6.3: store and persist the restore-during-playback setting")
    }

    /// Validate (shared proto rule), trim, store and persist a new name,
    /// returning what was actually stored.
    pub fn set_name(&mut self, raw: &str) -> Result<String, NameError> {
        let name = blue2th_proto::validate_backend_name(raw)?;
        // Owned copy: the validated value is both stored and handed back.
        self.name = name.clone();
        if let Some(path) = self.store.as_deref() {
            if let Err(e) = save_name(path, &self.name) {
                tracing::warn!("could not persist the backend name: {e}");
            }
        }
        Ok(name)
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

        let too_long: String = "a".repeat(MAX_BACKEND_NAME_LEN + 1);
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

    // ---- phase 6.3: the restore-during-playback setting ----

    // Criterion: the setting defaults to **on** — a fresh server restores a
    // returning speaker even mid-playback until the user says otherwise.
    #[test]
    fn test_new_defaults_to_restoring_during_playback() {
        assert!(ServerName::new().restore_during_playback());
        assert!(ServerName::with_store(None).restore_during_playback());
    }

    // Criterion: `POST /config` stores the flag — the setter applies it.
    #[test]
    fn test_set_restore_during_playback_applies_the_value() {
        let mut config = ServerName::new();
        config.set_restore_during_playback(false);
        assert!(!config.restore_during_playback());
        config.set_restore_during_playback(true);
        assert!(config.restore_during_playback());
    }

    // Criterion: the flag survives the store round-trip — it is persisted with
    // the name and reloaded on restart.
    #[test]
    fn test_restore_flag_round_trips_through_the_store() {
        let path = store_path("restore-roundtrip");
        {
            let mut config = ServerName::with_store(Some(path.clone()));
            config.set_name("Salon").expect("store a valid name");
            config.set_restore_during_playback(false);
        }

        let reloaded = ServerName::with_store(Some(path.clone()));
        assert_eq!(reloaded.name(), "Salon", "the name must still round-trip");
        assert!(
            !reloaded.restore_during_playback(),
            "the flag must be persisted next to the name"
        );

        let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
    }

    // Criterion: renaming after the flag was turned off must not resurrect it —
    // both values share one store, so one write may not clobber the other.
    #[test]
    fn test_setting_the_name_keeps_the_stored_restore_flag() {
        let path = store_path("restore-and-rename");
        {
            let mut config = ServerName::with_store(Some(path.clone()));
            config.set_restore_during_playback(false);
            config.set_name("Bureau").expect("store a valid name");
        }

        let reloaded = ServerName::with_store(Some(path.clone()));
        assert_eq!(reloaded.name(), "Bureau");
        assert!(!reloaded.restore_during_playback());

        let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
    }

    // Criterion (non-nominal): a phase 6.2-era store (name only, no flag) loads
    // without error and carries the default — restoration on.
    #[test]
    fn test_a_phase_6_2_store_loads_with_restoration_enabled() {
        let path = store_path("legacy-6-2");
        std::fs::write(&path, r#"{"name":"Salon"}"#).expect("write a phase 6.2-era store");

        let config = ServerName::with_store(Some(path.clone()));
        assert_eq!(config.name(), "Salon");
        assert!(
            config.restore_during_playback(),
            "a name-only store must default the flag to on, not to off"
        );

        let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
    }

    // Criterion (non-nominal): a malformed store yields the defaults for both
    // values rather than failing the startup.
    #[test]
    fn test_malformed_store_yields_the_default_restore_flag() {
        let path = store_path("malformed-restore");
        for blob in ["", "not json", r#"{"name": "#, "{}"] {
            std::fs::write(&path, blob).expect("write the test store");
            assert!(
                ServerName::with_store(Some(path.clone())).restore_during_playback(),
                "blob {blob:?} must fall back to restoration on"
            );
        }

        let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
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
