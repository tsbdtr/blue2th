// SPDX-License-Identifier: MIT OR Apache-2.0

use super::*;

// Criterion: the Search button's enabled state is a pure function over the
// cached preflight verdict — a ROM that cannot resolve `MulticastLock`
// renders it disabled, and no JNI call is ever attempted.
// #159: with browsing available, the verdict alone decides.
#[test]
fn test_search_enabled_follows_the_preflight_verdict() {
    assert!(
        search_enabled_on(true, true),
        "a capable ROM keeps the button live"
    );
    assert!(
        !search_enabled_on(false, true),
        "a failed preflight disables the button rather than failing on tap"
    );
}

// Criterion (#159): `search_enabled` is false whenever browsing is
// unavailable on the target, whatever the multicast verdict.
// Guard near-miss: `(true, false)` — multicast says yes (as it does off
// Android, i.e. on wasm), and only the browse-availability guard says no.
#[test]
fn test_search_enabled_is_false_without_browsing_whatever_the_multicast_verdict() {
    assert!(
        !search_enabled_on(true, false),
        "a target with no browse (wasm) must disable the button even though \
         multicast_supported() answers true there"
    );
    assert!(!search_enabled_on(false, false));
}

// Criterion (non-nominal): the multicast lock could not be acquired — browse
// anyway. Only the browse result decides what the user is told.
#[test]
fn test_browse_proceeds_even_without_the_multicast_lock() {
    assert!(browse_proceeds(true));
    assert!(
        browse_proceeds(false),
        "a failed lock is not a failed scan: hotspot mode has the phone as AP"
    );
}

/// A discovered service, for the deduplication cases.
fn service(id: Option<&str>, url: &str) -> DiscoveredBackend {
    DiscoveredBackend {
        id: id.map(str::to_string),
        name: "blue2th-PC".to_string(),
        url: url.to_string(),
    }
}

// Criterion (non-nominal): the same instance resolves more than once on a
// busy network — a repeat at the same address is the same machine.
#[test]
fn test_is_new_find_rejects_a_repeat_at_the_same_address() {
    let found = vec![service(Some("salon-id"), "http://192.168.1.107:4000")];
    assert!(!is_new_find(
        &found,
        &service(Some("salon-id"), "http://192.168.1.107:4000")
    ));
    // Even when the repeat lost its TXT id on the second resolution.
    assert!(!is_new_find(
        &found,
        &service(None, "http://192.168.1.107:4000")
    ));
}

// Criterion (non-nominal): a multi-homed backend resolves once per interface,
// at two different addresses — the id says it is one machine, and listing it
// twice would have the scan repair the same entry twice.
#[test]
fn test_is_new_find_rejects_the_same_id_at_another_address() {
    let found = vec![service(Some("salon-id"), "http://192.168.1.107:4000")];
    assert!(!is_new_find(
        &found,
        &service(Some("salon-id"), "http://10.0.0.5:4000")
    ));
}

// Criterion: two genuinely different backends are both listed — including two
// that advertise no id at all, which then only differ by address.
#[test]
fn test_is_new_find_accepts_a_second_backend() {
    let found = vec![service(Some("salon-id"), "http://192.168.1.107:4000")];
    assert!(is_new_find(
        &found,
        &service(Some("bureau-id"), "http://192.168.1.42:4000")
    ));
    assert!(
        is_new_find(&found, &service(None, "http://192.168.1.42:4000")),
        "a service with no id is matched on its address alone"
    );
    let idless = vec![service(None, "http://192.168.1.107:4000")];
    assert!(
        is_new_find(&idless, &service(None, "http://192.168.1.42:4000")),
        "two id-less backends are told apart by their addresses"
    );
    assert!(is_new_find(&[], &service(None, "http://192.168.1.42:4000")));
}

// Criterion: the scan is bounded (~5 s) so it cannot hold the settings page.
#[test]
fn test_browse_timeout_is_bounded_and_short() {
    assert!(
        BROWSE_TIMEOUT >= Duration::from_secs(1) && BROWSE_TIMEOUT <= Duration::from_secs(10),
        "the browse must be bounded and short, got {BROWSE_TIMEOUT:?}"
    );
}

// Criterion (non-nominal): "no backend found" is a neutral state, never an
// error — the error type has no variant for it, so an empty scan can only be
// reported as `Ok(vec![])`.
#[test]
fn test_discovery_error_has_no_variant_for_an_empty_scan() {
    let unsupported = DiscoveryError::Unsupported.to_string();
    assert!(
        !unsupported.is_empty(),
        "the disabled case must explain itself in the error card"
    );
    assert_eq!(
        DiscoveryError::Browse("socket bind failed".to_string()).to_string(),
        "socket bind failed",
        "a browse failure must surface its own detail"
    );
}
