// Copyright 2026 Blue2th
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Tests for the runtime backend settings (phase 6.2).
//!
//! Everything here is the **pure** half of the feature: the backend list, the
//! validation, the URL normalisation and the tolerant loading of the persisted
//! blob. The `SharedPreferences` read/write behind `settings::current` /
//! `settings::set_current` is an Android-only JNI seam, validated manually on a
//! device — as is the settings page UI itself.

use blue2th::settings::{self, AppSettings, BackendEntry, SettingsError, NO_BACKEND_LABEL};
use blue2th_proto::{NameError, MAX_BACKEND_NAME_LEN};

/// A settings blob holding two backends with the first one active.
///
/// Built by hand rather than through `add`/`activate`: a fixture must not depend
/// on the very functions under test, and `clippy`'s `allow-expect-in-tests` does
/// not reach a free helper in an integration-test binary.
fn two_backends() -> AppSettings {
    AppSettings {
        backends: vec![
            BackendEntry {
                name: "Salon".to_string(),
                url: "http://192.168.1.107:4000".to_string(),
            },
            BackendEntry {
                name: "Bureau".to_string(),
                url: "http://192.168.1.42:4000".to_string(),
            },
        ],
        active: Some(0),
    }
}

// Criterion: `AppSettings` (backends + active selection) round-trips through serde.
#[test]
fn test_app_settings_round_trips_through_serde() {
    let original = two_backends();
    let json = serde_json::to_string(&original).expect("serialize AppSettings");
    let parsed: AppSettings = serde_json::from_str(&json).expect("deserialize AppSettings");
    assert_eq!(original, parsed);
    assert_eq!(parsed.backends.len(), 2);
    assert_eq!(parsed.active, Some(0));
}

// Criterion: `AppSettings` round-trips through the storage seam's blob too —
// what `save_blob` writes, `load` reads back unchanged.
#[test]
fn test_save_blob_and_load_round_trip_the_settings() {
    let original = two_backends();
    let blob = settings::save_blob(&original);
    assert_eq!(settings::load(Some(&blob)), original);
}

// Criterion: adding a backend rejects a duplicate name — two identical labels in
// the status encart would be indistinguishable.
#[test]
fn test_add_rejects_a_duplicate_name() {
    let mut settings = two_backends();
    assert_eq!(
        settings.add("Salon", "http://10.0.0.9:4000"),
        Err(SettingsError::DuplicateName)
    );
    assert_eq!(
        settings.backends.len(),
        2,
        "a rejected entry must not be stored"
    );
}

// Criterion: adding a backend rejects a malformed URL (no scheme, spaces, empty);
// the entry is not stored.
#[test]
fn test_add_rejects_a_malformed_url_and_stores_nothing() {
    let mut settings = AppSettings::default();
    for url in [
        "192.168.1.107:4000",
        "http://192.168.1.107 :4000",
        "",
        "   ",
    ] {
        assert_eq!(
            settings.add("Salon", url),
            Err(SettingsError::MalformedUrl),
            "{url} must be rejected"
        );
    }
    assert!(
        settings.backends.is_empty(),
        "no rejected entry may be stored, got {:?}",
        settings.backends
    );
}

// Criterion: adding accepts and normalises a trailing slash.
#[test]
fn test_add_normalises_a_trailing_slash() {
    let mut settings = AppSettings::default();
    settings
        .add("Salon", "http://192.168.1.107:4000/")
        .expect("a trailing slash is tolerated");
    assert_eq!(
        settings.backends.first(),
        Some(&BackendEntry {
            name: "Salon".to_string(),
            url: "http://192.168.1.107:4000".to_string(),
        })
    );
}

// Criterion: the name rule lives in one place — `add` applies the shared
// `blue2th-proto` validator rather than its own, so the app and the server can
// never drift on what a legal name is.
#[test]
fn test_add_applies_the_shared_proto_name_rule() {
    for name in [
        "", "   ", "2salon", "-salon", "_salon", "salon tv", "séjour", "salon!",
    ] {
        let mut settings = AppSettings::default();
        let result = settings.add(name, "http://192.168.1.107:4000");
        assert!(
            matches!(result, Err(SettingsError::Name(_))),
            "{name} must be rejected by the shared name rule, got {result:?}"
        );
        assert!(settings.backends.is_empty());
    }

    for name in ["Salon", "blue2th-PC", "salon_tv", "pc2"] {
        let mut settings = AppSettings::default();
        let outcome = settings.add(name, "http://192.168.1.107:4000");
        assert!(outcome.is_ok(), "{name} must be accepted, got {outcome:?}");
    }
}

// Criterion: the name is capped at `MAX_BACKEND_NAME_LEN`, and the rejection
// carries the reason so the UI can name the rule.
#[test]
fn test_add_rejects_a_name_longer_than_the_cap() {
    let too_long: String = "a".repeat(MAX_BACKEND_NAME_LEN + 1);
    let mut settings = AppSettings::default();
    assert_eq!(
        settings.add(&too_long, "http://192.168.1.107:4000"),
        Err(SettingsError::Name(NameError::TooLong))
    );
}

// Criterion: `normalise_url` accepts a well-formed address unchanged.
#[test]
fn test_normalise_url_accepts_a_scheme_host_and_port() {
    assert_eq!(
        settings::normalise_url("http://192.168.1.107:4000"),
        Ok("http://192.168.1.107:4000".to_string())
    );
}

// Criterion: `normalise_url` trims a trailing slash (tolerated, not rejected).
#[test]
fn test_normalise_url_trims_a_trailing_slash() {
    assert_eq!(
        settings::normalise_url("http://192.168.1.107:4000/"),
        Ok("http://192.168.1.107:4000".to_string())
    );
    assert_eq!(
        settings::normalise_url("  http://192.168.1.107:4000/  "),
        Ok("http://192.168.1.107:4000".to_string())
    );
}

// Criterion: `normalise_url` rejects a missing scheme, a blank value and
// embedded spaces.
#[test]
fn test_normalise_url_rejects_malformed_addresses() {
    for raw in [
        "192.168.1.107:4000",
        "",
        "   ",
        "http://192.168.1.107 :4000",
        "http://",
    ] {
        assert_eq!(
            settings::normalise_url(raw),
            Err(SettingsError::MalformedUrl),
            "{raw:?} must be rejected"
        );
    }
}

// Criterion: exactly one backend is active at a time; activating another
// switches it.
#[test]
fn test_activate_keeps_exactly_one_backend_active() {
    let mut settings = two_backends();
    assert_eq!(settings.active, Some(0));
    assert_eq!(
        settings.active_backend().map(|b| b.name.as_str()),
        Some("Salon")
    );

    settings.activate(1).expect("switch to Bureau");
    assert_eq!(settings.active, Some(1));
    assert_eq!(
        settings.active_backend().map(|b| b.name.as_str()),
        Some("Bureau")
    );
}

// Criterion: activating an unknown index is refused rather than leaving a
// dangling active selection.
#[test]
fn test_activate_unknown_index_is_refused() {
    let mut settings = two_backends();
    assert_eq!(settings.activate(7), Err(SettingsError::UnknownBackend));
    assert_eq!(settings.active, Some(0));
}

// Criterion: deleting the active backend leaves no active backend and does not
// panic — the app falls back to "nothing configured".
#[test]
fn test_remove_active_backend_leaves_no_active_backend() {
    let mut settings = two_backends();
    settings.remove(0).expect("remove the active backend");

    assert_eq!(settings.backends.len(), 1);
    assert_eq!(settings.active, None);
    assert!(settings.active_backend().is_none());
    assert!(settings.active_url().is_none());
}

// Criterion: removing a non-active backend keeps the active one active, even
// when the removal shifts the indices.
#[test]
fn test_remove_non_active_backend_keeps_the_active_one() {
    let mut settings = two_backends();
    settings.activate(1).expect("activate Bureau");
    settings.remove(0).expect("remove Salon");

    assert_eq!(
        settings.active_backend().map(|b| b.name.as_str()),
        Some("Bureau"),
        "the active backend must survive an index shift"
    );
}

// Criterion: `active_backend_url()` returns the active entry's URL.
#[test]
fn test_active_url_returns_the_active_entry_url() {
    let settings = two_backends();
    assert_eq!(
        settings.active_url(),
        Some("http://192.168.1.107:4000".to_string())
    );
}

// Criterion: `active_backend_url()` is `None` when no backend is active — there
// is no fallback address of any kind (not even localhost).
#[test]
fn test_active_url_is_none_without_a_configured_backend() {
    let empty = AppSettings::default();
    assert!(empty.active_backend().is_none());
    assert_eq!(empty.active_url(), None);

    // A configured but inactive backend is still no address.
    let mut inactive = AppSettings::default();
    inactive
        .add("Salon", "http://192.168.1.107:4000")
        .expect("add Salon");
    assert_eq!(inactive.active_url(), None);
}

// Criterion: the status encart shows the active backend's name instead of the
// static label, and `-` when no backend is active.
#[test]
fn test_active_label_is_the_backend_name_or_a_dash() {
    assert_eq!(two_backends().active_label(), "Salon");
    assert_eq!(AppSettings::default().active_label(), NO_BACKEND_LABEL);
    assert_eq!(AppSettings::default().active_label(), "-");
}

// Criterion: loading settings from a missing/malformed blob yields an empty list,
// no error — a broken preferences entry must never prevent the app from starting.
#[test]
fn test_load_missing_or_malformed_blob_yields_empty_settings() {
    let empty = AppSettings::default();
    assert_eq!(settings::load(None), empty, "missing blob");
    assert_eq!(settings::load(Some("")), empty, "empty blob");
    assert_eq!(settings::load(Some("   ")), empty, "blank blob");
    assert_eq!(settings::load(Some("not json")), empty, "non-JSON blob");
    assert_eq!(
        settings::load(Some(r#"{"backends": ["#)),
        empty,
        "truncated blob"
    );
    assert_eq!(
        settings::load(Some(r#"{"backends": 42, "active": "yes"}"#)),
        empty,
        "wrongly typed blob"
    );
}

// Criterion: a well-formed blob is loaded as written (the restart path).
#[test]
fn test_load_restores_the_backends_and_the_active_choice() {
    let blob = r#"{"backends":[{"name":"Salon","url":"http://192.168.1.107:4000"}],"active":0}"#;
    let loaded = settings::load(Some(blob));
    assert_eq!(loaded.backends.len(), 1);
    assert_eq!(loaded.active_label(), "Salon");
    assert_eq!(
        loaded.active_url(),
        Some("http://192.168.1.107:4000".to_string())
    );
}

// Criterion: a blob whose `active` index points outside the list is repaired to
// "nothing active" rather than trusted — otherwise every call would resolve to a
// backend that is not there.
#[test]
fn test_load_repairs_an_out_of_range_active_index() {
    let blob = r#"{"backends":[{"name":"Salon","url":"http://192.168.1.107:4000"}],"active":9}"#;
    let loaded = settings::load(Some(blob));
    assert_eq!(loaded.backends.len(), 1);
    assert_eq!(loaded.active, None);
    assert_eq!(loaded.active_label(), NO_BACKEND_LABEL);
}

// Criterion: `BLUE2TH_BACKEND_URL` no longer appears anywhere in the codebase —
// the address is a runtime setting, with no compile-time value and no seeded
// default. The needle is assembled at compile time so this test is not itself an
// occurrence.
#[test]
fn test_compile_time_backend_url_env_var_is_gone_from_the_codebase() {
    let needle = concat!("BLUE2TH_", "BACKEND_URL");
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut offenders = Vec::new();

    for dir in ["src", "tests", "blue2th-server/src", "blue2th-proto/src"] {
        let dir = root.join(dir);
        // Mapped to a String so the failure names the directory without a
        // `panic!`, which clippy forbids even in tests here.
        let entries = std::fs::read_dir(&dir)
            .map_err(|e| format!("read {dir:?}: {e}"))
            .expect("the scanned source directories must be readable");
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let source = std::fs::read_to_string(&path).unwrap_or_default();
            // Skip this very file: it names the variable to assert its absence.
            if path.file_name().and_then(|n| n.to_str()) == Some("settings.rs")
                && path.starts_with(root.join("tests"))
            {
                continue;
            }
            if source.contains(needle) {
                offenders.push(path);
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "{needle} must not appear in the codebase, found in {offenders:?}"
    );
}
