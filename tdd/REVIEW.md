# Review Report — Phase 6.3 — Restore the playback selection when a speaker comes back

## Affected Layers
mobile, server, proto

## Spec audit (the three items RED could not cover)

1. **`sync_connected` wiring** — implemented and correct in both halves: `restore(...)`
   is called, and `apply_selection_change` runs only when it returns `true`. Verified
   that a quiet poll cannot re-route (`restore` is idempotent, pinned by
   `test_restore_is_idempotent_and_reports_no_change_the_second_time` and by
   `blue2th-server/tests/restore.rs`). Tightened further, see below.
2. **Live playback state in the gate** — `should_restore(playing, flag)` is fed by
   `engine.poll_state().status == Playing` **or** `spotify.poll_liveness().status ==
   Running`, so the Spotify source is covered as well as the local tone. Correct.
3. **Settings page Playback section** — present: section title, checkbox bound to the
   *active* backend, handler storing locally + pushing over `POST /config` through
   `push_active_config`, error surfaced into the shared `Signal<Option<String>>`, and
   the CSS (`.settings-toggle`, `.settings-hint`). Two defects fixed, see below.

**Store format change** — the reasoning holds: `load_store` tries the bare
`{address: ms}` map *first*, because `StoredTargets` ignores unknown fields and would
otherwise swallow a phase 6.1 file into an all-default value, losing every offset
(`test_a_phase_6_1_store_loads_with_no_intent` pins it). `save_offsets` (`#[cfg(test)]`)
delegates to the same `save_store` production uses, so it cannot diverge — its doc
comment claimed the opposite and was corrected.

**Deadlock risk** — no guard is ever held across an `await` that re-acquires it: every
lock is taken in its own block and dropped before the next, and `apply_selection_change`
/ `resync_spotify_sink` are only reached with all guards released. No nested acquisition
anywhere, so the ordering `connected → targets → engine → spotify → name → targets` is
safe. The double `targets` lock was collapsed anyway (see below).

**Test isolation** — verified empirically under `strace`: a full `cargo test --workspace`
opens `~/.local/state/blue2th/{offsets,name,spotify-token}.json` **read-only** (from the
pre-existing `app()` unit tests in `blue2th-server/src/lib.rs`) and never writes them —
the three files' md5 sums are byte-identical before and after the run. No `O_WRONLY`,
`O_CREAT`, `rename` or `unlink` touches that directory. The read is pre-existing 6.1/6.2
behaviour and is what `test_app_builds_with_the_offsets_store_and_restores_no_selection`
deliberately asserts on; 6.3 adds only an intent *read* to it.

## Issues Found & Fixed
- [dead code / stale marker] `blue2th-server/src/targets.rs:172` — a `STUB (phase 6.3)`
  comment still sat on the (now implemented) intent reload → removed and the doc comment
  rewritten to describe what `with_store` actually restores.
- [performance] `blue2th-server/src/targets.rs:170` — `with_store` called `load_intent`
  *and* `load_offsets`, each doing its own `load_store`: the same file was read and
  parsed twice on every startup (confirmed in the `strace` capture, two `openat` of
  `offsets.json` per process) and the two halves could come from two different versions
  of it → one `load_store` for both. `load_offsets` became test-only and is now
  `#[cfg(test)]`; `load_intent` was folded away.
- [error handling] `blue2th-server/src/targets.rs:198` — `restore` returned `true`
  whenever `restorable` was non-empty, even if every `select` had been rejected, which
  would claim a change and rebuild the whole PipeWire graph for nothing → it now reports
  the selects that actually succeeded (`changed |= …is_ok()`).
- [performance / hot path] `blue2th-server/src/lib.rs:509` — `sync_connected` locked the
  engine, the Spotify backend (`poll_liveness` does a `waitpid`) and the name on *every*
  `/devices` poll, i.e. every two seconds per client, even when nothing had come back →
  it now leaves early when `restorable` is empty, which is the overwhelmingly common
  case, and only then computes `playing` / `should_restore`.
- [correctness] `blue2th-server/src/lib.rs:537` — the selection was re-locked *after* the
  guard that changed it, so `apply_selection_change` could act on a snapshot a concurrent
  `/select` had already moved → `speakers()` is now read under the same guard as
  `restore`.
- [consistency] `blue2th-server/src/lib.rs:446` — `POST /config` echoed
  `req.restore_during_playback` instead of what was stored, unlike the `name` it reads
  back after trimming → both values now come from the store.
- [naming / consistency] `src/main.rs:1925` — the toggle read `e.value() == "true"` while
  every other checkbox in the file uses `e.checked()` → switched to `e.checked()` (same
  result, but the documented API rather than a string compare on the raw value).
- [UI defect] `src/main.rs:1956` — the notice/error feedback was moved into its own
  `div.settings-section`, which is a bordered, padded card: with no notice and no error
  (the normal state) an empty box sat under the page for the whole session → the card is
  now rendered only when there is something to show.

## Observation (not changed — it would alter specced behaviour)
`intended` is only ever cleared by `deselect`, so it grows without bound: every speaker
ever selected and left to go flat stays in it forever, and `restorable` fills the free
slots in *oldest-first* remembered order. After a long-lived install, two long-forgotten
speakers can therefore be preferred over a more recent choice when several reconnect at
once. The order is pinned by `test_restorable_lists_both_speakers_in_remembered_order`
and the "only `deselect` clears it" rule is an acceptance criterion, so this is left
as-is and merely documented by a new test.

## New Tests Added
- `test_restorable_caps_an_intent_longer_than_the_selection`: the intent can exceed
  `MAX_TARGETS` (three speakers wanted, none selected); restoration fills the cap in
  remembered order and the leftover entry does not keep reporting a change on later polls.
- `test_restore_does_not_rewrite_the_store`: a sentinel written behind the selection's
  back survives a `restore`, proving the `/devices` hot path performs no disk write.

## Final Status
- `cargo test --workspace`: ✅ 377 passed (375 before, +2 added), 0 failed
- `cargo clippy --workspace --all-targets -D warnings …`: ✅ clean
- `cargo fmt --check`: ✅ clean
- `cargo build --workspace`: ✅ success
- `dx build --platform android`: ✅ success
