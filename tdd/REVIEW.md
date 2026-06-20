# Review Report — Play a local audio file to one connected speaker (Phase 3 — PipeWire transport)

## Issues Found & Fixed
- [edge case] blue2th-server/src/audio.rs:20 — `clamp_volume` used `f32::clamp`, which propagates `NaN` unchanged (documented semantics). A `NaN` `level` from `POST /volume` would be stored as the sink volume and then serialized by `serde_json` as JSON `null`, breaking the `PlaybackState` round-trip contract and producing an invalid response body. Fixed by treating `NaN` as `0.0` (silence) before clamping, so the `0.0..=1.0` guarantee always holds.

## Items Reviewed — No Change Needed
- Error handling: no `unwrap`/`expect`/`panic` outside `#[cfg(test)]`. `run()` uses `unwrap_or_else` (not `unwrap`); the SSE handler degrades a serialization failure into a comment event instead of panicking. Clean.
- `AppError` correctly maps `AudioError::NoSpeakerConnected` -> 400 and decode/PipeWire -> 500; `/play` rejects with 4xx when no speaker is connected, satisfying that acceptance criterion without panicking.
- Host-gated audio seams (`start_output`/`resume_output`/`pause_output`/`stop_output`/`apply_sink_volume`) are side-effect-free no-ops as required; the gated `#[ignore]` PipeWire hardware test was left untouched.
- `/volume` behaviour intentionally left as clamp + state only (no connected-speaker precondition): matches the existing tests, and adding a precondition would be an unmotivated behaviour change (rule 9).
- State-machine transitions (`play`/`pause`/`stop`) are idempotent as specified; pause/stop while stopped return 200 with unchanged state.
- Cargo.toml: `rodio` correctly `default-features = false, features = ["wav"]` to avoid the cpal/ALSA build dependency on this host.

## New Tests Added
- `test_clamp_volume_nan_saturates_to_zero` (blue2th-server/src/audio.rs): asserts a `NaN` level clamps to `0.0` rather than propagating, covering the round-trip/serialization regression.

## Final Status
- `cargo test`: ✅ all passed (1 gated hardware test still `#[ignore]`)
- `cargo clippy`: ✅ clean (project command, workspace)
- `dx build --platform android`: N/A (backend-only phase)
