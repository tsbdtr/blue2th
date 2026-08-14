# Review Report — Phase 6.2 — Settings page: manage backends and their name

## Affected Layers
mobile, server, proto (proto reviewed only: no change needed — it stayed
target-agnostic, serde-only, and the shared name rule is genuinely the single
source of truth for both the app and the server).

## Issues Found & Fixed

- [regression] `locales/en.yaml` / `locales/fr.yaml` — the phase added a **second
  top-level `settings:` key**, so the per-device settings page lost every one of
  its translations (`settings.volume`, `settings.alias`, `settings.forget`, … all
  rendered as the raw key on the device), and `settings.title` collided with the
  new page's title → the app-wide page now owns an `app_settings:` namespace, the
  device page keeps `settings:`, and the 14 call sites in `src/main.rs` were
  rewritten. Verified empirically (`rust_i18n::t!` resolution) before and after.
- [logic] `src/backend.rs:activate_backend` — re-activating the backend already in
  use paused it (`pause_at(previous)` fired with `previous == next`), i.e. the
  opposite of what the tap asked for; only the UI guarded it → extracted the pure
  `is_left_behind()` predicate, used before pausing, with three tests.
- [logic] `src/backend.rs:activate_backend` — the name push went through
  `set_config()`, which re-resolved the address from the *process-wide cache*; a
  second switch landing meanwhile would have pushed the name to the wrong backend
  → added `set_config_at(base, name)`, addressed with the URL captured from the
  entry that was just activated.
- [robustness] `src/backend.rs` — none of the settings-page calls had a timeout, and
  `reqwest` has none by default: a mistyped LAN address that drops packets left the
  `Test` button waiting forever and leaked one task per switch → bounded
  `test_backend`, `pause_at` and the config push with `SETTINGS_CALL_TIMEOUT` (5 s).
- [dead code] `src/backend.rs` — `get_config()` and the `set_config()` wrapper had
  no caller; the Android build reported them as `dead_code` (host clippy hid them
  behind `cfg_attr(not(android), allow(dead_code))`) → removed. `dx build` is now
  warning-free.
- [mobile/JNI] `src/settings.rs:read_stored` — unlike its sibling `write_stored`
  and unlike `src/deep_link.rs`, the read seam returned `None` on a failed JNI call
  **without clearing the pending exception**, which aborts the process on the next
  JNI call → wrapped in the same closure + `exception_clear()` pattern, so a
  failure really degrades to "nothing stored".
- [error handling] `blue2th-server/src/lib.rs:set_config` — `let _ = spotify.stop();`
  silently discarded the error before the rename restart → logged with
  `tracing::warn!`, like the `start()` next to it.
- [docs] `blue2th-server/src/spotify_auth.rs:408` — the new `set_device_name` was
  inserted *under* `auth_state`'s doc comment, leaving `auth_state` undocumented and
  `set_device_name` carrying an unrelated first line → doc comments reattached.
- [duplication] `blue2th-server/src/spotify.rs` — `SpotifyBackend::new()` and
  `with_name()` were two copies of the same struct literal → `new()` now delegates
  to `with_name(SPOTIFY_DEVICE_NAME)`.
- [ui state] `src/main.rs:BackendStatus` / `AppSettingsPage` — the entry lists took a
  nested second `app_settings.read()` per row for the active index → one snapshot per
  render, so names and the active marker can never come from different reads.
- [ui state] `src/main.rs:AppSettingsPage` — `notice` ("backend answered") and `error`
  were never cleared against each other, so a stale success sat next to a fresh
  failure; and `next.activate(last)` after the first add swallowed its error with
  `let _ =` → exactly one message is shown per action, and the activation failure is
  surfaced.
- [naming] `src/settings.rs:write_stored` — the local holding the write outcome was
  called `stored` (copied from the read seam) → `written`.
- [docs] `docs/ROADMAP.md` — the pending item still described `BLUE2TH_BACKEND_URL`
  read via `option_env!` as the current state, which phase 6.2 removed → rewritten as
  "mDNS discovery", the part that is actually left.

### Checked, no change needed
- **Test isolation** (verified empirically, not by reading): `~/.local/state/blue2th/`
  was byte-identical (md5 + mtime) before and after a full `cargo test --workspace`,
  and no `name.json` was created — `ServerName::new()`, `with_store(None)` and
  `SpotifyAuth::with_config` all stay off-disk.
- **The settings cache guard**: the process-wide cache is touched by exactly two
  tests, both in `src/backend.rs`'s module, both taking `SETTINGS_GUARD`; the
  integration binary `tests/settings.rs` is a separate process and only exercises the
  pure functions. The `tokio::sync::Mutex` is the right choice (the guard is held
  across `await`s) and the "assert nothing configured" test also resets the cache on
  entry, so a failing peer cannot cascade into it.
- **Activation ordering**: the local switch + persist happens before any network
  step, and both remote steps are best-effort, so a failure can never leave the app
  on a backend the user did not choose.
- **Layer hygiene**: `blue2th-proto` gained only serde DTOs, two constants and a pure
  validator — no platform or hardware dependency; `blue2th-server` re-validates with
  that same rule; every `SPOTIFY_DEVICE_NAME` use left is a *default*, never a lookup
  key (the Web API lookup now takes `&self.device_name`).

## New Tests Added
- `test_is_left_behind_is_true_for_another_backend`: switching to a different backend
  quietens the one being left.
- `test_is_left_behind_is_false_for_the_same_backend`: re-activating the current
  backend must not pause it.
- `test_is_left_behind_is_true_when_nothing_is_active`: with no target left, the
  previous backend is still quietened.
- `test_remove_unknown_index_is_refused`: deleting a row that is already gone (stale
  render, double tap) is refused instead of panicking, on both a filled and an empty
  list.
- `test_add_accepts_a_name_freed_by_a_deletion`: the duplicate check looks at the
  current list, not at a history of names.
- `test_locales_carry_both_settings_pages_labels`: every `app_settings.*` **and**
  `settings.*` key resolves in `en` and `fr` — the regression test for the duplicate
  YAML key that silently un-translated the device page.

## Final Status
- `cargo test --workspace`: ✅ 325 passed (316 before), 1 ignored (needs a live PipeWire daemon)
- `cargo clippy --workspace`: ✅ clean (with `-D warnings` and the unwrap/expect/panic lints)
- `cargo fmt --check`: ✅ clean
- `cargo build --workspace`: ✅ exit 0
- `dx build --platform android`: ✅ success, and now warning-free
