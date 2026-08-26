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

use blue2th_frontend::settings::{
    self, AppSettings, BackendEntry, PairingMethod, SettingsError, NO_BACKEND_LABEL,
};
use blue2th_proto::{NameError, PairLink, MAX_BACKEND_NAME_LEN};

// The settings page reads its labels from `locales/`, and a duplicated top-level
// key there silently drops a whole block, so the keys are checked here.
rust_i18n::i18n!("locales", fallback = "en");

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
                restore_during_playback: true,
                auto_reconnect: true,
                // Phase 6.4: paired, with the typed-code transport.
                token: Some("salon-token".to_string()),
                pairing: PairingMethod::Code,
                // Phase 6.6: Salon knows which machine answers it, Bureau does
                // not (a pre-6.6 entry, matched on its URL).
                id: Some("salon-backend-id".to_string()),
            },
            BackendEntry {
                name: "Bureau".to_string(),
                url: "http://192.168.1.42:4000".to_string(),
                restore_during_playback: false,
                auto_reconnect: false,
                token: None,
                pairing: PairingMethod::Qr,
                id: None,
            },
        ],
        active: Some(0),
        // Phase 6.6: both discovery settings ship enabled.
        auto_repair_url: true,
        discovery_adds_backends: true,
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
            // Phase 6.3: a new backend starts with restoration on.
            restore_during_playback: true,
            // Phase 6.5: and dials a remembered speaker back by itself.
            auto_reconnect: true,
            // Phase 6.4: and unpaired, with the default pairing method.
            token: None,
            pairing: PairingMethod::Code,
            // Phase 6.6: typing an address says nothing about which machine
            // answers it, so no id is adopted yet.
            id: None,
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

// Criterion: an IPv6 literal is still accepted — the host check must read the
// hostname, not merely everything before the first colon.
#[test]
fn test_normalise_url_accepts_an_ipv6_literal() {
    assert_eq!(
        settings::normalise_url("http://[::1]:4000"),
        Ok("http://[::1]:4000".to_string())
    );
}

// Criterion: `normalise_url` rejects a missing scheme, a blank value, embedded
// spaces, and an address whose host is missing — `http://:4000` reaches reqwest
// as "URL scheme is not allowed", a message that names nothing the user typed.
#[test]
fn test_normalise_url_rejects_malformed_addresses() {
    for raw in [
        "192.168.1.107:4000",
        "",
        "   ",
        "http://192.168.1.107 :4000",
        "http://",
        "http://:4000",
        "http://:4000/",
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

// Criterion: deleting an entry that is no longer there is refused rather than
// panicking on an out-of-range index — the row the user tapped may already be
// gone (a second tap, a stale render).
#[test]
fn test_remove_unknown_index_is_refused() {
    let mut settings = two_backends();
    assert_eq!(settings.remove(7), Err(SettingsError::UnknownBackend));
    assert_eq!(settings.backends.len(), 2);
    assert_eq!(settings.active, Some(0));

    let mut empty = AppSettings::default();
    assert_eq!(empty.remove(0), Err(SettingsError::UnknownBackend));
    assert_eq!(empty.active, None);
}

// Criterion: a name freed by a deletion can be used again — the duplicate check
// looks at the current list, not at a history of names.
#[test]
fn test_add_accepts_a_name_freed_by_a_deletion() {
    let mut settings = two_backends();
    settings.remove(0).expect("remove Salon");
    settings
        .add("Salon", "http://10.0.0.9:4000")
        .expect("the freed name must be available again");
    assert_eq!(settings.backends.len(), 2);
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

// ---- phase 6.3: the restore-during-playback toggle ----

// Criterion: the settings page's toggle is remembered per backend and survives
// serde, in both states.
#[test]
fn test_backend_entry_round_trips_with_the_restore_flag() {
    let original = two_backends();
    let json = serde_json::to_string(&original).expect("serialize AppSettings");
    let parsed: AppSettings = serde_json::from_str(&json).expect("deserialize AppSettings");
    assert_eq!(original, parsed);
    assert_eq!(
        parsed
            .backends
            .iter()
            .map(|b| b.restore_during_playback)
            .collect::<Vec<bool>>(),
        vec![true, false],
        "each backend keeps its own setting"
    );
}

// Criterion: the setting defaults to **on** — a freshly added backend restores
// a returning speaker until the user says otherwise.
#[test]
fn test_add_starts_with_restoration_enabled() {
    let mut settings = AppSettings::default();
    settings
        .add("Salon", "http://192.168.1.107:4000")
        .expect("add Salon");
    assert!(
        settings
            .backends
            .first()
            .is_some_and(|b| b.restore_during_playback),
        "a new backend must start with restoration on"
    );
}

// Criterion (non-nominal): a phase 6.2-era persisted blob (no flag) must load
// without error, with restoration on rather than silently off.
#[test]
fn test_a_phase_6_2_blob_loads_with_restoration_enabled() {
    let blob = r#"{"backends":[{"name":"Salon","url":"http://192.168.1.107:4000"}],"active":0}"#;
    let loaded = settings::load(Some(blob));
    assert_eq!(loaded.backends.len(), 1);
    assert_eq!(loaded.active, Some(0));
    assert!(
        loaded
            .active_backend()
            .is_some_and(|b| b.restore_during_playback),
        "an entry written before the flag existed must default to on"
    );
}

// Criterion: the toggle is applied to the active backend and nothing else.
#[test]
fn test_set_restore_during_playback_updates_only_that_backend() {
    let mut settings = two_backends();
    settings
        .set_restore_during_playback(0, false)
        .expect("toggle the active backend off");
    assert_eq!(
        settings.backends.first().map(|b| b.restore_during_playback),
        Some(false)
    );
    assert_eq!(
        settings.backends.get(1).map(|b| b.restore_during_playback),
        Some(false),
        "the other backend must be left exactly as it was"
    );

    settings
        .set_restore_during_playback(0, true)
        .expect("toggle it back on");
    assert_eq!(
        settings.backends.first().map(|b| b.restore_during_playback),
        Some(true)
    );
}

// Criterion (non-nominal): a stale index is refused rather than panicking, like
// every other index-taking settings operation.
#[test]
fn test_set_restore_during_playback_on_an_unknown_backend_is_refused() {
    let mut settings = two_backends();
    assert_eq!(
        settings.set_restore_during_playback(9, false),
        Err(SettingsError::UnknownBackend)
    );
    assert_eq!(settings, two_backends(), "nothing may have changed");
}

// Criterion: the toggle survives the persistence round-trip, so the app still
// knows what to push after a restart.
#[test]
fn test_restore_flag_survives_the_settings_blob_round_trip() {
    let mut settings = two_backends();
    settings
        .set_restore_during_playback(0, false)
        .expect("toggle Salon off");
    let reloaded = settings::load(Some(&settings::save_blob(&settings)));
    assert_eq!(
        reloaded.backends.first().map(|b| b.restore_during_playback),
        Some(false)
    );
}

// ---- phase 6.5: auto-reconnect the remembered speakers ----

// Criterion: the setting defaults to **on** — a freshly added backend dials a
// remembered speaker back until the user says otherwise.
#[test]
fn test_add_starts_with_auto_reconnect_enabled() {
    let mut settings = AppSettings::default();
    settings
        .add("Salon", "http://192.168.1.107:4000")
        .expect("add Salon");
    assert!(
        settings.backends.first().is_some_and(|b| b.auto_reconnect),
        "a new backend must start with auto-reconnect on"
    );
}

// Criterion (non-nominal): a blob written before the flag existed must load with
// auto-reconnect **on** rather than silently off — a bare `serde(default)` would
// yield `false` and disable the feature for every existing install.
#[test]
fn test_a_pre_6_5_blob_loads_with_auto_reconnect_enabled() {
    let blob = r#"{
        "backends": [
            {
                "name": "Salon",
                "url": "http://192.168.1.107:4000",
                "restore_during_playback": true,
                "token": "salon-token",
                "pairing": "code"
            }
        ],
        "active": 0
    }"#;
    let loaded = settings::load(Some(blob));

    let salon = loaded.active_backend().expect("the stored backend");
    assert!(
        salon.auto_reconnect,
        "an entry written before the flag existed must default to on"
    );
    assert_eq!(
        salon.token.as_deref(),
        Some("salon-token"),
        "loading a pre-6.5 blob must not unpair the backend"
    );
    assert!(
        salon.restore_during_playback,
        "the phase 6.3 flag must survive alongside the new one"
    );
}

// Criterion: the toggle is applied to the named backend and nothing else.
#[test]
fn test_set_auto_reconnect_updates_only_that_backend() {
    let mut settings = two_backends();
    settings
        .set_auto_reconnect(0, false)
        .expect("toggle Salon off");
    assert_eq!(
        settings.backends.first().map(|b| b.auto_reconnect),
        Some(false)
    );
    assert_eq!(
        settings.backends.get(1).map(|b| b.auto_reconnect),
        Some(false),
        "the other backend must be left exactly as it was"
    );

    settings
        .set_auto_reconnect(0, true)
        .expect("toggle it back on");
    assert_eq!(
        settings.backends.first().map(|b| b.auto_reconnect),
        Some(true)
    );
    assert_eq!(
        settings.backends.first().map(|b| b.restore_during_playback),
        Some(true),
        "the phase 6.3 toggle is a separate setting and must not move"
    );
}

// Criterion (non-nominal): a stale index is refused rather than panicking, like
// every other index-taking settings operation.
#[test]
fn test_set_auto_reconnect_on_an_unknown_backend_is_refused() {
    let mut settings = two_backends();
    assert_eq!(
        settings.set_auto_reconnect(9, false),
        Err(SettingsError::UnknownBackend)
    );
    assert_eq!(settings, two_backends(), "nothing may have changed");
}

// Criterion: the toggle survives the persistence round-trip, so the app still
// knows what to push after a restart.
#[test]
fn test_auto_reconnect_survives_the_settings_blob_round_trip() {
    let mut settings = two_backends();
    settings
        .set_auto_reconnect(0, false)
        .expect("toggle Salon off");
    let reloaded = settings::load(Some(&settings::save_blob(&settings)));
    assert_eq!(
        reloaded.backends.first().map(|b| b.auto_reconnect),
        Some(false)
    );
}

// ---- phase 6.6: find the backend on the network ----

// Criterion: applying a `Repair` updates the URL in place and never creates a
// second entry; the token survives. This is the DHCP-lease-change case that used
// to duplicate the backend.
#[test]
fn test_applying_a_repair_updates_the_url_in_place() {
    let mut settings = two_backends();
    settings
        .set_url(0, "http://192.168.1.200:4000")
        .expect("a known backend's address can always be repaired");

    assert_eq!(settings.backends.len(), 2, "no second entry may be created");
    let salon = settings.backends.first().expect("Salon is still there");
    assert_eq!(salon.url, "http://192.168.1.200:4000");
    assert_eq!(
        salon.token.as_deref(),
        Some("salon-token"),
        "repairing an address must not unpair the backend"
    );
    assert_eq!(salon.name, "Salon", "the locally chosen name survives");
    assert_eq!(salon.pairing, PairingMethod::Code);
    assert!(salon.restore_during_playback);
    assert_eq!(settings.active, Some(0), "the active entry does not move");
}

// Criterion: `set_url` normalises through `normalise_url` — a trailing slash is
// trimmed exactly as `add` trims it.
#[test]
fn test_set_url_normalises_the_address() {
    let mut settings = two_backends();
    settings
        .set_url(1, "  http://192.168.1.55:4000/  ")
        .expect("a trailing slash and spaces around it are tolerated");
    assert_eq!(
        settings.backends.get(1).map(|b| b.url.as_str()),
        Some("http://192.168.1.55:4000")
    );
}

// Criterion: `set_url` refuses a malformed address, and refuses an unknown
// index — a rejected repair must leave the whole list untouched.
#[test]
fn test_set_url_refuses_a_malformed_address_or_an_unknown_index() {
    let mut settings = two_backends();
    for bad in [
        "",
        "   ",
        "192.168.1.107:4000",
        "http://",
        "http://:4000",
        "http://a b",
    ] {
        assert_eq!(
            settings.set_url(0, bad),
            Err(SettingsError::MalformedUrl),
            "{bad} is not a usable backend address"
        );
    }
    assert_eq!(
        settings.set_url(9, "http://192.168.1.200:4000"),
        Err(SettingsError::UnknownBackend)
    );
    assert_eq!(settings, two_backends(), "nothing may have changed");
}

// Criterion: a repaired address survives the persistence round-trip — the point
// of the repair is that the next start still reaches the backend.
#[test]
fn test_a_repaired_url_survives_the_settings_blob_round_trip() {
    let mut settings = two_backends();
    settings
        .set_url(0, "http://192.168.1.200:4000")
        .expect("repair Salon");
    let reloaded = settings::load(Some(&settings::save_blob(&settings)));
    assert_eq!(reloaded, settings);
    assert_eq!(
        reloaded.backends.first().map(|b| b.token.as_deref()),
        Some(Some("salon-token"))
    );
}

// Criterion: `BackendEntry` gains `id: Option<String>` with `serde(default)` — a
// phase 6.4 blob loads with `id: None` and its token intact, and both new app
// settings load **enabled**.
#[test]
fn test_a_phase_6_4_blob_loads_with_no_id_and_discovery_enabled() {
    let blob = r#"{
        "backends": [
            {
                "name": "Salon",
                "url": "http://192.168.1.107:4000",
                "restore_during_playback": true,
                "token": "salon-token",
                "pairing": "code"
            }
        ],
        "active": 0
    }"#;
    let loaded = settings::load(Some(blob));

    let salon = loaded.backends.first().expect("the entry must survive");
    assert_eq!(salon.id, None, "a pre-6.6 entry simply has no id yet");
    assert_eq!(
        salon.token.as_deref(),
        Some("salon-token"),
        "loading a pre-6.6 blob must not unpair the backend"
    );
    assert!(
        loaded.auto_repair_url,
        "an existing install must not be silently opted out of auto-repair"
    );
    assert!(
        loaded.discovery_adds_backends,
        "an existing install must not be silently opted out of adding backends"
    );
}

// Criterion: both settings default to **on** — a bare `serde(default)` would
// yield `false` and opt every install out without saying so.
#[test]
fn test_both_discovery_settings_default_to_on() {
    let fresh = AppSettings::default();
    assert!(fresh.auto_repair_url);
    assert!(fresh.discovery_adds_backends);

    // An empty/absent blob takes the same path.
    let loaded = settings::load(None);
    assert!(loaded.auto_repair_url);
    assert!(loaded.discovery_adds_backends);
}

// Criterion: both settings round-trip through the persisted blob, so a user who
// turned one off finds it off after a restart.
#[test]
fn test_both_discovery_settings_round_trip_through_the_blob() {
    let mut settings = two_backends();
    settings.set_auto_repair_url(false);
    settings.set_discovery_adds_backends(false);

    let reloaded = settings::load(Some(&settings::save_blob(&settings)));
    assert!(!reloaded.auto_repair_url);
    assert!(!reloaded.discovery_adds_backends);

    let mut back_on = reloaded;
    back_on.set_auto_repair_url(true);
    back_on.set_discovery_adds_backends(true);
    let reloaded = settings::load(Some(&settings::save_blob(&back_on)));
    assert!(reloaded.auto_repair_url);
    assert!(reloaded.discovery_adds_backends);
}

// Criterion: the id is adopted the first time a backend is discovered, **without
// clearing the token** — a pre-6.6 entry becomes id-matched without re-pairing.
#[test]
fn test_adopting_an_id_keeps_the_token_and_the_url() {
    let mut settings = two_backends();
    settings
        .set_backend_id(1, Some("bureau-backend-id".to_string()))
        .expect("Bureau adopts the id it just advertised");

    let bureau = settings.backends.get(1).expect("Bureau is still there");
    assert_eq!(bureau.id.as_deref(), Some("bureau-backend-id"));
    assert_eq!(bureau.url, "http://192.168.1.42:4000");
    assert_eq!(bureau.name, "Bureau");
    assert_eq!(settings.backends.len(), 2);

    // Adopting an id on a paired backend must never unpair it.
    settings
        .set_backend_id(0, Some("salon-backend-id".to_string()))
        .expect("Salon re-confirms its id");
    assert_eq!(
        settings.backends.first().and_then(|b| b.token.as_deref()),
        Some("salon-token")
    );

    assert_eq!(
        settings.set_backend_id(9, Some("nope".to_string())),
        Err(SettingsError::UnknownBackend)
    );
}

// Criterion: the app-wide settings page has every label it renders. `rust-i18n`
// resolves a missing key to the key itself, so an absent translation shows as raw
// `app_settings.foo` on screen rather than failing anywhere.
//
// The `app_settings.` prefix is not cosmetic: a second top-level `settings:`
// mapping in a locale file drops the first one wholesale, which is how this page
// once silently un-translated the per-device one in 6.2. That per-device page is
// gone with the on-phone Bluetooth stack, but the prefix stays — the trap comes
// back the day a second page is added under a bare `settings:`.
#[test]
fn test_locales_carry_the_settings_page_labels() {
    for locale in ["en", "fr"] {
        rust_i18n::set_locale(locale);
        for key in [
            // The second section carrying the phase 6.3 toggle. It lives under
            // the *same* `app_settings:` namespace on purpose — see above.
            "app_settings.section_playback",
            "app_settings.restore_during_playback",
            "app_settings.restore_during_playback_hint",
            // The phase 6.5 toggle, in the same section: the backend dialling a
            // remembered speaker back on its own.
            "app_settings.auto_reconnect",
            "app_settings.auto_reconnect_hint",
            // The pairing section (phase 6.4): the per-backend method, the code
            // exchange and what a 401 reads as.
            "app_settings.section_pairing",
            "app_settings.pairing_method",
            "app_settings.pairing_method_code",
            "app_settings.pairing_method_qr",
            "app_settings.pair",
            "app_settings.pairing_code_placeholder",
            "app_settings.paired_ok",
            "app_settings.not_paired",
            // The discovery section (phase 6.6): the two toggles, the search
            // action and every state it can report — including the ones that are
            // deliberately *not* errors (nothing found, already up to date).
            "app_settings.section_discovery",
            "app_settings.search_network",
            "app_settings.searching",
            "app_settings.no_backend_found",
            "app_settings.discovery_unsupported",
            "app_settings.auto_repair_url",
            "app_settings.auto_repair_url_hint",
            "app_settings.discovery_adds_backends",
            "app_settings.discovery_adds_backends_hint",
            "app_settings.discovered_known",
            "app_settings.discovered_up_to_date",
            "app_settings.discovered_new",
            "app_settings.confirm_repair",
            "app_settings.address_repaired",
            // The app-wide settings page (phase 6.2).
            "app_settings.title",
            "app_settings.backends",
            "app_settings.no_backend_yet",
            "app_settings.active",
            "app_settings.activate",
            "app_settings.delete",
            "app_settings.add",
            "app_settings.test",
            "app_settings.test_ok",
            "app_settings.name_placeholder",
            "app_settings.url_placeholder",
        ] {
            let translated = rust_i18n::t!(key);
            assert_ne!(
                translated, key,
                "{key} must be translated in {locale}, got the raw key back"
            );
        }
    }
}

// Criterion: `BLUE2TH_BACKEND_URL` no longer appears anywhere in the codebase —
// the address is a runtime setting, with no compile-time value and no seeded
// default. The needle is assembled at compile time so this test is not itself an
// occurrence.
#[test]
fn test_compile_time_backend_url_env_var_is_gone_from_the_codebase() {
    let needle = concat!("BLUE2TH_", "BACKEND_URL");
    // Anchored on the workspace root, one level above this crate, because the
    // scan spans all three crates. CARGO_MANIFEST_DIR alone would resolve the
    // sibling crates under blue2th-frontend/ and fail on a missing directory.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("blue2th-frontend sits one level below the workspace root");
    let mut offenders = Vec::new();

    for dir in [
        "blue2th-frontend/src",
        "blue2th-frontend/tests",
        "blue2th-server/src",
        "blue2th-proto/src",
    ] {
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
                && path.starts_with(root.join("blue2th-frontend/tests"))
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

// ---- phase 6.4: per-backend token and pairing method ----

/// A pair link as the phone's camera app would deliver it.
fn pair_link(url: &str, name: Option<&str>) -> PairLink {
    PairLink {
        url: url.to_string(),
        name: name.map(str::to_string),
        code: "K7M2QX".to_string(),
    }
}

// Criterion: `BackendEntry` gains `token` and a `pairing` method, both persisted
// — they survive the blob round-trip, per backend.
#[test]
fn test_token_and_pairing_method_round_trip_through_the_settings_blob() {
    let original = two_backends();
    let reloaded = settings::load(Some(&settings::save_blob(&original)));
    assert_eq!(reloaded, original);
    assert_eq!(
        reloaded
            .backends
            .iter()
            .map(|b| (b.token.clone(), b.pairing))
            .collect::<Vec<(Option<String>, PairingMethod)>>(),
        vec![
            (Some("salon-token".to_string()), PairingMethod::Code),
            (None, PairingMethod::Qr),
        ],
        "each backend keeps its own token and method"
    );
}

// Criterion: the pairing method defaults to `Code` — the transport that needs
// nothing but the terminal.
#[test]
fn test_pairing_method_defaults_to_code() {
    assert_eq!(PairingMethod::default(), PairingMethod::Code);

    let mut settings = AppSettings::default();
    settings
        .add("Salon", "http://192.168.1.107:4000")
        .expect("add Salon");
    let added = settings.backends.first().expect("the added backend");
    assert_eq!(added.pairing, PairingMethod::Code);
    assert_eq!(added.token, None, "a new backend starts unpaired");
}

// Criterion (non-nominal): a phase 6.3-era blob (no token, no method) still
// loads — with the backend simply unpaired, which the settings page can act on.
#[test]
fn test_a_phase_6_3_blob_loads_unpaired_with_the_default_method() {
    let blob = r#"{"backends":[{"name":"Salon","url":"http://192.168.1.107:4000","restore_during_playback":true}],"active":0}"#;
    let loaded = settings::load(Some(blob));
    let entry = loaded.active_backend().expect("the stored backend");
    assert_eq!(entry.token, None);
    assert_eq!(entry.pairing, PairingMethod::Code);
}

// Criterion: the pairing method is a **per-backend** setting the user can change
// afterwards, and changing one leaves the other alone.
#[test]
fn test_set_pairing_method_updates_only_that_backend() {
    let mut settings = two_backends();
    settings
        .set_pairing_method(0, PairingMethod::Qr)
        .expect("switch Salon to QR");
    assert_eq!(
        settings.backends.first().map(|b| b.pairing),
        Some(PairingMethod::Qr)
    );
    assert_eq!(
        settings.backends.get(1).map(|b| b.pairing),
        Some(PairingMethod::Qr),
        "the other backend must be left exactly as it was"
    );
}

// Criterion (non-nominal): a stale index is refused rather than panicking, like
// every other index-taking settings operation.
#[test]
fn test_set_pairing_method_on_an_unknown_backend_is_refused() {
    let mut settings = two_backends();
    assert_eq!(
        settings.set_pairing_method(9, PairingMethod::Qr),
        Err(SettingsError::UnknownBackend)
    );
    assert_eq!(settings, two_backends(), "nothing may have changed");
}

// Criterion: the token obtained by pairing is stored on that backend, and can be
// cleared again (an unpaired backend is a normal state).
#[test]
fn test_set_token_stores_and_clears_the_token() {
    let mut settings = two_backends();
    settings
        .set_token(1, Some("bureau-token".to_string()))
        .expect("store the paired token");
    assert_eq!(
        settings.backends.get(1).and_then(|b| b.token.clone()),
        Some("bureau-token".to_string())
    );

    settings.set_token(1, None).expect("clear the token");
    assert_eq!(settings.backends.get(1).and_then(|b| b.token.clone()), None);
}

// Criterion (non-nominal): storing a token on a stale index is refused.
#[test]
fn test_set_token_on_an_unknown_backend_is_refused() {
    let mut settings = two_backends();
    assert_eq!(
        settings.set_token(9, Some("token".to_string())),
        Err(SettingsError::UnknownBackend)
    );
    assert_eq!(settings, two_backends(), "nothing may have changed");
}

// Criterion: the app resolves the *active* backend's token, and reports none
// while that backend is unpaired.
#[test]
fn test_active_token_follows_the_active_backend() {
    let mut settings = two_backends();
    assert_eq!(settings.active_token(), Some("salon-token".to_string()));

    settings.activate(1).expect("switch to Bureau");
    assert_eq!(
        settings.active_token(),
        None,
        "Bureau has never been paired"
    );

    assert_eq!(AppSettings::default().active_token(), None);
}

// Criterion: scanning a pair link creates the whole entry — address, name and
// token — and activates it. Nothing was typed.
#[test]
fn test_upsert_from_pair_link_creates_and_activates_the_backend() {
    let mut settings = AppSettings::default();
    let index = settings
        .upsert_from_pair_link(
            &pair_link("http://192.168.1.107:4000", Some("blue2th-PC")),
            "api-token",
        )
        .expect("a scanned link must create the backend");

    assert_eq!(index, 0);
    assert_eq!(settings.backends.len(), 1);
    assert_eq!(
        settings.active,
        Some(0),
        "the scanned backend becomes active"
    );
    let entry = settings.active_backend().expect("the created backend");
    assert_eq!(entry.name, "blue2th-PC");
    assert_eq!(entry.url, "http://192.168.1.107:4000");
    assert_eq!(entry.token.as_deref(), Some("api-token"));
}

// Criterion: the pairing method is chosen when the backend is added, and a
// scanned one was added by QR — so that is what its settings page offers next.
#[test]
fn test_upsert_from_pair_link_records_the_qr_as_the_new_backend_method() {
    let mut settings = AppSettings::default();
    settings
        .upsert_from_pair_link(
            &pair_link("http://192.168.1.107:4000", Some("blue2th-PC")),
            "api-token",
        )
        .expect("a scanned link must create the backend");
    assert_eq!(
        settings.backends.first().map(|b| b.pairing),
        Some(PairingMethod::Qr)
    );
}

// Criterion: for a backend the app already knows, the locally chosen pairing
// method survives the scan exactly as the local name does.
#[test]
fn test_upsert_from_pair_link_keeps_the_method_of_a_known_backend() {
    let mut settings = two_backends();
    settings
        .upsert_from_pair_link(
            &pair_link("http://192.168.1.107:4000", Some("blue2th-PC")),
            "fresh-token",
        )
        .expect("a known URL must be updated");
    assert_eq!(
        settings.backends.first().map(|b| b.pairing),
        Some(PairingMethod::Code),
        "the user's own choice must not be overwritten by a scan"
    );
}

// Criterion (non-nominal): a QR scanned for a URL the app already knows updates
// the token rather than duplicating the entry — and the locally chosen name
// survives, since the user may have renamed it deliberately.
#[test]
fn test_upsert_from_pair_link_updates_a_known_url_without_touching_its_name() {
    let mut settings = two_backends();
    let index = settings
        .upsert_from_pair_link(
            &pair_link("http://192.168.1.107:4000", Some("blue2th-PC")),
            "fresh-token",
        )
        .expect("a known URL must be updated");

    assert_eq!(index, 0);
    assert_eq!(settings.backends.len(), 2, "no duplicate entry");
    let entry = settings.backends.first().expect("the known backend");
    assert_eq!(entry.name, "Salon", "the local name must survive the scan");
    assert_eq!(entry.token.as_deref(), Some("fresh-token"));
    assert_eq!(settings.active, Some(0), "the re-paired backend is active");
}

// Criterion: the URL is normalised before it is matched, so the same backend
// advertised with a trailing slash is still the same entry.
#[test]
fn test_upsert_from_pair_link_matches_a_known_url_across_a_trailing_slash() {
    let mut settings = two_backends();
    settings
        .upsert_from_pair_link(
            &pair_link("http://192.168.1.107:4000/", Some("blue2th-PC")),
            "fresh-token",
        )
        .expect("a trailing slash must not create a second entry");
    assert_eq!(settings.backends.len(), 2);
}

// Criterion (non-nominal): a link carrying an address the app cannot use is
// refused, and leaves no half-created entry behind.
#[test]
fn test_upsert_from_pair_link_refuses_a_malformed_url_and_stores_nothing() {
    let mut settings = AppSettings::default();
    assert_eq!(
        settings.upsert_from_pair_link(&pair_link("192.168.1.107:4000", Some("blue2th-PC")), "t"),
        Err(SettingsError::MalformedUrl)
    );
    assert!(settings.backends.is_empty());
    assert_eq!(settings.active, None);
}

// Criterion (non-nominal): only `url` and `code` are required in a pair link, so
// a nameless one cannot name a *new* backend — it is refused rather than stored
// under an invented label.
#[test]
fn test_upsert_from_pair_link_refuses_to_create_a_nameless_backend() {
    let mut settings = AppSettings::default();
    assert_eq!(
        settings.upsert_from_pair_link(&pair_link("http://192.168.1.107:4000", None), "t"),
        Err(SettingsError::Name(NameError::Empty))
    );
    assert!(settings.backends.is_empty());
}

// Criterion: a nameless link for a URL the app already knows still works — the
// name was never needed there, since the local one is kept anyway.
#[test]
fn test_upsert_from_pair_link_accepts_a_nameless_link_for_a_known_url() {
    let mut settings = two_backends();
    let index = settings
        .upsert_from_pair_link(&pair_link("http://192.168.1.107:4000", None), "fresh-token")
        .expect("a known URL needs no name");
    assert_eq!(index, 0);
    assert_eq!(
        settings.backends.first().map(|b| b.name.as_str()),
        Some("Salon")
    );
    assert_eq!(
        settings.backends.first().and_then(|b| b.token.clone()),
        Some("fresh-token".to_string())
    );
}

// Criterion (non-nominal): a scanned name that collides with another backend is
// refused rather than silently creating a second "Salon" — the status encart
// could not tell the two apart. Nothing is stored.
#[test]
fn test_upsert_from_pair_link_refuses_a_name_another_backend_already_uses() {
    let mut settings = two_backends();
    assert_eq!(
        settings.upsert_from_pair_link(&pair_link("http://10.0.0.9:4000", Some("Salon")), "t"),
        Err(SettingsError::DuplicateName)
    );
    assert_eq!(settings, two_backends(), "nothing may have changed");
}

// ── The status dot's three states (phase 6.6) ────────────────────────────────

// Criterion: a backend that answers but was never paired is neither working nor
// unreachable. It gets its own state, because a green dot over it would promise
// something every route but `/health` refuses.
#[test]
fn test_backend_health_separates_unpaired_from_ready() {
    assert_eq!(
        settings::backend_health(true, true),
        settings::BackendHealth::Ready
    );
    assert_eq!(
        settings::backend_health(true, false),
        settings::BackendHealth::Unpaired,
        "reachable but tokenless is the in-between state, not a working one"
    );
}

// Criterion: unreachable wins over unpaired — pairing a backend the phone cannot
// talk to is not the next step, reaching it is.
#[test]
fn test_backend_health_reports_offline_whatever_the_pairing() {
    for paired in [true, false] {
        assert_eq!(
            settings::backend_health(false, paired),
            settings::BackendHealth::Offline,
            "an unreachable backend is offline whether or not a token is held"
        );
    }
}

// Criterion: nothing configured reads as offline, not as unpaired — there is no
// backend to pair with yet, so the settings page is the answer either way.
#[test]
fn test_backend_health_with_nothing_configured_is_offline() {
    let empty = AppSettings::default();
    assert_eq!(empty.active_token(), None);
    assert_eq!(
        settings::backend_health(false, empty.active_token().is_some()),
        settings::BackendHealth::Offline
    );
}
