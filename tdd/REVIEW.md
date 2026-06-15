# Review Report — Real A2DP connect/disconnect via embedded ServiceListener `.dex`

## Issues Found & Fixed
- [naming/clarity] src/bluetooth.rs:222-229 — the `obtain_a2dp_proxy` doc comment still described the *old* mechanism (a `java.lang.reflect.Proxy` whose `InvocationHandler` calls a removed native `onA2dpServiceConnected`). → Rewrote it to describe the new DexClassLoader-loaded `dev.dioxus.main.A2dpServiceListener` whose `onServiceConnected` invokes `nativeOnServiceConnected`. No behavior change.
- [spurious change] assets/tailwind.css — the diff added an unrelated generated `.inline` utility class (not referenced anywhere in `src/`). → Reverted to the base revision via `git checkout <base> -- assets/tailwind.css`, as permitted by the review instructions. This generated file was not regenerated.

## Items Reviewed — No Change Needed
- No `unwrap`/`expect`/`panic`/`todo`/`unreachable`/`unimplemented` outside `#[cfg(test)]`. All fallible JNI calls propagate via `?` and route exceptions through `bt_err_clear`.
- `build_service_listener_proxy`: error propagation correct; `loadClass`/`newInstance` null results are checked and return `BluetoothError` (no more `null` return). The renamed JNI exports (`Java_dev_dioxus_main_A2dpServiceListener_nativeOnServiceConnected` / `_nativeOnServiceDisconnected`) store the proxy in `A2DP_PROXY_SLOT` and signal the condvar.
- Dex write robustness: written to the app-private code-cache dir (`getCodeCacheDir()`); `FileOutputStream(String)` truncates+overwrites on every call, so a stale/partial prior write cannot poison a later load.
- Local-reference hygiene: `build_service_listener_proxy` allocates ~12 local refs and runs once per connect/disconnect (not in a loop), well under the default JNI local-ref table; consistent with the existing convention of only deleting refs inside iteration loops (`scan_devices_inner`, `connected_device_names_inner`, `device_is_connected_reflect`). No leak risk.
- `java/A2dpServiceListener.java`: package `dev.dioxus.main`, `implements BluetoothProfile.ServiceListener`, callbacks delegate to `private static native` methods with matching signatures. OK.
- `java/build-dex.sh`: `set -euo pipefail`, overridable `JAVAC`/`D8`/`ANDROID_JAR`, script-relative path resolution, temp dir with `trap` cleanup. Robust. d8/javac NOT re-run (committed dex is the artifact).

## New Tests Added
- none (artifact-contract + non-Android regression tests already cover the testable surface; the Android JNI path is validated on-device).

## Final Status
- `cargo test`: ✅ all suites pass (17 + 23 + 10 + 16 + 0 doc)
- `cargo clippy`: ✅ clean (host target and `aarch64-linux-android` target)
- `dx build --platform android`: ✅ success (exit 0)
