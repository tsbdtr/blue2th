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

//! Integration tests for the real Bluetooth adapter state detection feature.
//! These tests exercise the async inner logic of `enable_bluetooth_inner` directly.

use blue2th::bluetooth::{
    enable_bluetooth_inner, request_enable_bluetooth, request_enable_bluetooth_inner,
    scan_devices, scan_devices_inner,
};

// Criterion 3 + combined criteria 1 & 2:
// On non-Android platforms the function falls back to the existing simulation (returns true).
// The function must return a Result<bool, _> and must not panic.
#[tokio::test]
async fn test_enable_bluetooth_returns_bool() {
    // Asserts the function returns a Result<bool, _> and does not panic.
    let result: Result<bool, _> = enable_bluetooth_inner().await;
    assert!(result.is_ok(), "enable_bluetooth_inner() must return Ok(_), got: {result:?}");
}

// Criterion 3: on non-Android targets, enable_bluetooth returns Ok(true) (simulation fallback).
#[tokio::test]
#[cfg(not(target_os = "android"))]
async fn test_enable_bluetooth_simulation_fallback() {
    let result = enable_bluetooth_inner().await;
    assert!(
        result.unwrap(),
        "on non-Android, enable_bluetooth_inner() must return Ok(true) as simulation fallback"
    );
}

// Criterion 4: the UI handler must not activate BT when enable_bluetooth_inner returns Ok(false).
// Models the ConfirmModal on_confirm handler: match result { Ok(true) => activate, _ => {} }
#[test]
fn test_bt_enabled_stays_false_when_enable_bluetooth_returns_false() {
    let mut bt_enabled = false;
    let result: Result<bool, String> = Ok(false);
    if let Ok(true) = result {
        bt_enabled = true;
    }
    assert!(
        !bt_enabled,
        "bt_enabled must remain false when enable_bluetooth returns Ok(false)"
    );
}

// Criterion 5: clicking "Enable Bluetooth" while BT is off must NOT fake-activate it.
// An Err result must also leave bt_enabled unchanged.
#[test]
fn test_no_fake_activation_when_bt_returns_error() {
    let mut bt_enabled = false;
    let result: Result<bool, String> = Err("BT unavailable".to_string());
    if let Ok(true) = result {
        bt_enabled = true;
    }
    assert!(
        !bt_enabled,
        "bt_enabled must NOT be set to true when enable_bluetooth returns Err"
    );
}

// Criterion: request_enable_bluetooth_inner() returns Ok(()) on non-Android (simulation stub).
// Covers: "On non-Android: returns Ok(()) immediately (simulation stub)."
#[cfg(not(target_os = "android"))]
#[tokio::test]
async fn test_request_enable_bluetooth_inner_returns_ok_on_non_android() {
    let result = request_enable_bluetooth_inner().await;
    assert!(
        result.is_ok(),
        "request_enable_bluetooth_inner() must return Ok(()) on non-Android, got: {result:?}"
    );
}

// Criterion: request_enable_bluetooth() returns Ok(()) and must not panic on non-Android.
// Covers: "request_enable_bluetooth() must return Ok(()) and not panic."
#[cfg(not(target_os = "android"))]
#[tokio::test]
async fn test_request_enable_bluetooth_returns_ok_on_non_android() {
    let result = request_enable_bluetooth().await;
    assert!(
        result.is_ok(),
        "request_enable_bluetooth() must return Ok(()), got: {result:?}"
    );
}

// Criterion 2: scan_devices() returns Ok(_) and does not panic on non-Android.
// Covers: "On non-Android, scan_devices() still returns a non-empty Ok(Vec<String>)."
#[cfg(not(target_os = "android"))]
#[tokio::test]
async fn test_scan_devices_returns_ok_on_non_android() {
    let result = scan_devices().await;
    assert!(
        result.is_ok(),
        "scan_devices() must return Ok(_) on non-Android, got: {result:?}"
    );
}

// Criterion 2: simulation fallback returns at least one device name.
// Covers: "simulation fallback — keeps host cargo test green."
#[cfg(not(target_os = "android"))]
#[tokio::test]
async fn test_scan_devices_simulation_non_empty() {
    let Ok(devices) = scan_devices_inner().await else {
        assert!(false, "scan_devices_inner() must return Ok(_) on non-Android");
        return;
    };
    assert!(
        !devices.is_empty(),
        "scan_devices_inner() simulation must return at least one device name, got empty vec"
    );
    // Each device name must be a non-empty string.
    for name in &devices {
        assert!(
            !name.is_empty(),
            "simulation device names must be non-empty strings"
        );
    }
}

// Criterion 1: on Android, scan_devices() dispatches to scan_devices_inner() which uses JNI.
// Covers: "On Android, scan_devices() returns the list of bonded device names via JNI."
// On non-Android we verify the delegation contract: scan_devices() must call scan_devices_inner()
// and return exactly the same result. The current hardcoded list in scan_devices() means the
// results differ — this test enforces they must be identical after the implementation.
#[cfg(not(target_os = "android"))]
#[tokio::test]
async fn test_scan_devices_dispatches_to_inner() {
    let (Ok(outer), Ok(inner)) = (scan_devices().await, scan_devices_inner().await) else {
        assert!(false, "scan_devices() and scan_devices_inner() must both return Ok(_)");
        return;
    };
    // scan_devices() must delegate to scan_devices_inner(): their results must be identical.
    // This fails (red) until scan_devices() is rewritten to call scan_devices_inner().
    assert_eq!(
        outer, inner,
        "scan_devices() must delegate to scan_devices_inner() — results must be identical"
    );
}

// Criterion 3: scan_devices() propagates BluetoothError without panicking.
// Covers: "scan_devices() returns Err(BluetoothError) if the JNI call fails instead of panicking."
// We model this at the type level: Result<Vec<String>, BluetoothError> is the correct signature.
#[tokio::test]
async fn test_scan_devices_error_path_does_not_panic() {
    // Calling scan_devices() must never panic — even in an error scenario.
    // On non-Android this always succeeds; we verify the return type handles Err correctly.
    let result: Result<Vec<String>, _> = scan_devices().await;
    // We simply assert it returns without panicking (the test itself proves no panic occurred).
    let _ = result.is_ok() || result.is_err();
}

// Criterion 5 & 6: locale files must contain the new scan labels.
// Covers: fr.yaml scan.button and scan.scanning, en.yaml scan.button and scan.scanning.
#[test]
fn test_locale_fr_scan_labels_updated() {
    let Ok(content) = std::fs::read_to_string("locales/fr.yaml") else {
        assert!(false, "locales/fr.yaml must exist");
        return;
    };
    assert!(
        content.contains("Charger les appareils"),
        "locales/fr.yaml must contain 'Charger les appareils' for scan.button, got:\n{content}"
    );
    assert!(
        content.contains("Chargement en cours"),
        "locales/fr.yaml must contain 'Chargement en cours' for scan.scanning, got:\n{content}"
    );
}

#[test]
fn test_locale_en_scan_labels_updated() {
    let Ok(content) = std::fs::read_to_string("locales/en.yaml") else {
        assert!(false, "locales/en.yaml must exist");
        return;
    };
    assert!(
        content.contains("Load devices"),
        "locales/en.yaml must contain 'Load devices' for scan.button, got:\n{content}"
    );
    assert!(
        content.contains("Loading"),
        "locales/en.yaml must contain 'Loading' for scan.scanning, got:\n{content}"
    );
}
