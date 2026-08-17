# Review Report — Phase 6.6 — Find the backend on the network instead of retyping its address

## Affected Layers
mobile, server, proto (proto reviewed, unchanged — it stays target-agnostic:
`blue2th-proto/Cargo.toml` gained no dependency, `mdns-sd` is on the root and
server crates only)

## Issues Found & Fixed
- [design] `blue2th-server/src/lib.rs:328` — `advertise` recovered the host and port by string surgery on the URL it had just assembled (`trim_start_matches("http://")` + `rsplit_once(':')`) → `AdvertisedService` now carries `endpoint: Option<(String, u16)>`, built by a new pure `advertised_endpoint_from`, which `advertised_url_from` also builds its URL from. The wildcard rule now lives in exactly one place, so the QR and the mDNS record cannot point at different hosts, and an unadvertisable bind address is declined with a warning instead of silently.
- [dead code] `blue2th-server/src/identity.rs:36` — the `store: Option<PathBuf>` field was never read and carried `#[allow(dead_code)]` → removed; the id never changes once minted, so there is nothing to write after construction.
- [edge case] `src/discovery.rs:114` — the browse deduplicated on `url == url && id == id`, so a multi-homed backend resolving once per interface (two addresses, one id) was listed twice, shown as two machines and repaired twice in the same scan → extracted a pure `is_new_find`, which treats a repeated address **or** a repeated id as the same machine.
- [edge case] `src/settings.rs` / `src/main.rs` — a **pre-6.6 entry never learnt an id**: it was matched on its URL, classified `UpToDate`, and nothing adopted the id the service was advertising. Its first address change therefore still offered the known machine as a brand new backend — precisely the duplication this phase exists to stop, for every existing install → added `AppSettings::adopt_discovered_ids`, called once per scan before the repair pass. It never re-assigns an id another entry already claims, never touches the token, and only the moved-address case still raises the "address updated" notice.
- [safety] `src/discovery.rs:196` (Android-only) — `MulticastGuard::acquire` called `acquire()` on the Java lock *before* taking the global ref, so a failure on `new_global_ref` left the multicast lock held with no guard to release it (radio filter lifted for the rest of the process) → the global ref is taken first, `acquire` runs on it, and a failure drops the ref with no lock held. Every JNI failure still routes through `jni_util::err_clear`, and `Drop` clears through it too, so no path can return with a pending Java exception.
- [idiomatic] `src/main.rs:1848` — a four-element tuple with `Option<(usize, String, String)>` inside, silenced by `#[allow(clippy::type_complexity)]` → named `DiscoveryRow` / `PendingRepair` structs; the lint allow is gone and the rsx destructures them by name.
- [test hygiene] `tests/discovery.rs:422` — the source-order assertion read comments as if they were code (it had already misfired once on a doc line quoting `attach_current_thread()`) → comment lines are stripped before the search, so the test pins the order of the *calls* and stops being hostage to the prose around them. Kept in this form otherwise: the ART abort it guards cannot be caught at runtime.
- [test hygiene] `src/bluetooth.rs:1276` — the scoping of `test_locale_en_scan_scanning_is_loading` to the `scanning:` line is a correction, not a weakening (`en.yaml` holds exactly one such key, and the assertions always named `scan.scanning`); it only failed to say so when the key disappeared → added an explicit "the key must still exist" assertion so a rename fails loudly instead of through an empty string.

## Judgements made, deliberately left alone
- `browse_proceeds(lock_acquired: bool) -> bool` keeps its odd signature: the rule ("a refused lock is not a failed scan") is pinned by a test rather than buried in an `if`, and reshaping it would only move the rule back out of sight.
- `tokio::time::timeout_at` inside the browse follows `src/backend.rs:445`, which already runs `tokio::time::timeout` in an app-spawned task.
- The phase invariants hold: `add_discovered` goes through `add` and leaves `token: None`; the id store is a separate file from `auth.json` (`test_identity_survives_a_token_store_wipe`); every identity test writes under `std::env::temp_dir()`, never `~/.local/state/blue2th/`.

## New Tests Added
- `test_advertised_service_carries_the_host_and_port_it_announces` (server): the record carries the very host/port `ServiceInfo` is given, and they agree with the URL — wildcard bind and explicit bind.
- `test_advertised_service_without_a_usable_port_has_no_endpoint` (server): a bind address with no port, or a non-numeric one, yields no endpoint and is still shown verbatim in the banner.
- `test_is_new_find_rejects_a_repeat_at_the_same_address` (mobile): a second resolution at the same address is the same machine, id or no id.
- `test_is_new_find_rejects_the_same_id_at_another_address` (mobile): a multi-homed backend is listed once.
- `test_is_new_find_accepts_a_second_backend` (mobile): two real backends are both listed, including two that advertise no id.
- `test_a_url_matched_entry_adopts_the_discovered_id` (mobile): a pre-6.6 entry learns the id, keeps its token and URL, and then survives its next move as a `Repair`.
- `test_adopting_ids_is_idempotent_and_never_reassigns_one` (mobile): a second scan writes nothing, and an id another entry claims is never stolen.
- `test_adopting_ids_ignores_an_idless_or_malformed_service` (mobile): an id-less or unusable-address service teaches nothing and writes nothing.

## Final Status
- `cargo test --workspace`: ✅ 588 passed, 0 failed (1 ignored — requires a live PipeWire daemon)
- `cargo clippy --workspace`: ✅ clean (with `-D warnings -W clippy::unwrap_used -W clippy::expect_used -W clippy::panic -W clippy::todo -W clippy::unreachable -W clippy::unimplemented`)
- `cargo fmt --check`: ✅ clean
- `cargo build --workspace`: ✅ success
- `dx build --platform android`: ✅ success
