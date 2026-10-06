// SPDX-License-Identifier: MIT OR Apache-2.0

//! The browser build's pure policies (#160): one backend at the page origin,
//! the backend's config adopted rather than overwritten, the start page, and
//! page events mapped to presence.
//!
//! `cargo test` runs on the host and never builds wasm, so everything here is
//! the logic both builds share; the `localStorage`, `location.origin`, event
//! listener and `keepalive` fetch glue is checked by hand (see the spec's
//! manual verification).

use blue2th_frontend::lifecycle::{presence_for, PageEvent};
use blue2th_frontend::settings::{
    self, AppSettings, BackendEntry, ClientKind, ConfigSync, PairingMethod, StartPage,
};
use blue2th_proto::{ClientPresence, ServerConfig};

/// The page origin under `dx serve`.
const ORIGIN: &str = "http://localhost:8080";

/// A backend entry, built by hand: a fixture must not depend on the functions
/// under test.
fn entry(
    name: &str,
    url: &str,
    token: Option<&str>,
    restore: bool,
    reconnect: bool,
) -> BackendEntry {
    BackendEntry {
        name: name.to_string(),
        url: url.to_string(),
        restore_during_playback: restore,
        auto_reconnect: reconnect,
        token: token.map(str::to_string),
        pairing: PairingMethod::Code,
        id: None,
    }
}

/// Settings holding `backends`, with `active` selected.
fn stored(backends: Vec<BackendEntry>, active: Option<usize>) -> AppSettings {
    AppSettings {
        backends,
        active,
        auto_repair_url: true,
        discovery_adds_backends: true,
    }
}

/// What a backend's `GET /config` returns: the toggles set to **different**
/// values, so a swap between them shows, and the volume lock on.
fn backend_config() -> ServerConfig {
    ServerConfig {
        name: "blue2th-PC".to_string(),
        restore_during_playback: false,
        auto_reconnect: true,
        spotify_volume_lock: true,
    }
}

// ── browser_settings ─────────────────────────────────────────────────────────

// Criterion: in the browser, the settings hold exactly one backend whose URL is
// the page origin, active, keeping the stored token, name and toggles — even
// when the stored entry carries another URL.
#[test]
fn test_browser_settings_moves_the_stored_entry_to_the_origin() {
    let blob = stored(
        vec![entry(
            "Salon",
            "http://192.168.1.107:4000",
            Some("tok-salon"),
            false,
            false,
        )],
        Some(0),
    );

    let settings = settings::browser_settings(blob, ORIGIN);

    assert_eq!(
        settings.backends,
        vec![entry("Salon", ORIGIN, Some("tok-salon"), false, false)]
    );
    assert_eq!(settings.active, Some(0));
}

// Criterion (guard, "exactly one backend"): a stored blob with several entries
// is reduced to one, at the origin, carrying the **active** entry's token, name
// and toggles. Near-miss: two entries with the second one active — keeping the
// list, or keeping the first entry, both look like a valid settings blob.
#[test]
fn test_browser_settings_reduces_several_entries_to_the_active_one() {
    let blob = stored(
        vec![
            entry(
                "Salon",
                "http://192.168.1.107:4000",
                Some("tok-salon"),
                true,
                true,
            ),
            entry(
                "Bureau",
                "http://192.168.1.42:4000",
                Some("tok-bureau"),
                false,
                true,
            ),
        ],
        Some(1),
    );

    let settings = settings::browser_settings(blob, ORIGIN);

    assert_eq!(
        settings.backends,
        vec![entry("Bureau", ORIGIN, Some("tok-bureau"), false, true)]
    );
    assert_eq!(settings.active, Some(0));
}

// Criterion: with nothing stored (first visit, private window, a throwing or
// malformed store, all of which `load` reads as empty), the browser still has
// its one backend — the origin, active and unpaired.
#[test]
fn test_browser_settings_without_a_stored_entry_creates_the_origin_backend() {
    let settings = settings::browser_settings(settings::load(None), ORIGIN);

    assert_eq!(settings.backends.len(), 1, "{settings:?}");
    assert_eq!(settings.active, Some(0));
    assert_eq!(settings.active_url().as_deref(), Some(ORIGIN));
    assert_eq!(settings.active_token(), None, "a fresh browser is unpaired");
}

// Criterion: the same holds for a store that held garbage — `load`'s contract,
// then the reduction, never a failed start.
#[test]
fn test_browser_settings_over_a_malformed_blob_creates_the_origin_backend() {
    let settings = settings::browser_settings(settings::load(Some("{not json")), ORIGIN);

    assert_eq!(settings.active_url().as_deref(), Some(ORIGIN));
    assert_eq!(settings.active_token(), None);
}

// Criterion (guard, empty value): an empty origin, or the literal `"null"` a
// `file://` page reports, yields no backend at all. Near-miss: a stored, paired
// entry — keeping it, or rewriting its URL to the empty origin, would both
// leave something for a call to go to.
#[test]
fn test_browser_settings_with_an_unusable_origin_has_no_backend() {
    for origin in ["", "null"] {
        let blob = stored(
            vec![entry("Salon", ORIGIN, Some("tok-salon"), true, true)],
            Some(0),
        );

        let settings = settings::browser_settings(blob, origin);

        assert!(
            settings.backends.is_empty(),
            "origin {origin:?}: {settings:?}"
        );
        assert_eq!(settings.active, None, "origin {origin:?}");
        assert_eq!(settings.active_url(), None, "origin {origin:?}");
    }
}

// ── adopt_config ─────────────────────────────────────────────────────────────

// Criterion: the browser adopts `name`, `restore_during_playback` and
// `auto_reconnect` from `GET /config` into its active entry; the token, the URL
// and everything else stay as they were. The stored toggles are the opposite
// of the backend's, so leaving one out or swapping the two fails.
#[test]
fn test_adopt_config_takes_the_name_and_both_toggles() {
    let mut settings = stored(
        vec![entry("Salon", ORIGIN, Some("tok-salon"), true, false)],
        Some(0),
    );

    settings::adopt_config(&mut settings, &backend_config());

    assert_eq!(
        settings,
        stored(
            vec![entry("blue2th-PC", ORIGIN, Some("tok-salon"), false, true)],
            Some(0),
        )
    );
}

// Criterion: adoption goes into the **active** entry only.
#[test]
fn test_adopt_config_changes_the_active_entry_only() {
    let other = entry(
        "Salon",
        "http://192.168.1.107:4000",
        Some("tok-salon"),
        true,
        false,
    );
    let mut settings = stored(
        vec![
            other.clone(),
            entry("Bureau", ORIGIN, Some("tok-bureau"), true, false),
        ],
        Some(1),
    );

    settings::adopt_config(&mut settings, &backend_config());

    assert_eq!(settings.backends.first(), Some(&other));
    assert_eq!(
        settings.backends.get(1),
        Some(&entry(
            "blue2th-PC",
            ORIGIN,
            Some("tok-bureau"),
            false,
            true
        ))
    );
}

// Criterion (non-nominal): with no active entry there is nothing to adopt
// into, and nothing changes.
#[test]
fn test_adopt_config_without_an_active_entry_changes_nothing() {
    let before = stored(
        vec![entry("Salon", ORIGIN, Some("tok-salon"), true, false)],
        None,
    );
    let mut settings = before.clone();

    settings::adopt_config(&mut settings, &backend_config());

    assert_eq!(settings, before);
}

// ── client kind: reconnection and start page ─────────────────────────────────

// Criterion (guard, "never push config on reconnection in the browser"): the
// browser **reads** the backend's config when the backend becomes usable
// again; the phone keeps pushing its own. Near-miss: the phone's answer, which
// is what the shared health loop did for every client before #160.
#[test]
fn test_config_sync_on_reconnect_reads_in_the_browser_and_pushes_on_the_phone() {
    assert_eq!(
        settings::config_sync_on_reconnect(ClientKind::Browser),
        ConfigSync::Read
    );
    assert_eq!(
        settings::config_sync_on_reconnect(ClientKind::Phone),
        ConfigSync::Push
    );
}

// Criterion: an unpaired browser opens on `/settings`; a paired one on `/`.
#[test]
fn test_start_page_sends_an_unpaired_browser_to_settings() {
    assert_eq!(
        settings::start_page(ClientKind::Browser, false),
        StartPage::Settings
    );
    assert_eq!(
        settings::start_page(ClientKind::Browser, true),
        StartPage::Home
    );
}

// Criterion: the phone's start-up route is unchanged — home, paired or not.
// Near-miss: an unpaired phone, which an "unpaired → settings" rule applied to
// every client would send to settings.
#[test]
fn test_start_page_keeps_the_phone_on_home() {
    assert_eq!(
        settings::start_page(ClientKind::Phone, false),
        StartPage::Home
    );
    assert_eq!(
        settings::start_page(ClientKind::Phone, true),
        StartPage::Home
    );
}

// Criterion: the phone build behaves exactly as before — every host build,
// which is what the Android build compiles as, is the phone.
#[test]
fn test_the_native_build_is_the_phone() {
    assert_eq!(settings::CLIENT_KIND, ClientKind::Phone);
}

// ── presence ─────────────────────────────────────────────────────────────────

/// Every page event. The `match` is exhaustive on purpose: a new variant does
/// not compile here until it is listed, so the "never `Gone`" test covers it.
fn every_page_event() -> Vec<PageEvent> {
    let events = vec![
        PageEvent::Load,
        PageEvent::Hidden,
        PageEvent::Visible,
        PageEvent::PageHide,
    ];
    for event in &events {
        match event {
            PageEvent::Load | PageEvent::Hidden | PageEvent::Visible | PageEvent::PageHide => {},
        }
    }
    events
}

// Criterion: load → `Foreground`, hidden → `Background`, visible →
// `Foreground`, `pagehide` → `Background`.
#[test]
fn test_presence_for_maps_each_page_event() {
    assert_eq!(presence_for(PageEvent::Load), ClientPresence::Foreground);
    assert_eq!(presence_for(PageEvent::Hidden), ClientPresence::Background);
    assert_eq!(presence_for(PageEvent::Visible), ClientPresence::Foreground);
    assert_eq!(
        presence_for(PageEvent::PageHide),
        ClientPresence::Background
    );
}

// Criterion (guard): no page event ever maps to `Gone` — a reload fires
// `pagehide` exactly as a close does, and `Gone` pauses Spotify at once.
// Near-miss: `pagehide` → `Gone`, what an Android-style "closing" mapping
// would do.
#[test]
fn test_presence_for_never_reports_gone() {
    for event in every_page_event() {
        assert_ne!(
            presence_for(event),
            ClientPresence::Gone,
            "{event:?} must not report Gone"
        );
    }
}
