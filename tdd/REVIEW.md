# Review Report — Reflect real connection status on scan

## Issues Found & Fixed
- [clippy/android] src/bluetooth.rs:105 — `A2DP_PROXY_SLOT` used a deeply nested `Mutex<Option<Arc<(Mutex<Option<GlobalRef>>, Condvar)>>>` type that tripped `clippy::type_complexity` (error under `-D warnings` on the Android target) → extracted an `A2dpProxySlot` type alias and reused it in both the static and `obtain_a2dp_proxy`.
- [clippy/android] src/bluetooth.rs:139 — `let env = env;` redundant rebinding inside the JNI `onA2dpServiceConnected` callback tripped `clippy::redundant_locals` → removed the rebinding (kept the SAFETY comment).
- [clippy/android] src/bluetooth.rs:387-389 — three `drop()` calls on non-`Drop` JNI types (`JClass`, `JObjectArray`, `JObject`) in `build_service_listener_proxy` tripped `clippy::drop_non_drop` → removed the `drop()` calls and bound the unused-but-fallible JNI lookups with `_`-prefixed names so their `?` error propagation is preserved.
- [style] tests/bluetooth_integration.rs — three `assert!` calls exceeded the line width and failed `cargo fmt --check` (pre-existing from an earlier phase) → applied `rustfmt`.

## Review focus verification (no change needed)
- `connected_device_names_inner()` (Android) acquires the A2DP proxy **once** via `obtain_a2dp_proxy`, then calls `getConnectionState` in a loop on that single `a2dp_ref` — the expensive proxy acquisition is not repeated per device.
- The bonded-set iteration maps a null `getName()` to `String::new()`, consistent with `scan_devices_inner` — no panic path.
- The `main.rs` scan handler uses `scan_devices().await.unwrap_or_default()` and `connected_device_names().await.unwrap_or_default()` inside an async `onclick` closure, so a JNI error degrades gracefully and the UI thread is not blocked.
- `merge_connection_status` has no unnecessary clones/allocations (consumes `found` by value, borrows `connected` as `&[String]`); the O(n²) dedup lookup is acceptable for the small device lists involved.
- Left `.tdd-base-sha` untouched and restored the dx-regenerated `assets/tailwind.css` (not committed).

## New Tests Added
- none (the existing 38 tests already cover the acceptance criteria; all fixes were behavior-preserving lint/format changes).

## Final Status
- `cargo test`: ✅ 38 passed (22 lib + 16 integration)
- `cargo clippy`: ✅ clean (host target and `aarch64-linux-android` target)
- `dx build --platform android`: ✅ success (exit 0)
