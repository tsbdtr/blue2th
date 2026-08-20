# Review Report — Auto-reconnect remembered speakers

## Affected Layers
mobile, server, proto

## Issues Found & Fixed

- [edge case] `blue2th-server/src/reconnect.rs` (`record_success`) — cleared the
  address state *entirely*, dismissal included, which contradicted `rearm`'s own
  doc ("clears the give-up **and** the dismissal") and opened a real race: the app
  polls `/devices` every couple of seconds, so a listing taken just before a
  `/disconnect` landed still reports the speaker as connected, `sync_connected`
  called `record_success`, the dismissal vanished and the next pass dialled back
  the speaker the user had just hung up → `record_success` now clears only the
  retry ladder and keeps a dismissal; only `rearm` (the user's `/connect` or
  `/select`) takes a hang-up back. Two tests added.
- [invariant: never pair] `blue2th-server/src/lib.rs` (`auto_reconnect_pass`) —
  dialled through `bluetooth::connect_device`, which calls `device.pair()` when
  the device is not bonded; an unattended pass must never create a bond (the
  candidate filter was the only thing preventing it, across a TOCTOU window and a
  hand-edited store) → added `bluetooth::connect_paired_device`, which refuses an
  unpaired address instead of pairing it, and the pass now uses it. `/connect`
  keeps `connect_device`: there the user is asking.
- [edge case] `blue2th-server/src/lib.rs` (`auto_reconnect_pass`) — `Ok(device)`
  was recorded as a success even when `device.connected` was `false` (BlueZ
  accepted the dial, the link never came up), which cleared the ladder and made
  the pass re-dial that speaker on every 15 s tick, backoff permanently reset →
  a dial that does not bring the link up is now counted as a failure.
- [correctness] `blue2th-server/src/lib.rs` (`auto_reconnect_pass`) — a successful
  dial left `state.connected` stale until the next `/devices` poll, which only
  happens while the app is open; that cache is what the next tick short-circuits
  on and what `/select` validates against → the reconnected address now joins it,
  exactly as the `/connect` handler does.
- [edge case] `blue2th-server/src/lib.rs` (`set_config`) — turning `auto_reconnect`
  back off and on left the tracker's given-up and dismissed states in place, so
  the toggle looked inert on precisely the speaker the user turned it back on for
  → the off → on edge (only that edge, since the app re-pushes the whole config on
  every activation) now calls the new `ReconnectTracker::rearm_all`. Test added.
- [performance] `blue2th-server/src/lib.rs` (`sync_connected`) — took the targets
  and reconnect locks and cloned the intent on every `/devices` poll of every
  client, even with nothing connected → guarded on a non-empty connected list.
- [performance / maintainability] `src/backend.rs` (`set_config_at`) — took an
  owned `ConfigRequest` and then rebuilt it field by field to serialize it, so a
  setting added later would silently fail to reach the wire from this one call →
  serializes the request it was handed.

Checked and found sound (no change): no lock is held across a BlueZ `await` (the
`if`-condition guard is dropped before the block, every other guard is
statement-scoped); the pass returns before any D-Bus call when the setting is
off, the intent is empty or nothing is due; every failure is logged and
swallowed; no `unwrap`/`expect`/`panic`/`todo` outside `#[cfg(test)]`; the
`auto_reconnect` serde default is `true` in all three layers; `blue2th-proto`
gained no dependency; the pass never selects, routes or plays; tests build the
router through `app_with_auth_store` (no store, empty intent), so the spawned
pass returns before touching the developer's own adapter.

## New Tests Added
- `test_record_success_keeps_a_dismissal`: a stale "still connected" listing must
  not undo a `/disconnect`; `rearm` still does.
- `test_record_success_clears_the_backoff_of_a_dismissed_address`: keeping the
  dismissal must not drag the old backoff along — once re-armed, the ladder
  starts at its first step.
- `test_rearm_all_makes_every_address_due_again`: switching the setting back on
  clears the given-up, the dismissed and the mid-backoff alike.

## Final Status
- `cargo test --workspace`: ✅ 652 passed (1 ignored: needs a live PipeWire daemon)
- `cargo clippy --workspace`: ✅ clean (with `-D warnings` and the `unwrap`/`expect`/`panic`/`todo`/`unreachable`/`unimplemented` lints)
- `cargo fmt --check`: ✅ clean
- `cargo build --workspace`: ✅ success
- `dx build --platform android`: ⏭️ skipped (the user drives the device build)
