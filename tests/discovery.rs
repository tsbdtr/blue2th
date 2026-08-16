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

//! Finding the backend on the network (phase 6.6) — the pure half.
//!
//! The classifier ([`settings::reconcile`]) is the whole decision the feature
//! makes: it turns a discovered service plus the settings the app holds into one
//! of five actions, driven by the id match, the URL fallback and the two
//! app-level toggles. No network and no JNI take part in it, so the matrix is
//! exercised exhaustively here.
//!
//! The mDNS announcement, the browse itself and Android's `MulticastLock` are a
//! manual-test boundary, exactly as BlueZ and PipeWire are on the backend.

use blue2th::discovery;
use blue2th::settings::{
    self, AppSettings, BackendEntry, DiscoveryAction, PairingMethod, SettingsError,
};
use blue2th_proto::{DiscoveredBackend, NameError};

/// The address Salon was last known to answer at.
const SALON_URL: &str = "http://192.168.1.107:4000";
/// The address the router handed it after a reboot.
const SALON_NEW_URL: &str = "http://192.168.1.200:4000";
/// Salon's stable id, as its mDNS record advertises it.
const SALON_ID: &str = "salon-backend-id";

/// Settings holding one paired, id-carrying backend, with both discovery
/// settings on (the shipped defaults).
///
/// Built by hand rather than through `add`/`set_backend_id`: a fixture must not
/// depend on the functions under test.
fn known_salon() -> AppSettings {
    AppSettings {
        backends: vec![BackendEntry {
            name: "Salon".to_string(),
            url: SALON_URL.to_string(),
            restore_during_playback: true,
            token: Some("salon-token".to_string()),
            pairing: PairingMethod::Code,
            id: Some(SALON_ID.to_string()),
        }],
        active: Some(0),
        auto_repair_url: true,
        discovery_adds_backends: true,
    }
}

/// The same settings, but for a pre-6.6 entry that never learnt an id.
fn known_salon_without_id() -> AppSettings {
    let mut settings = known_salon();
    if let Some(entry) = settings.backends.first_mut() {
        entry.id = None;
    }
    settings
}

/// A discovered service.
fn found(id: Option<&str>, name: &str, url: &str) -> DiscoveredBackend {
    DiscoveredBackend {
        id: id.map(str::to_string),
        name: name.to_string(),
        url: url.to_string(),
    }
}

// ── The classifier matrix ────────────────────────────────────────────────────

// Criterion: a known backend answering at the address the app already has is
// `UpToDate` — nothing to repair, no write, no toast.
#[test]
fn test_reconcile_reports_a_known_backend_at_a_known_address_as_up_to_date() {
    let settings = known_salon();
    assert_eq!(
        settings::reconcile(&settings, &found(Some(SALON_ID), "blue2th-PC", SALON_URL)),
        DiscoveryAction::UpToDate
    );
}

// Criterion: the id matches a known entry and the address moved, with
// `auto_repair_url` on (the default) — repair in place, no question asked.
#[test]
fn test_reconcile_repairs_a_moved_backend_when_auto_repair_is_on() {
    let settings = known_salon();
    assert_eq!(
        settings::reconcile(
            &settings,
            &found(Some(SALON_ID), "blue2th-PC", SALON_NEW_URL)
        ),
        DiscoveryAction::Repair {
            index: 0,
            url: SALON_NEW_URL.to_string(),
        }
    );
}

// Criterion: with `auto_repair_url` off, the same input yields `ConfirmRepair`
// and **no write happens** until the user confirms.
#[test]
fn test_reconcile_asks_before_repairing_when_auto_repair_is_off() {
    let mut settings = known_salon();
    settings.auto_repair_url = false;
    let service = found(Some(SALON_ID), "blue2th-PC", SALON_NEW_URL);

    assert_eq!(
        settings::reconcile(&settings, &service),
        DiscoveryAction::ConfirmRepair {
            index: 0,
            url: SALON_NEW_URL.to_string(),
        }
    );
    assert_eq!(
        settings.backends.first().map(|b| b.url.as_str()),
        Some(SALON_URL),
        "classifying must be pure: nothing is written before confirmation"
    );
}

// Criterion: the repaired URL is normalised on the way through the classifier,
// so a record advertising a trailing slash does not look like a move.
#[test]
fn test_reconcile_normalises_the_discovered_address() {
    let settings = known_salon();
    let with_slash = format!("{SALON_URL}/");
    assert_eq!(
        settings::reconcile(&settings, &found(Some(SALON_ID), "blue2th-PC", &with_slash)),
        DiscoveryAction::UpToDate,
        "a trailing slash is the same address, not a move"
    );
}

// Criterion (non-nominal): a discovered service with no `id` TXT record falls
// back to matching on URL, exactly as before — it must never be treated as a new
// machine on the basis of a missing id alone.
#[test]
fn test_reconcile_falls_back_to_the_url_when_the_service_has_no_id() {
    let settings = known_salon();
    assert_eq!(
        settings::reconcile(&settings, &found(None, "blue2th-PC", SALON_URL)),
        DiscoveryAction::UpToDate
    );
}

// Criterion (non-nominal): a pre-6.6 entry has no stored id — matching falls
// back to its URL, so the machine it already knows is recognised rather than
// offered as new.
#[test]
fn test_reconcile_matches_a_pre_6_6_entry_on_its_url() {
    let settings = known_salon_without_id();
    assert_eq!(
        settings::reconcile(&settings, &found(Some(SALON_ID), "blue2th-PC", SALON_URL)),
        DiscoveryAction::UpToDate
    );
}

// Criterion: an unknown backend is `Addable` while `discovery_adds_backends` is
// on — tapping it creates the entry, then the normal pairing flow runs.
#[test]
fn test_reconcile_offers_an_unknown_backend_when_adding_is_on() {
    let settings = known_salon();
    assert_eq!(
        settings::reconcile(
            &settings,
            &found(Some("other-id"), "Bureau", "http://192.168.1.42:4000")
        ),
        DiscoveryAction::Addable
    );
}

// Criterion: with `discovery_adds_backends` off, an unknown backend yields
// `Ignored` — listed as found, but not addable, and no entry is created.
#[test]
fn test_reconcile_ignores_an_unknown_backend_when_adding_is_off() {
    let mut settings = known_salon();
    settings.discovery_adds_backends = false;
    assert_eq!(
        settings::reconcile(
            &settings,
            &found(Some("other-id"), "Bureau", "http://192.168.1.42:4000")
        ),
        DiscoveryAction::Ignored
    );
    assert_eq!(settings.backends.len(), 1, "no entry may be created");
}

// Criterion: an unknown backend with no id at an unknown address is still just a
// new backend — the two toggles decide, not the missing id.
#[test]
fn test_reconcile_handles_an_idless_unknown_backend_under_both_settings() {
    let mut settings = known_salon();
    let service = found(None, "Bureau", "http://192.168.1.42:4000");
    assert_eq!(
        settings::reconcile(&settings, &service),
        DiscoveryAction::Addable
    );
    settings.discovery_adds_backends = false;
    assert_eq!(
        settings::reconcile(&settings, &service),
        DiscoveryAction::Ignored
    );
}

// Criterion: `auto_repair_url` governs the known case only — it must not make an
// unknown backend addable, nor stop one being offered.
#[test]
fn test_reconcile_auto_repair_does_not_govern_unknown_backends() {
    let mut settings = known_salon();
    settings.auto_repair_url = false;
    assert_eq!(
        settings::reconcile(
            &settings,
            &found(Some("other-id"), "Bureau", "http://192.168.1.42:4000")
        ),
        DiscoveryAction::Addable
    );
    // And an id match at the known address stays `UpToDate` either way.
    assert_eq!(
        settings::reconcile(&settings, &found(Some(SALON_ID), "blue2th-PC", SALON_URL)),
        DiscoveryAction::UpToDate
    );
}

// Criterion (non-nominal): a record advertising an unusable address is `Ignored`
// rather than stored as an address every later call would fail on.
#[test]
fn test_reconcile_ignores_a_malformed_advertised_address() {
    let settings = known_salon();
    for bad in ["", "   ", "192.168.1.200:4000", "http://", "http://:4000"] {
        assert_eq!(
            settings::reconcile(&settings, &found(Some(SALON_ID), "blue2th-PC", bad)),
            DiscoveryAction::Ignored,
            "{bad} is not a usable backend address"
        );
    }
}

// Criterion: the id identifies the *machine*, not the address — a service whose
// id matches while the app also knows a different backend at that new address
// still repairs the id-matched entry rather than inventing a second one.
#[test]
fn test_reconcile_matches_on_the_id_before_the_url() {
    let mut settings = known_salon();
    settings.backends.push(BackendEntry {
        name: "Bureau".to_string(),
        url: "http://192.168.1.42:4000".to_string(),
        restore_during_playback: true,
        token: None,
        pairing: PairingMethod::Code,
        id: Some("bureau-backend-id".to_string()),
    });
    assert_eq!(
        settings::reconcile(
            &settings,
            &found(Some("bureau-backend-id"), "Bureau", SALON_NEW_URL)
        ),
        DiscoveryAction::Repair {
            index: 1,
            url: SALON_NEW_URL.to_string(),
        }
    );
}

// ── Applying what the classifier decided ─────────────────────────────────────

// Criterion: adding a discovered backend creates it from the mDNS name and
// address and leaves `token: None`, so the app still reports "not paired" until
// the six-character code is exchanged. Discovery is not authentication.
#[test]
fn test_add_discovered_creates_an_unpaired_entry() {
    let mut settings = known_salon();
    let service = found(
        Some("bureau-backend-id"),
        "Bureau",
        "http://192.168.1.42:4000",
    );

    let index = settings
        .add_discovered(&service)
        .expect("an unknown backend can be added from the discovery list");

    assert_eq!(index, 1);
    let bureau = settings.backends.get(1).expect("the entry was created");
    assert_eq!(bureau.name, "Bureau");
    assert_eq!(bureau.url, "http://192.168.1.42:4000");
    assert_eq!(
        bureau.token, None,
        "being found grants nothing: the code is still required"
    );
    assert_eq!(
        bureau.id.as_deref(),
        Some("bureau-backend-id"),
        "the id is adopted at creation, so the next lease change repairs it"
    );
}

// Criterion (non-nominal): a discovered name colliding with an existing entry is
// refused through the same `add` rules as typing — a service announcement cannot
// smuggle in what typing cannot.
#[test]
fn test_add_discovered_refuses_a_duplicate_name() {
    let mut settings = known_salon();
    let service = found(Some("other-id"), "Salon", "http://192.168.1.42:4000");
    assert_eq!(
        settings.add_discovered(&service),
        Err(SettingsError::DuplicateName)
    );
    assert_eq!(settings.backends.len(), 1, "nothing may have been created");
}

// Criterion: the shared proto name rule applies to an announced name too, and a
// malformed advertised address is refused.
#[test]
fn test_add_discovered_applies_the_name_and_url_rules() {
    let mut settings = known_salon();
    assert_eq!(
        settings.add_discovered(&found(
            Some("other-id"),
            "salon tv",
            "http://192.168.1.42:4000"
        )),
        Err(SettingsError::Name(NameError::BadChar))
    );
    assert_eq!(
        settings.add_discovered(&found(Some("other-id"), "Bureau", "192.168.1.42:4000")),
        Err(SettingsError::MalformedUrl)
    );
    assert_eq!(settings.backends.len(), 1, "nothing may have been created");
}

// Criterion (non-nominal): the scan finds several backends and two entries with
// the same id must never both be created — once added, the same service
// classifies as known.
#[test]
fn test_a_backend_added_from_discovery_is_never_added_twice() {
    let mut settings = known_salon();
    let service = found(
        Some("bureau-backend-id"),
        "Bureau",
        "http://192.168.1.42:4000",
    );
    settings
        .add_discovered(&service)
        .expect("first find creates the entry");

    assert_eq!(
        settings::reconcile(&settings, &service),
        DiscoveryAction::UpToDate,
        "a second sighting of the same id is a known backend, not a new one"
    );
    assert_eq!(settings.backends.len(), 2);
}

// Criterion: applying a `Repair` is `set_url` on the classified index — the URL
// moves, the entry count does not, and the token survives.
#[test]
fn test_applying_the_classified_repair_moves_only_the_url() {
    let mut settings = known_salon();
    let service = found(Some(SALON_ID), "blue2th-PC", SALON_NEW_URL);

    match settings::reconcile(&settings, &service) {
        DiscoveryAction::Repair { index, url } => {
            settings.set_url(index, &url).expect("apply the repair");
        },
        other => assert_eq!(
            other,
            DiscoveryAction::Repair {
                index: 0,
                url: SALON_NEW_URL.to_string()
            },
            "a moved, id-matched backend must be repairable"
        ),
    }

    assert_eq!(settings.backends.len(), 1, "no duplicate entry");
    let salon = settings.backends.first().expect("Salon is still there");
    assert_eq!(salon.url, SALON_NEW_URL);
    assert_eq!(salon.token.as_deref(), Some("salon-token"));
    assert_eq!(salon.name, "Salon");
}

// Criterion: after a repair, the same service is `UpToDate` — the second scan
// writes nothing at all.
#[test]
fn test_a_repaired_backend_is_up_to_date_on_the_next_scan() {
    let mut settings = known_salon();
    settings.set_url(0, SALON_NEW_URL).expect("repair Salon");
    assert_eq!(
        settings::reconcile(
            &settings,
            &found(Some(SALON_ID), "blue2th-PC", SALON_NEW_URL)
        ),
        DiscoveryAction::UpToDate
    );
}

// ── The JNI/network boundary, from the outside ───────────────────────────────

// Criterion: the Search button's enabled state is a pure function over the
// cached preflight verdict — no JNI call happens to render it.
#[test]
fn test_search_button_state_derives_from_the_preflight_verdict() {
    assert!(discovery::search_enabled(true));
    assert!(!discovery::search_enabled(false));
}

// Criterion: a shared JNI helper captures the Java exception's `toString()`
// **before** clearing it, mirroring `bt_err_clear`. Asserted on the source, the
// way the `.dex` contract is: the behaviour itself only exists on a device, but
// leaving the capture after the clear is an ART abort no test could catch.
#[test]
fn test_jni_util_captures_the_exception_before_clearing_it() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/jni_util.rs");
    let source = std::fs::read_to_string(&path)
        .map_err(|e| format!("read {path:?}: {e}"))
        .expect("the shared JNI seam must exist");

    let capture = source
        .find("exception_occurred")
        .expect("the helper must capture the pending exception");
    let clear = source
        .find("exception_clear")
        .expect("the helper must clear the pending exception");
    assert!(
        capture < clear,
        "the exception's toString() must be captured before it is cleared"
    );
    assert!(
        source.contains("toString"),
        "the captured detail is the exception's toString(), not the jni crate's generic message"
    );
    assert!(
        !source.contains("attach_current_thread()"),
        "attach_current_thread's AttachGuard detaches on drop and aborts the next FindClass"
    );
    assert!(
        source.contains("attach_current_thread_permanently"),
        "the env helper must attach permanently, as src/bluetooth.rs documents"
    );
}

// Criterion: the manifest declares `CHANGE_WIFI_MULTICAST_STATE` — a normal
// permission with no runtime prompt, without which the multicast lock is refused
// on a device. The manifest is a frozen copy of dx's template: one line, no
// other edit.
#[test]
fn test_manifest_declares_the_multicast_permission() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("android/AndroidManifest.xml");
    let manifest = std::fs::read_to_string(&path)
        .map_err(|e| format!("read {path:?}: {e}"))
        .expect("the frozen Android manifest must exist");
    assert!(
        manifest.contains("android.permission.CHANGE_WIFI_MULTICAST_STATE"),
        "the multicast lock needs CHANGE_WIFI_MULTICAST_STATE, got:\n{manifest}"
    );
}
