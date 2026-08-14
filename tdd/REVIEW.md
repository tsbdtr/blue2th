# Review Report — Phase 6.1 — Remember each speaker's sync offset across restarts

## Affected Layers
server (`blue2th-server`) — `blue2th-proto` untouched, mobile untouched.

## Issues Found & Fixed
- [duplication] `blue2th-server/src/targets.rs:33` — `offsets_store_path()` duplicated
  verbatim the XDG resolution of `spotify_auth::token_store_path()` (blank-value
  filtering, `HOME` fallback, `blue2th/` scoping) → extracted into a new
  `blue2th-server/src/state_store.rs` (`state_store_path(file)`), now used by both
  stores, so the state-directory rules live in exactly one place.
- [clippy] `blue2th-server/tests/offsets.rs:33` — the GREEN phase silenced
  `clippy::expect_used` on the `targets_state` helper with an `#[allow]`. The lint
  was indeed firing (`allow-expect-in-tests` does not cover a free helper in an
  integration-test binary), but the `#[allow]` was not the right answer: the helper
  now returns `Result<TargetsState, String>` and the two `#[tokio::test]` functions
  assert with `expect`, which clippy allows. The `#[allow]` is gone and a decode
  failure now reports which step failed.
- [test isolation] `blue2th-server/src/targets.rs:496` —
  `test_offsets_store_path_is_app_scoped_and_honours_xdg_state_home` asserted while
  `XDG_STATE_HOME`/`HOME` were still overridden; a failing assertion would panic
  before the restore block and leave the *whole* lib test binary running with a
  bogus `HOME`, cascading unrelated failures. It now resolves the three paths,
  restores the process env, and only then asserts.
- [edge case] `save_offsets` never exercised its `create_dir_all` branch — every
  test seeded an existing directory, so the real first-run case (`~/.local/state/blue2th`
  does not exist yet) was untested → added
  `test_save_offsets_creates_the_missing_store_directory`.
- [edge case] nothing checked that persisting one speaker keeps the other speakers'
  remembered offsets on disk (a plausible regression if `persist` ever wrote only the
  current selection) → added `test_set_offset_keeps_the_remembered_offsets_of_other_speakers`.
- [edge case] the "blank `XDG_STATE_HOME` falls back to `HOME`" branch of the path
  resolution had no coverage → added a case to the single existing env-mutating test
  (deliberately not a second test: env mutation is process-wide and two mutating
  tests in one binary would race).

## Reviewed and deliberately left as-is
- **Test isolation of `app()`**: the phase-6.1 unit test builds the real `app()`, which
  loads the real `~/.local/state/blue2th/offsets.json` — same as the pre-existing health
  test and `tests/transport.rs`/`tests/spotify.rs`, which already load the real Spotify
  token store. It is read-only: a write only happens through `set_offset` on a *selected*
  speaker, which requires a connected BlueZ device. Verified empirically — a full
  `cargo test --workspace` neither creates `offsets.json` nor changes the token store's
  checksum. Every route test that could write (`tests/offsets.rs`) uses the store-free
  `app_with_auth`.
- **Best-effort I/O**: `set_target_offset` returns `Json<TargetsState>` unconditionally and
  `persist()` only logs a `tracing::warn!` — an unwritable store never turns
  `POST /devices/{addr}/offset` into an error. Covered by
  `test_set_offset_with_unwritable_store_still_applies_to_the_session`.
- **Blocking `fs::write` inside the async handler** (under the `tokio::Mutex`): a sub-kilobyte
  write per slider drag, and identical to the existing token-store pattern. Moving it to
  `spawn_blocking` would add real complexity for no measurable gain.
- **Non-atomic write / `HashMap` key ordering**: a torn write degrades to "nothing
  remembered", which the design already tolerates by contract, and the file is
  machine-written. Not worth diverging from the token-store pattern.

## New Tests Added
- `test_save_offsets_creates_the_missing_store_directory`: first run — the app-scoped
  state directory (nested, possibly missing state home) is created rather than erroring.
- `test_set_offset_keeps_the_remembered_offsets_of_other_speakers`: persisting speaker A's
  offset preserves B's and C's seeded values on disk.
- extended `test_offsets_store_path_is_app_scoped_and_honours_xdg_state_home`: a blank
  `XDG_STATE_HOME` falls back to `HOME`, not to the filesystem root.

## Final Status
- `cargo test --workspace`: ✅ 249 passed (0 failed, 1 ignored — the manual PipeWire test)
- `cargo clippy --workspace`: ✅ clean (with `-D warnings -W clippy::unwrap_used
  -W clippy::expect_used -W clippy::panic -W clippy::todo -W clippy::unreachable
  -W clippy::unimplemented`)
- `cargo fmt --check`: ✅ clean
- `cargo build --workspace`: ✅ exit 0
- `dx build --platform android`: ⏭️ skipped (mobile not affected)
