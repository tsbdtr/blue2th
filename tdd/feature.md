# TDD Feature Specification

> **Instructions**: Describe the feature to Claude (in conversation) and it will fill this file.
> Then run: `/tdd all`

---

## Feature Name
Phase 5.1 — Spotify source backend (librespot as a Spotify Connect device)

## Description
Make the PC a Spotify Connect device so a Premium user can stream Spotify to the two
speakers already wired up in phase 4. The server manages a `librespot` **subprocess**
(spawned via `Command`, not the crate) whose PipeWire output feeds the existing audio
graph: for two targets it points at the `blue2th_combined` null sink, for one target at
that speaker's `bluez_output.*` sink. The whole downstream fan-out + per-speaker offset
logic is reused unchanged. This slice covers **activating/deactivating the backend** and
surfacing its state; actual transport (play/pause/skip/now-playing) is driven by the
official Spotify app in 5.1 and moves into our own OAuth + Web API in slice 5.2.

## Nominal Scenario
1. At least one speaker is selected as a playback target (combined sink available for two).
2. In the blue2th app, the user taps "Start Spotify backend".
3. The mobile app calls `POST /spotify/start`; the server establishes routing (reusing
   `route_to_speaker` / `route_to_combined`), then spawns `librespot` as a Spotify Connect
   device named `blue2th-PC`, output targeting the correct sink.
4. The app shows state **"Spotify backend running"**.
5. The user opens the official Spotify app, picks **blue2th-PC** in the devices list and
   starts a track → audio plays on both speakers, tunable via the existing offsets.
6. Tapping "Stop Spotify backend" calls `POST /spotify/stop`; the server kills the
   subprocess and the app shows **"Spotify backend stopped"**.

## Non-nominal Scenarios
- **librespot binary missing** — spawn fails with `io::ErrorKind::NotFound`; the server maps
  it to a typed error and returns a 500 with a clear message ("Spotify backend unavailable
  (librespot not found)"). State stays `Stopped`. The mobile app surfaces the message.
- **No speaker selected** — `POST /spotify/start` is rejected with **400** ("Select a speaker
  first") before any spawn; state stays `Stopped`.
- **Start while already running** — idempotent: the server does not spawn a second process
  and returns the current `Running` state.
- **Stop while already stopped** — idempotent: no-op, returns `Stopped`.
- **Subprocess dies after start** (Spotify protocol / network) — the server detects the exited
  child, resets state to `Stopped`; the next `GET /spotify/status` reports `Stopped`.
- **Other spawn failure** (permission, exec error) — mapped to a 500 with the OS message;
  state stays `Stopped`.

## Acceptance Criteria
- [ ] `SpotifyState` DTO round-trips through serde (proto).
- [ ] `build_librespot_args(device_name, sink_name)` yields the expected argv
      (`--name blue2th-PC`, `--backend pulseaudio`, device/sink pointing at `sink_name`).
- [ ] `spotify_target_sink(&speakers)` returns `blue2th_combined` for two targets and the
      speaker's `bluez_output.*` prefix for one.
- [ ] A `NotFound` spawn error maps to the `BackendMissing` variant (→ 500, clear message).
- [ ] `POST /spotify/start` with no target selected returns **400** and does not spawn.
- [ ] `GET /spotify/status` on a fresh server returns `Stopped`.
- [ ] Starting while `Running` is idempotent (decision helper does not re-spawn).
- [ ] Server compiles with the three new routes wired; mobile exposes start/stop/status
      calls and an activation control that surfaces errors via `Signal<Option<String>>`.

## Layers touched
<!-- Source of truth for which crates the TDD agents build/test. Check all that apply. -->
<!-- mobile = blue2th (Dioxus/Android, src/) · server = blue2th-server (Axum/PipeWire) · proto = blue2th-proto (shared serde DTOs) -->
- [x] mobile (`blue2th` — `src/`, `tests/`, `assets/`, `locales/`)
- [x] server (`blue2th-server/`)
- [x] proto (`blue2th-proto/`)

## Technical Scope

### Files to modify
- **proto**: `blue2th-proto/src/lib.rs` — add `SpotifyStatus` enum + `SpotifyState` DTO (+ serde round-trip test).
- **server**: `blue2th-server/src/lib.rs` — add `spotify` module; extend `AppState` with a
  `spotify: Arc<Mutex<SpotifyBackend>>`; add routes `/spotify/start`, `/spotify/stop`,
  `/spotify/status`; add `From<SpotifyError> for AppError`.
- **mobile**: `src/backend.rs` — add `start_spotify`, `stop_spotify`, `spotify_status` reqwest calls.
- **mobile**: `src/lib.rs` (+ relevant UI component / `locales/`) — activation control, state display, error surfacing.

### Files to create
- **server**: `blue2th-server/src/spotify.rs` — `SpotifyBackend` (subprocess lifecycle),
  `SpotifyError`, pure helpers `build_librespot_args`, `spotify_target_sink`, spawn-error mapping.
- **server**: `blue2th-server/tests/spotify.rs` — route tests (no-target rejection, initial status).

### API / functions needed
- **proto**: `enum SpotifyStatus { Stopped, Running }`, `struct SpotifyState { status, device_name }`.
- **server** `spotify.rs`:
  - `fn build_librespot_args(device_name: &str, sink_name: &str) -> Vec<String>` (pure).
  - `fn spotify_target_sink(speakers: &[SpeakerTarget]) -> String` (pure; combined vs single).
  - `fn map_spawn_error(err: std::io::Error) -> SpotifyError` (pure; NotFound → BackendMissing).
  - `struct SpotifyBackend { child: Option<Child>, device_name: String }` with
    `status()`, `start(speakers)` (establish routing + spawn), `stop()`, `poll_liveness()`.
  - `enum SpotifyError { NoSpeakerSelected, BackendMissing, Spawn(String) }` + `Display`.
- **server** `lib.rs` handlers: `spotify_start`, `spotify_stop`, `spotify_status`
  returning `Result<Json<SpotifyState>, AppError>`.
- **mobile** `backend.rs`: `async fn start_spotify(&self) -> Result<SpotifyState, BackendError>` etc.

## Test Strategy

### Unit tests (synchronous, pure logic)
- proto: `SpotifyState` serde round-trip (serialize → deserialize equals original).
- `build_librespot_args` includes `--name blue2th-PC`, `--backend pulseaudio`, and the sink name.
- `spotify_target_sink`: two targets → `blue2th_combined`; one target → its `bluez_output.*` prefix.
- `map_spawn_error`: `ErrorKind::NotFound` → `SpotifyError::BackendMissing`; other kinds → `Spawn`.
- `SpotifyError → AppError`: `NoSpeakerSelected` → 400, `BackendMissing`/`Spawn` → 500.
- Idempotence decision helper: "already running ⇒ do not spawn".

### Integration tests (async)
<!-- server: blue2th-server/tests/ via tower oneshot. Hardware/external (librespot, PipeWire, Spotify) is NOT test-runnable. -->
- `POST /spotify/start` with an empty target selection → **400**, and no process is spawned.
- `GET /spotify/status` on a fresh router → `Stopped`.
- (Manual, not CI) actual spawn of librespot, appearance in the Spotify app, audio on two
  speakers, kill on stop, crash-detection — the external/hardware seam.

## Constraints & Notes
<!-- Technical constraints, edge cases to handle -->
- Must pass `cargo test --workspace`
- Must pass `cargo clippy --workspace --all-targets -- -D warnings -W clippy::unwrap_used -W clippy::expect_used -W clippy::panic -W clippy::todo -W clippy::unreachable -W clippy::unimplemented`
- If the **mobile** layer is touched: must pass `dx build --platform android`
- Mobile code follows Dioxus 0.7 patterns (no cx/Scope/use_state); `blue2th-proto` stays target-agnostic
- `librespot` is an **external runtime dependency** (a binary on `PATH`), not a cargo dep — the
  crate is never linked (unstable lib API, heavy build). Interaction is argv + process lifecycle.
- Requires Spotify **Premium**; librespot is unofficial and may break on Spotify updates.
- The combined sink only exists for two targets; `/spotify/start` must establish routing first.
- No `unwrap`/`expect`/`panic` outside `#[cfg(test)]`; the dead subprocess must never poison the server.
- Transport control + now-playing metadata + OAuth PKCE are **out of scope** here (slice 5.2).
