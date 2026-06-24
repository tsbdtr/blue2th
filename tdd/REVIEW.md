# Review Report — Fan-out playback to two speakers

## Affected Layers
mobile, server, proto

## Issues Found & Fixed
- [build artifact] `assets/tailwind.css` — 88-line diff was a regenerated `dx build` Tailwind output (new utility classes `.absolute`, `.grow`, `.transition`, etc.) unrelated to the fan-out feature, which touches no UI markup. Reverted to base (`git checkout a942b3f -- assets/tailwind.css`).
- [duplication] `blue2th-server/src/audio.rs:444` — `bluetooth_sink_for` inlined `format!("bluez_output.{}", mac.to_uppercase().replace(':', "_"))`, which is exactly what the feature's new `bluez_sink_prefix` produces. Replaced the inline expression with a call to `bluez_sink_prefix(mac)` so the BlueZ node-name derivation lives in one place (the prefix used to match a live sink is now guaranteed identical to the one put in the combined-sink plan).
- [docs accuracy] `blue2th-server/src/audio.rs` — `CombineBranch::sink` doc claimed the value is a full node name (`bluez_output.AA_..._CC.1`), but `combine_sink_plan` stores the prefix without the trailing card suffix. Corrected the doc to state it is the `bluez_output.*` prefix that the hardware seam resolves to the live node. Also linked `bluetooth_sink_for`'s doc to `bluez_sink_prefix`.

## New Tests Added
- `test_set_offset_on_unselected_address_is_noop`: setting an offset on an address that is not selected inserts no phantom entry and leaves existing offsets untouched (covers the documented no-op branch of `set_offset`).
- `test_reselect_is_idempotent_even_if_no_longer_connected`: re-selecting an already-selected speaker returns `Ok` even when it is absent from the `connected` slice and preserves its current offset (covers the early idempotent return before the connectivity check).
- `test_deselect_unselected_address_is_noop`: deselecting an address that was never selected leaves the rest of the selection intact.

## Notes
- The new `targets` module is pure, I/O-free logic with full unit coverage; the PipeWire combined-sink I/O stays behind the untested `route_to_speaker` / sink seams, consistent with the project's hardware-boundary convention. Proto stays target-agnostic (serde + serde_json dev-dep only).
- The new mobile client fns (`select_target`, `deselect_target`, `set_offset`, `fetch_targets` and their URL builders) are not yet called from the UI, so the Android build emits `never used` INFO warnings — identical to the existing pattern for the phase-3 transport client fns and gated by the same `#[cfg_attr(not(target_os = "android"), allow(dead_code))]`. Wiring them into the UI would be adding behavior beyond this feature's tested scope, so they were left as-is. The Android build still completes successfully and clippy `-D warnings` is clean.

## Final Status
- `cargo test --workspace`: ✅ all passed (server unit incl. 3 new targets edge-case tests; proto, mobile, transport integration all green; 1 PipeWire-gated test ignored as designed)
- `cargo clippy --workspace --all-targets -- -D warnings ...`: ✅ clean
- `cargo build --workspace`: ✅ success
- `dx build --platform android`: ✅ success (client build completed; non-blocking `never used` INFO warnings on unwired client fns)
