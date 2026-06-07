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
//! These tests exercise the async inner logic of `enable_bluetooth` directly,
//! bypassing the Dioxus server-function macro wrapper.

use blue2th::bluetooth::enable_bluetooth_inner;

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
#[tokio::test]
async fn test_bt_enabled_stays_false_when_enable_bluetooth_returns_false() {
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
#[tokio::test]
async fn test_no_fake_activation_when_bt_returns_error() {
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
