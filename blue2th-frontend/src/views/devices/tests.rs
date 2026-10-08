// SPDX-License-Identifier: MIT OR Apache-2.0

use super::{marks_unavailable, signal_bars, sort_scanned};
use crate::backend::BackendError;

/// A scanned (disconnected) device. Built by hand: a fixture must not lean
/// on the code under test.
fn scanned(address: &str, paired: bool, rssi: Option<i16>) -> blue2th_proto::DeviceInfo {
    blue2th_proto::DeviceInfo {
        address: address.to_string(),
        name: None,
        paired,
        connected: false,
        rssi,
    }
}

/// The list as the user reads it, top to bottom.
fn order(devices: &[blue2th_proto::DeviceInfo]) -> Vec<&str> {
    devices.iter().map(|d| d.address.as_str()).collect()
}

// AC: RSSI maps to a bar count (1..=4) and a colour, stronger = more bars/greener.
#[test]
fn test_signal_bars_strong_signal_is_full_and_green() {
    assert_eq!(signal_bars(-40), (4, "#22c55e"));
}

#[test]
fn test_signal_bars_medium_signal_is_orange() {
    let (bars, color) = signal_bars(-72);
    assert_eq!(bars, 2);
    assert_eq!(color, "#f59e0b");
}

#[test]
fn test_signal_bars_weak_signal_is_one_bar_and_red() {
    assert_eq!(signal_bars(-95), (1, "#ef4444"));
}

#[test]
fn test_signal_bars_is_monotonic_in_strength() {
    // Weaker signal never yields more bars than a stronger one.
    assert!(signal_bars(-90).0 <= signal_bars(-50).0);
}

// AC: a favourite — a device the backend is already bonded with — is listed
// above every stranger, however much stronger the stranger's signal. This is
// the whole point: in a crowded place the user's own speaker was buried.
#[test]
fn test_sort_scanned_puts_favourites_above_a_stronger_stranger() {
    let mut devices = vec![
        scanned("STRANGER", false, Some(-35)),
        scanned("MINE", true, Some(-90)),
    ];

    sort_scanned(&mut devices);

    assert_eq!(
        order(&devices),
        vec!["MINE", "STRANGER"],
        "pairing must outrank signal strength"
    );
}

// AC: signal strength still orders each group, unknown RSSI last.
#[test]
fn test_sort_scanned_orders_within_each_group_by_signal() {
    let mut devices = vec![
        scanned("KNOWN_WEAK", true, Some(-88)),
        scanned("NEW_UNKNOWN_RSSI", false, None),
        scanned("NEW_STRONG", false, Some(-40)),
        scanned("KNOWN_UNKNOWN_RSSI", true, None),
        scanned("KNOWN_STRONG", true, Some(-45)),
    ];

    sort_scanned(&mut devices);

    assert_eq!(
        order(&devices),
        vec![
            "KNOWN_STRONG",
            "KNOWN_WEAK",
            "KNOWN_UNKNOWN_RSSI",
            "NEW_STRONG",
            "NEW_UNKNOWN_RSSI",
        ],
        "favourites first, each group strongest first with an unknown RSSI last"
    );
}

// AC: two devices the sort cannot tell apart keep the order the scan found
// them in — the list must not shuffle under the user on every poll.
#[test]
fn test_sort_scanned_is_stable_for_devices_it_cannot_tell_apart() {
    let mut devices = vec![
        scanned("FIRST", true, Some(-60)),
        scanned("SECOND", true, Some(-60)),
    ];

    sort_scanned(&mut devices);

    assert_eq!(order(&devices), vec!["FIRST", "SECOND"]);
}

// ---- #52: a refused Bluetooth pairing must not grey the row ----

// AC: a `BackendError` flagged as a pairing failure does not add the address
// to the `unavailable` set — the speaker was not in pairing mode, so the row
// stays clickable for a retry.
#[test]
fn test_marks_unavailable_is_false_for_a_pairing_failure() {
    assert!(
        !marks_unavailable(&BackendError::pairing_failed()),
        "a refused pairing says nothing about the speaker being reachable"
    );
}

// AC: any other connect failure still marks the address unavailable — a
// paired speaker that will not connect is the one case where the hardware
// really is the suspect. This is the behaviour the fix must not regress.
#[test]
fn test_marks_unavailable_is_true_for_a_plain_backend_error() {
    let err = BackendError::protocol(blue2th_proto::ProtocolMismatch::BackendTooOld);
    assert!(
        marks_unavailable(&err),
        "every non-pairing failure keeps greying the row"
    );
}

// AC: an unpaired *backend* (401) is not a Bluetooth pairing failure, so the
// two flags must not be conflated into one rule.
#[test]
fn test_marks_unavailable_is_true_for_an_unpaired_backend() {
    assert!(
        marks_unavailable(&BackendError::not_paired()),
        "app-to-backend pairing is a different failure from speaker pairing"
    );
}
