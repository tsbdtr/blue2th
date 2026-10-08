// SPDX-License-Identifier: MIT OR Apache-2.0

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
