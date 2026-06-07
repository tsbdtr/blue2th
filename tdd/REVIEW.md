# Review Report — Replace confirm modal with Android Bluetooth enable dialog

## Issues Found & Fixed

- [style] `src/main.rs:82` — `#[cfg_attr(target_os = "android", allow(unused_mut))]` was placed on the entire `Home` component function, silencing `unused_mut` for all local bindings; only `bt_enabled` needs the suppressor (its write path is `#[cfg(not(target_os = "android"))]`-gated) → moved the `cfg_attr` to the `let mut bt_enabled` binding and added an explanatory comment.

- [docs] `src/bluetooth.rs:64,101,106` — three `#[allow(dead_code)]` attributes on `enable_bluetooth_inner` (both cfg variants) and `enable_bluetooth` had no comment explaining why the suppression is necessary → added comments stating these functions are retained for the integration-test suite (lib target) but are unreachable from `main()` in the binary target after the `ConfirmModal` removal.

## New Tests Added

None — the existing 16 tests (5 unit via lib, 5 unit via bin, 6 integration) provide sufficient coverage for all acceptance criteria.

## Final Status

- `cargo test`: ✅ 16 passed (5 lib unit, 5 bin unit, 6 integration)
- `cargo clippy`: ✅ clean (host + aarch64-linux-android)
- `dx build --platform android`: ✅ success
