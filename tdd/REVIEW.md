# Review Report — Reflect real connection status on scan

## Issues Found & Fixed
- [duplication/dead-code] src/main.rs — the post-scan background reconcile and the 2 s polling reconcile duplicated the same inline status-mapping logic, while `merge_connection_status` stayed test-only. Extracted a pure `reconcile_connection_status(&mut [(String, ConnectionStatus)], &[String])` helper that updates status in both directions and always preserves `Connecting`, and wired it into both Android reconcile sites so they cannot drift. `merge_connection_status` is left in place (still tested; serves a different shape) with its existing doc/allow attribute.
- [edge-case / JNI hygiene] src/bluetooth.rs `connected_device_names_inner`, `scan_devices_inner`, `device_is_connected_reflect` — JNI local references created per bonded device (device, name string, plus ~6 reflection temporaries each) are not reclaimed until the native frame returns, so a user with many bonded devices could overflow the default local-reference table. Added explicit `delete_local_ref` for the per-iteration objects and for the reflection helper's intermediates. Errors are ignored intentionally (a failed delete is non-fatal).
- [error handling / robustness] src/bluetooth.rs `bt_err_clear` — if the `toString`/`get_string` retrieval path itself raised (e.g. OOM) after the initial clear, a fresh pending exception could be left on the thread and abort the next JNI call. Added a defensive second `exception_clear()` so the function can never return with a pending exception. No `unwrap`/`expect`/`panic` introduced.

## New Tests Added
- test_reconcile_connection_status_updates_both_directions: a listed device in the connected set becomes Connected, one absent becomes Disconnected.
- test_reconcile_connection_status_preserves_connecting: an in-flight `Connecting` entry is never clobbered, while a non-connecting device absent from the set becomes Disconnected.

## Final Status
- `cargo test`: ✅ 39 passed (17 + 6 unit + 16 integration; 0 doctests)
- `cargo clippy` (host): ✅ clean
- `cargo clippy` (aarch64-linux-android): ✅ clean
- `cargo fmt --check`: ✅ clean (apart from the known nightly-only `imports_granularity`/`group_imports` warnings)

Notes: `dx build --platform android` not run (out of scope / slow). `obtain_a2dp_proxy` / `build_service_listener_proxy` left untouched as instructed.
