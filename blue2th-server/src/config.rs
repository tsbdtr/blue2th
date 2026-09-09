// SPDX-License-Identifier: MIT OR Apache-2.0

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
    /// Whether the backend dials a remembered-but-disconnected speaker back by
    /// itself (phase 6.5). Persisted next to the name; defaults to on.
    auto_reconnect: bool,
    /// Whether the Spotify Connect level is pinned to 100 (#58). Persisted next
    /// to the name; defaults to off, since pinning is an opt-in.
    spotify_volume_lock: bool,
    /// Where the name is persisted, or `None` to stay in memory only.
    store: Option<std::path::PathBuf>,
}

/// Read the persisted config, or `None` when there is nothing usable to read.
/// The flag rides along, defaulted by the DTO — so a phase 6.2 store, which only
/// carries a name, comes back with restoration on rather than silently off.
fn load_config(path: Option<&std::path::Path>) -> Option<ServerConfig> {
    let raw = std::fs::read_to_string(path?).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Persist the configured name. Failures are reported to the caller, which logs
/// them: losing persistence must never turn a rename into an error.
fn save_config(
    path: &std::path::Path,
    name: &str,
    restore_during_playback: bool,
    auto_reconnect: bool,
    spotify_volume_lock: bool,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string(&ServerConfig {
        // Owned copy: `ServerConfig` is a plain DTO built for serialization.
        name: name.to_string(),
        restore_during_playback,
        auto_reconnect,
        spotify_volume_lock,
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
            restore_during_playback: true,
            auto_reconnect: true,
            spotify_volume_lock: false,
            store: None,
        }
    }

    /// A name backed by a store, reloaded on construction so a restart keeps the
    /// configured name. A missing or malformed store yields the default.
    pub fn with_store(store: Option<std::path::PathBuf>) -> Self {
        let stored = load_config(store.as_deref());
        Self {
            name: stored
                .as_ref()
                .and_then(|c| blue2th_proto::validate_backend_name(&c.name).ok())
                .unwrap_or_else(|| DEFAULT_BACKEND_NAME.to_string()),
            // Absent from the store (phase 6.2 file) or unreadable: on, the default.
            restore_during_playback: stored
                .as_ref()
                .map(|c| c.restore_during_playback)
                .unwrap_or(true),
            // Absent from the store (a pre-6.5 file) or unreadable: on, the default.
            auto_reconnect: stored.as_ref().map(|c| c.auto_reconnect).unwrap_or(true),
            // Absent from the store (a pre-#58 file) or unreadable: off, the
            // default — a guard is never turned on by omission.
            spotify_volume_lock: stored
                .as_ref()
                .map(|c| c.spotify_volume_lock)
                .unwrap_or(false),
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
    pub fn set_restore_during_playback(&mut self, enabled: bool) {
        self.restore_during_playback = enabled;
        self.persist();
    }

    /// Whether the backend may dial a remembered speaker back by itself.
    pub fn auto_reconnect(&self) -> bool {
        self.auto_reconnect
    }

    /// Store and persist the auto-reconnect setting.
    pub fn set_auto_reconnect(&mut self, enabled: bool) {
        self.auto_reconnect = enabled;
        self.persist();
    }

    /// Whether the Spotify Connect level is pinned to 100 (#58).
    pub fn spotify_volume_lock(&self) -> bool {
        self.spotify_volume_lock
    }

    /// Store and persist the Spotify volume lock.
    pub fn set_spotify_volume_lock(&mut self, enabled: bool) {
        self.spotify_volume_lock = enabled;
        self.persist();
    }

    /// Write name and flag together: they share one file, so a partial write
    /// would drop whichever half it left out.
    fn persist(&self) {
        let Some(path) = self.store.as_deref() else {
            return;
        };
        if let Err(e) = save_config(
            path,
            &self.name,
            self.restore_during_playback,
            self.auto_reconnect,
            self.spotify_volume_lock,
        ) {
            tracing::warn!("could not persist the backend config: {e}");
        }
    }

    /// Validate (shared proto rule), trim, store and persist a new name,
    /// returning what was actually stored.
    pub fn set_name(&mut self, raw: &str) -> Result<String, NameError> {
        let name = blue2th_proto::validate_backend_name(raw)?;
        // Owned copy: the validated value is both stored and handed back.
        self.name = name.clone();
        self.persist();
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

    // ---- phase 6.5: the auto-reconnect setting ----

    // Criterion: `ServerName` stores `auto_reconnect`, and it ships **on** — a
    // fresh backend dials a remembered speaker back until the user says otherwise.
    #[test]
    fn test_new_defaults_to_auto_reconnect_enabled() {
        assert!(ServerName::new().auto_reconnect());
        assert!(ServerName::with_store(None).auto_reconnect());
    }

    // Criterion: `POST /config` stores the flag — the setter applies it, in both
    // directions.
    #[test]
    fn test_set_auto_reconnect_applies_the_value() {
        let mut config = ServerName::new();
        config.set_auto_reconnect(false);
        assert!(!config.auto_reconnect());
        config.set_auto_reconnect(true);
        assert!(config.auto_reconnect());
    }

    // Criterion: `ServerName` persists and reloads `auto_reconnect` — it survives
    // the restart the whole feature is about.
    #[test]
    fn test_auto_reconnect_round_trips_through_the_store() {
        let path = store_path("auto-reconnect-roundtrip");
        {
            let mut config = ServerName::with_store(Some(path.clone()));
            config.set_name("Salon").expect("store a valid name");
            config.set_auto_reconnect(false);
        }

        let reloaded = ServerName::with_store(Some(path.clone()));
        assert_eq!(reloaded.name(), "Salon", "the name must still round-trip");
        assert!(
            reloaded.restore_during_playback(),
            "the phase 6.3 flag must keep its own value"
        );
        assert!(
            !reloaded.auto_reconnect(),
            "the flag must be persisted next to the name"
        );

        let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
    }

    // Criterion: the three settings share one file, so writing one may not
    // clobber the others.
    #[test]
    fn test_setting_the_name_keeps_the_stored_auto_reconnect_flag() {
        let path = store_path("auto-reconnect-and-rename");
        {
            let mut config = ServerName::with_store(Some(path.clone()));
            config.set_auto_reconnect(false);
            config.set_restore_during_playback(false);
            config.set_name("Bureau").expect("store a valid name");
        }

        let reloaded = ServerName::with_store(Some(path.clone()));
        assert_eq!(reloaded.name(), "Bureau");
        assert!(!reloaded.restore_during_playback());
        assert!(!reloaded.auto_reconnect());

        let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
    }

    // Criterion: a store written before 6.5 (name + restore flag only) reloads
    // with auto-reconnect **on**, never silently off.
    #[test]
    fn test_a_pre_6_5_store_loads_with_auto_reconnect_enabled() {
        let path = store_path("legacy-6-3");
        std::fs::write(&path, r#"{"name":"Salon","restore_during_playback":false}"#)
            .expect("write a phase 6.3-era store");

        let config = ServerName::with_store(Some(path.clone()));
        assert_eq!(config.name(), "Salon");
        assert!(
            !config.restore_during_playback(),
            "the stored phase 6.3 flag must still be honoured"
        );
        assert!(
            config.auto_reconnect(),
            "a store predating the field must default the flag to on, not to off"
        );

        let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
    }

    // Criterion (non-nominal): a malformed store yields the default rather than
    // failing the startup.
    #[test]
    fn test_malformed_store_yields_the_default_auto_reconnect_flag() {
        let path = store_path("malformed-auto-reconnect");
        for blob in ["", "not json", r#"{"name": "#, "{}"] {
            std::fs::write(&path, blob).expect("write the test store");
            assert!(
                ServerName::with_store(Some(path.clone())).auto_reconnect(),
                "blob {blob:?} must fall back to auto-reconnect on"
            );
        }

        let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
    }

    // ---- #58: the Spotify volume lock ----

    // Criterion: the lock ships **off** — pinning the level to 100 is an opt-in.
    #[test]
    fn test_new_defaults_to_spotify_volume_lock_off() {
        assert!(!ServerName::new().spotify_volume_lock());
        assert!(!ServerName::with_store(None).spotify_volume_lock());
    }

    // Criterion: `POST /config` applies the lock — the setter stores it, in
    // both directions.
    #[test]
    fn test_set_spotify_volume_lock_applies_the_value() {
        let mut config = ServerName::new();
        config.set_spotify_volume_lock(true);
        assert!(config.spotify_volume_lock());
        config.set_spotify_volume_lock(false);
        assert!(!config.spotify_volume_lock());
    }

    // Criterion: the lock is persisted in `name.json` and reloaded on restart.
    #[test]
    fn test_spotify_volume_lock_round_trips_through_the_store() {
        let path = store_path("spotify-volume-lock-roundtrip");
        {
            let mut config = ServerName::with_store(Some(path.clone()));
            config.set_name("Salon").expect("store a valid name");
            config.set_spotify_volume_lock(true);
        }

        let reloaded = ServerName::with_store(Some(path.clone()));
        assert_eq!(reloaded.name(), "Salon", "the name must still round-trip");
        assert!(
            reloaded.spotify_volume_lock(),
            "the lock must be persisted next to the name"
        );
        assert!(
            reloaded.auto_reconnect() && reloaded.restore_during_playback(),
            "the other flags keep their own values"
        );

        let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
    }

    // Criterion: the settings share one file, so renaming or toggling another
    // flag may not clobber the stored lock.
    #[test]
    fn test_setting_the_name_keeps_the_stored_spotify_volume_lock() {
        let path = store_path("spotify-volume-lock-and-rename");
        {
            let mut config = ServerName::with_store(Some(path.clone()));
            config.set_spotify_volume_lock(true);
            config.set_auto_reconnect(false);
            config.set_name("Bureau").expect("store a valid name");
        }

        let reloaded = ServerName::with_store(Some(path.clone()));
        assert_eq!(reloaded.name(), "Bureau");
        assert!(!reloaded.auto_reconnect());
        assert!(reloaded.spotify_volume_lock());

        let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
    }

    // Criterion (non-nominal: old store): a `name.json` written before #58
    // loads with the lock **off**, never silently on.
    #[test]
    fn test_a_pre_58_store_loads_with_the_spotify_volume_lock_off() {
        let path = store_path("legacy-pre-58");
        std::fs::write(&path, r#"{"name":"Salon","auto_reconnect":false}"#)
            .expect("write a pre-#58 store");

        let config = ServerName::with_store(Some(path.clone()));
        assert_eq!(config.name(), "Salon");
        assert!(
            !config.auto_reconnect(),
            "the stored flag is still honoured"
        );
        assert!(
            !config.spotify_volume_lock(),
            "a store predating the field must default the lock to off"
        );

        let _ = std::fs::remove_dir_all(path.parent().expect("store parent"));
    }

    // Criterion (non-nominal): a malformed store yields the default (off)
    // rather than failing the startup.
    #[test]
    fn test_malformed_store_yields_the_default_spotify_volume_lock() {
        let path = store_path("malformed-spotify-volume-lock");
        for blob in ["", "not json", r#"{"name": "#, "{}"] {
            std::fs::write(&path, blob).expect("write the test store");
            assert!(
                !ServerName::with_store(Some(path.clone())).spotify_volume_lock(),
                "blob {blob:?} must fall back to the lock off"
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
