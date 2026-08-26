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

//! RED-phase tests for the "Real A2DP connect/disconnect via embedded
//! ServiceListener `.dex`" feature.
//!
//! Most of the runtime behaviour is Android-only (JNI / DexClassLoader) and is
//! validated manually on-device. On a non-Android host we can only assert the
//! *artifact contract* (the committed dex + its Java source + build script) and
//! the source-level rename of the JNI export, plus the non-Android regression
//! guards for connect/disconnect.

use std::path::Path;

use blue2th_frontend::bluetooth::{connect_device, disconnect_device};

// ── Artifact contract ────────────────────────────────────────────────────────

// AC: `assets/a2dp_listener.dex` exists and is committed.
// We use std::fs (not include_bytes!) so this file compiles now and FAILS (red)
// until the dex is committed, rather than breaking compilation.
#[test]
fn test_a2dp_listener_dex_exists() {
    let path = Path::new("assets/a2dp_listener.dex");
    assert!(
        path.exists(),
        "assets/a2dp_listener.dex must exist and be committed"
    );
}

// AC: `assets/a2dp_listener.dex` is non-empty.
#[test]
fn test_a2dp_listener_dex_non_empty() {
    let path = Path::new("assets/a2dp_listener.dex");
    let meta = std::fs::metadata(path);
    assert!(
        meta.is_ok(),
        "assets/a2dp_listener.dex must exist so its metadata can be read"
    );
    let len = meta.map(|m| m.len()).unwrap_or(0);
    assert!(
        len > 0,
        "assets/a2dp_listener.dex must be non-empty, got {len} bytes"
    );
}

// AC: the dex is produced from java/A2dpServiceListener.java by java/build-dex.sh.
#[test]
fn test_java_source_and_build_script_exist() {
    assert!(
        Path::new("java/A2dpServiceListener.java").exists(),
        "java/A2dpServiceListener.java must exist (the dex source)"
    );
    assert!(
        Path::new("java/build-dex.sh").exists(),
        "java/build-dex.sh must exist (produces assets/a2dp_listener.dex)"
    );
}

// AC: A2dpServiceListener implements BluetoothProfile.ServiceListener and its
// callbacks delegate to `private static native` methods.
#[test]
fn test_java_source_implements_service_listener_with_native_callbacks() {
    let src = std::fs::read_to_string("java/A2dpServiceListener.java");
    assert!(
        src.is_ok(),
        "java/A2dpServiceListener.java must be readable"
    );
    let content = src.unwrap_or_default();
    assert!(
        content.contains("class A2dpServiceListener"),
        "java source must declare class A2dpServiceListener"
    );
    assert!(
        content.contains("implements") && content.contains("BluetoothProfile.ServiceListener"),
        "A2dpServiceListener must implement BluetoothProfile.ServiceListener"
    );
    assert!(
        content.contains("private static native"),
        "callbacks must delegate to private static native methods"
    );
    assert!(
        content.contains("nativeOnServiceConnected"),
        "java source must declare the nativeOnServiceConnected native method"
    );
    assert!(
        content.contains("onServiceConnected"),
        "A2dpServiceListener must override onServiceConnected"
    );
}

// AC: the build script targets the committed asset path.
#[test]
fn test_build_dex_script_outputs_to_asset_path() {
    let script = std::fs::read_to_string("java/build-dex.sh");
    assert!(script.is_ok(), "java/build-dex.sh must be readable");
    let content = script.unwrap_or_default();
    assert!(
        content.contains("a2dp_listener.dex"),
        "build-dex.sh must produce a2dp_listener.dex"
    );
    assert!(
        content.contains("A2dpServiceListener"),
        "build-dex.sh must compile A2dpServiceListener"
    );
}

// ── JNI export rename (source-level contract) ────────────────────────────────

// AC: The Rust JNI export is renamed to match the listener's own native method
// `Java_..._A2dpServiceListener_nativeOnServiceConnected`, and still stores the
// proxy in A2DP_PROXY_SLOT + signals the condvar.
#[test]
fn test_jni_export_renamed_to_listener_native_method() {
    let src = std::fs::read_to_string("src/bluetooth.rs");
    assert!(src.is_ok(), "src/bluetooth.rs must be readable");
    let content = src.unwrap_or_default();
    assert!(
        content.contains("Java_")
            && content.contains("A2dpServiceListener_nativeOnServiceConnected"),
        "the JNI export must be named Java_..._A2dpServiceListener_nativeOnServiceConnected"
    );
    // The old export name must be gone.
    assert!(
        !content.contains("WryActivity_onA2dpServiceConnected"),
        "the old JNI export name WryActivity_onA2dpServiceConnected must be removed"
    );
    // The rendezvous mechanism must still be referenced from the export path.
    assert!(
        content.contains("A2DP_PROXY_SLOT"),
        "the JNI export must still store the proxy in A2DP_PROXY_SLOT"
    );
    assert!(
        content.contains("notify_all"),
        "the JNI export must still signal the condvar (notify_all)"
    );
}

// AC: build_service_listener_proxy loads the dex via DexClassLoader, instantiates
// A2dpServiceListener, and no longer returns JObject::null().
#[test]
fn test_build_service_listener_proxy_uses_dex_class_loader() {
    let src = std::fs::read_to_string("src/bluetooth.rs");
    assert!(src.is_ok(), "src/bluetooth.rs must be readable");
    let content = src.unwrap_or_default();
    assert!(
        content.contains("DexClassLoader") || content.contains("dalvik/system/DexClassLoader"),
        "build_service_listener_proxy must load the dex via DexClassLoader"
    );
    assert!(
        content.contains("A2dpServiceListener"),
        "build_service_listener_proxy must instantiate A2dpServiceListener"
    );
    assert!(
        content.contains("a2dp_listener.dex"),
        "build_service_listener_proxy must reference the embedded dex asset a2dp_listener.dex"
    );
}

// ── Non-Android regression guards (must stay GREEN) ──────────────────────────

// AC: Non-Android build — connect_device keeps returning the simulated Ok(true).
#[cfg(not(target_os = "android"))]
#[tokio::test]
async fn test_connect_device_simulated_ok_on_non_android() {
    let result = connect_device("Blue Speaker".to_string()).await;
    assert!(
        result.is_ok(),
        "connect_device() must return Ok(_) on non-Android, got: {result:?}"
    );
    assert!(
        result.unwrap_or(false),
        "connect_device() non-Android simulation must return Ok(true)"
    );
}

// AC: Non-Android build — disconnect_device keeps returning the simulated Ok(true).
#[cfg(not(target_os = "android"))]
#[tokio::test]
async fn test_disconnect_device_simulated_ok_on_non_android() {
    let result = disconnect_device("Blue Speaker".to_string()).await;
    assert!(
        result.is_ok(),
        "disconnect_device() must return Ok(_) on non-Android, got: {result:?}"
    );
    assert!(
        result.unwrap_or(false),
        "disconnect_device() non-Android simulation must return Ok(true)"
    );
}

// AC: connect/disconnect error paths never panic on the host (type-level guard).
#[cfg(not(target_os = "android"))]
#[tokio::test]
async fn test_connect_disconnect_do_not_panic() {
    let c: Result<bool, _> = connect_device("X".to_string()).await;
    let d: Result<bool, _> = disconnect_device("X".to_string()).await;
    // Reaching here proves neither call panicked.
    let _ = c.is_ok() || c.is_err();
    let _ = d.is_ok() || d.is_err();
}
