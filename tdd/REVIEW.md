# Review Report — Phase 5.1 Spotify source backend (librespot)

> The dedicated `tdd-reviewer` agent crashed mid-run (connection closed after ~80 min)
> without writing its report or applying changes. This review was completed by the
> orchestrator directly.

## Affected Layers
mobile, server, proto

## Issues Found & Fixed
- [build artifact] `assets/tailwind.css` — an 87-line diff (`.absolute`, `.grow`,
  `.transition`, `--default-transition-*`) was a regenerated `dx build` Tailwind output.
  Phase 5.1 touches no UI markup (backend + reqwest call layer only), so the change is
  unrelated drift. Reverted to base — identical handling to the phase-4 review.

No code issues found. The implementation already:
- Reuses `audio::bluez_sink_prefix` (single-target sink) and the `blue2th_combined`
  convention (two-target sink) instead of re-deriving node names.
- Reuses `route_to_speaker` / `route_to_combined` for routing — no duplicated PipeWire logic.
- Mirrors `From<AudioError> for AppError` in `From<SpotifyError> for AppError`
  (precondition → 400, backend fault → 500).
- Snapshots the target selection then locks the backend, matching the `/play` handler pattern.

## Correctness Review (SpotifyBackend lifecycle)
- **start**: rejects an empty selection (`NoSpeakerSelected`) before any work; calls
  `poll_liveness` first so a self-exited child never blocks a respawn; honours idempotence
  via `should_spawn`; establishes routing, then spawns `librespot` mapping spawn errors with
  `map_spawn_error`. ✅
- **stop**: best-effort `kill` + `wait`, idempotent while stopped (`child.take()`). ✅
- **poll_liveness**: reconciles an exited/errored child back to `Stopped` — a dead
  subprocess never poisons the server. ✅
- No `unwrap`/`expect`/`panic`/`todo!` outside `#[cfg(test)]`; errors propagate via typed
  `SpotifyError` + `?`. ✅

## Minor Notes (not blocking)
- The `[] =>` arm inside `start`'s routing `match` is unreachable in practice (the
  `speakers.is_empty()` guard returns earlier) but is a plain `return Err(..)`, not
  `unreachable!()`, so it is clippy-clean and harmless — a defensive belt-and-braces.
- The new mobile client fns (`start_spotify`/`stop_spotify`/`spotify_status`/`spotify_url`)
  are the in-scope reqwest layer; they are not yet wired into a UI component, so the Android
  build emits non-blocking `never used` INFO warnings under the existing
  `#[cfg_attr(not(target_os = "android"), allow(dead_code))]` guard — same pattern as the
  phase-3/4 client fns. UI wiring is deliberately out of this slice's tested scope.

## Final Status
- `cargo test --workspace`: ✅ all passed (proto serde round-trip; server unit incl. spotify
  pure helpers + error mapping; spotify route tests; mobile URL tests; transport integration;
  1 PipeWire-gated test ignored as designed)
- `cargo clippy --workspace --all-targets -- -D warnings -W clippy::unwrap_used ...`: ✅ clean
- `cargo fmt --check`: ✅ clean
- `dx build --platform android`: ✅ success in the GREEN phase (mobile layer built);
  no source changed in review beyond reverting the CSS artifact.

## Manual (hardware/process) seam — not CI-testable
Left to a live setup with `librespot` installed:
- Actual spawn/kill of the real binary, appearance as `blue2th-PC` in the official Spotify app,
  audio on both speakers via the combined sink, per-speaker offset tuning, crash detection.
- In the current sandbox `librespot` is **not installed**, so `POST /spotify/start` with a
  selected speaker exercises the `BackendMissing` path (clean 500) — a valid first manual check.
