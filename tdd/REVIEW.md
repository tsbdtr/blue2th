# Review Report — Phase 6.4 — Authenticated LAN API with QR or code pairing

## Affected Layers
mobile, server, proto

## Spec compliance — criterion by criterion

Every acceptance criterion is now pinned by a named test. The ones that were
**not** pinned (or not implemented) when the review started are marked → HOLE and
listed again in the section below.

| Criterion | Pinned by |
| --- | --- |
| `generate_token()` ≥32 bytes, URL-safe, different every call | `auth.rs::test_generate_token_is_long_and_url_safe`, `test_generate_token_differs_on_every_call` |
| `PairingCode::mint()` short, URL-safe, expires after `PAIRING_TTL` | `test_pairing_code_mint_is_short_and_url_safe`, `test_pairing_code_mint_expires_after_the_ttl`, `test_pairing_code_mint_differs_on_every_call` |
| `verify_code` accepts only unexpired/unconsumed/exact; expired ≡ unknown | `test_verify_code_accepts_the_armed_code`, `test_verify_code_rejects_anything_but_an_exact_match`, `test_verify_code_cannot_tell_expired_from_unknown_or_unarmed`, `test_verify_code_rejects_a_code_past_its_expiry` |
| One-shot | `test_verify_code_rejects_a_consumed_code`, `test_redeem_consumes_the_code_so_a_second_use_fails`, `pairing.rs::test_pair_with_an_already_used_code_is_unauthorised` |
| `MAX_PAIRING_ATTEMPTS` invalidates the code | `test_verify_code_rejects_the_right_code_once_the_attempt_cap_is_reached`, `test_verify_code_still_accepts_the_right_code_below_the_attempt_cap`, `test_redeem_invalidates_the_code_after_the_attempt_cap`, `pairing.rs::test_pair_is_rate_limited_and_invalidates_the_armed_code` |
| Token persisted `0600`, reloaded, malformed → new token | `test_auth_store_persists_the_token_with_owner_only_permissions`, `test_auth_store_round_trips_the_token_through_its_store`, `test_auth_store_with_a_malformed_store_mints_a_new_token`, `test_auth_store_with_a_missing_store_mints_and_persists` |
| proto `PairRequest`/`PairResponse` round-trip, `HealthStatus.auth_required` with `serde(default)` | `proto::test_pair_request_round_trips_through_json`, `test_pair_response_round_trips_through_json`, `test_health_status_round_trips_with_auth_required`, `test_health_status_without_auth_required_parses_an_old_payload` |
| Every route but `/health` and `POST /pair` 401s, route by route | `auth.rs (tests)::test_every_guarded_route_rejects_a_missing_bearer`, `…_rejects_a_wrong_bearer`, `…_accepts_a_valid_bearer`, `test_only_health_and_pair_are_public`, `test_route_table_lists_every_route_the_backend_serves` |
| `POST /pair` valid → token; wrong/expired/used/unarmed → 401 | `pairing.rs::test_pair_with_a_valid_code_returns_the_token` + the four refusal tests + `test_pair_failures_are_indistinguishable` |
| `pair_deep_link` / `parse_pair_link` round-trip, reject missing `code`/`url` | `proto::test_pair_deep_link_round_trips_through_parse_pair_link`, `test_parse_pair_link_rejects_a_missing_code`, `…_missing_url`, `…_a_malformed_url` |
| The QR encodes exactly that URL | → HOLE 6 — was only pinned as "a block of text"; now `lib.rs::test_pairing_qr_encodes_the_link_it_is_given` |
| `lan_bind_address()` prefers LAN IPv4, falls back to `0.0.0.0`, yields to `BLUE2TH_BIND` | `test_bind_address_*`, `test_preferred_lan_ipv4_*`, `test_lan_bind_address_yields_to_the_bind_env_var` |
| `CorsLayer::permissive()` gone | `auth.rs (tests)::test_permissive_cors_is_gone_from_the_server_sources` + the two behavioural CORS tests |
| mobile: `BackendEntry.token` + `pairing`, persisted, round-tripping | `tests/settings.rs::test_token_and_pairing_method_round_trip_through_the_settings_blob`, `test_pairing_method_defaults_to_code`, `test_a_phase_6_3_blob_loads_unpaired_with_the_default_method` |
| mobile: every call carries the bearer; a 401 reads as "not paired" | → HOLE 2 & 3 — partially implemented; now `test_backend_calls_carry_the_bearer_token`, `test_the_config_push_carries_the_bearer_token`, `test_ping_backend_carries_the_bearer_once_paired`, `test_activate_backend_pauses_the_previous_one_with_its_own_token`, `test_a_plain_call_reports_not_paired_on_401`, `test_scan_reports_not_paired_on_401` |
| mobile: a scanned link creates, activates, updates-not-duplicates | `tests/settings.rs::test_upsert_from_pair_link_*` (8 tests) |
| settings page: pick the method, run the exchange | UI (manual); labels pinned by `test_locales_carry_both_settings_pages_labels` |
| `cargo test --workspace` + `dx build --platform android` | see Final Status |

Non-nominal scenarios: SSE 401-terminal ✅ (both feeds, see HOLE 3/5), known URL
updated with the local name kept ✅, malformed deep link ✅, unreadable token
store ✅ (see HOLE 1), no-LAN-address fallback ✅.

Constraints: no `unwrap`/`expect`/`panic` outside `#[cfg(test)]` ✅ (clippy line
clean), proto target-agnostic ✅ (the one addition, `percent_decode`, is plain
string handling), `android/AndroidManifest.xml` untouched ✅ (`git diff` on
`android/` is empty), no test touches `~/.local/state/blue2th/` ✅ (no `auth.json`
exists there after a full run; no test calls `app()`), `CorsLayer::permissive()`
gone ✅.

### Security core, verified by reading (not by trusting its tests)
- **One-shot**: `redeem` calls `consume()` on success; `verify_code` refuses a
  consumed code before anything else. ✅
- **Attempt cap**: every failure calls `register_failure`, and reaching
  `MAX_PAIRING_ATTEMPTS` drops the armed code entirely (`self.pairing = None`). ✅
- **Indistinguishable refusals**: `PairError` has a single variant, and the
  handler maps it to one status and one message. Adding a variant breaks a test. ✅
- **Constant-time compare**: no early exit; only the length is short-circuited,
  which is not the secret. ✅
- **Bearer parsing**: a trailing space is *not* trimmed, so `"Bearer s3cret "` is
  refused (length differs) — pinned by `test_is_authorised_accepts_only_the_stored_token`. ✅
- **Guard coverage**: the router has exactly one `Router::new()`, no `.fallback`,
  no `.nest`, no `.merge`, no `route_service`, and the binary only calls `run()`.
  Every route therefore comes from `ROUTES`, each non-public entry individually
  layered; a table entry without a handler 404s. No route can exist unguarded. ✅

## Issues Found & Fixed

- [spec hole] `src/backend.rs:367` `ping_backend` — went through `authed_client()`,
  so an app with no token could not reach the deliberately-open `/health` at all →
  an alive-but-unpaired backend showed a **red dot and "offline"**, the exact
  confusion the open probe exists to prevent. Now resolves the base URL only and
  attaches the bearer when there is one. (HOLE 1)
- [spec hole] `src/backend.rs` (10 call sites) — `fetch_devices`, `playback_state`,
  `set_volume`, `post_transport`, `post_device_action`, `select_target`,
  `deselect_target`, `set_offset`, `fetch_targets`, `spotify_status`,
  `post_spotify`, `spotify_auth_status` used `error_for_status()`, which flattens a
  401 into a bare status line: the criterion "a 401 is surfaced as *not paired*"
  held only for the three routes that happened to use `backend_error_message`.
  Replaced by one `send_json` helper — which also removed ~90 lines of the same
  four-step chain. (HOLE 2)
- [spec hole] `src/backend.rs:233` `pause_at` — sent **no bearer at all**, so
  quietening the backend being left behind on a switch always 401'd (a regression
  this phase introduced, surfaced to the user as a "not paired" toast on every
  switch). It now carries *that* entry's token, captured before the switch;
  `set_config_at` takes its token explicitly too, instead of reading the global
  active one. (HOLE 3)
- [spec hole] `blue2th-server/src/lib.rs:279` `run()` — the pairing window opened
  on `!path.exists()`. A store that existed but was unreadable or malformed mints a
  new token (invalidating every paired client) yet armed **no code**, and logged
  "this backend is already paired". The operator's phone would simply stop working.
  `AuthStore` now reports `minted_a_new_token()` and `run()` arms on that. (HOLE 4)
- [spec hole] `src/main.rs:295` now-playing loop — a 401 `return`ed: the error was
  **never surfaced** (the spec asks for "stop retrying *and* surface not paired"),
  and the comment claimed pairing again re-armed the loop, which nothing did — the
  feed stayed dead until an app restart. It now surfaces the message and parks on
  `await_new_token()` until the stored token changes. (HOLE 5)
- [unpinned] `blue2th-server/src/lib.rs` `pairing_qr` — "the QR encodes exactly
  that URL" was asserted only as "some block of text that is not the URL". Added a
  test that the render is a function of the link (same link ⇒ same block, a
  different code ⇒ a different block). (HOLE 6)
- [unpinned] `blue2th-server/src/lib.rs:317` `advertised_url` — untested, because
  it resolved the host's interfaces itself. Split into a pure `advertised_url_from`
  and pinned (wildcard → LAN address, explicit bind kept, no-LAN fallback). (HOLE 7)
- [unpinned] `blue2th-proto/src/lib.rs` `percent_decode` — its comment claims to
  defend against a `%` followed by a multi-byte character, with no test. Added
  `test_parse_pair_link_survives_a_mangled_percent_escape` and
  `test_parse_pair_link_decodes_a_multibyte_name`.
- [duplication] `src/deep_link.rs:84` — `percent_decode`/`hex_digit` were copied
  byte-for-byte into `blue2th-proto`: two copies of the same security-relevant
  parser, for two links arriving through the *same* custom scheme. The proto one is
  now `pub` and `deep_link.rs` uses it.
- [dead code] `blue2th-server/src/lib.rs:444` `app_with_auth` — no caller left, and
  it minted a token the caller could not know, so any future test using it would
  401 on everything and look broken for the wrong reason. Removed, with a note
  saying why no such constructor should come back.
- [edge case] `src/main.rs` settings pairing section — the **Pair** button fired on
  an empty box and on a double tap, each spending one of the server's five
  attempts (and a double tap reports a failure for a code that in fact just
  worked). Now disabled while empty or in flight, and the code is trimmed.
- [comment] `blue2th-server/src/lib.rs:73` — the `auth` field carried a
  `#[allow(dead_code)]` and a "STUB (phase 6.4): held but not yet read" comment
  while being read by both the guard and the pair handler.
- [comment] `blue2th-server/src/lib.rs` `set_config` — "reachable by anything on
  the LAN until the authenticated API lands" was exactly what this phase fixed;
  `DEFAULT_BIND` still described `0.0.0.0` as the intent rather than the fallback.
- [comment] `src/backend.rs:157` — two doc comments had been merged into one block,
  leaving `authed_client` documented as "The active backend's base URL".
- [naming] `src/main.rs` `SpotifyUi::login_error` → `background_error`: it now
  carries pairing failures from the deep-link task and the now-playing feed's
  refusal, not just a Spotify login failure.
- [consistency] `src/settings.rs` `upsert_from_pair_link` — a backend created by
  scanning kept `PairingMethod::Code`, though the spec has the method "chosen when
  the backend is added". It now records `Qr` **on creation only**; for a known
  entry the user's own choice survives, exactly as their chosen name does.

## Placement question (raised in the brief)
`parse_pair_link` living in `blue2th-proto` rather than `src/deep_link.rs` is the
right call, and is *not* inconsistent with the OAuth callback: the pair link is a
contract **we** define on both sides — the server builds it, the app parses it —
so it belongs where the DTOs are, and the two halves cannot drift. The Spotify
callback URI is defined by a third party and only ever parsed, so it stays in the
app. What *was* inconsistent is that the two parsers had a copy each of the same
percent-decoder; they now share one.

## Left for a product decision (not changed)
- **The typed code is case-sensitive.** `PAIRING_CODE_ALPHABET` is upper-case, and
  Android will usually capitalise only the first character, so a user typing
  `K7m2qx` burns an attempt with no visible reason. Options: upper-case the input
  in the app, or compare case-insensitively server-side. Both narrow the code
  space slightly; neither is obviously yours to choose.
- **`NOT_PAIRED` ("not paired") is not localised**, while the settings page has a
  translated `app_settings.not_paired`. Backend error messages are not localised
  anywhere in the app today, so this follows the existing convention — but it is
  the one such message the user is *expected* to see.
- **The API token is stored in plain `SharedPreferences`** (app-private, not
  `EncryptedSharedPreferences`/Keystore). Fine on a non-rooted device, worth a
  decision if the threat model ever includes a compromised phone.

## New Tests Added
- `test_pairing_qr_encodes_the_link_it_is_given`: the QR render is a function of
  the link — same link, same block; different code, different block.
- `test_advertised_url_replaces_the_wildcard_with_the_lan_address`: a backend on
  `0.0.0.0`/`[::]` advertises the detected LAN address in the QR.
- `test_advertised_url_keeps_an_explicit_bind_address`: an operator's own address
  is advertised as-is.
- `test_advertised_url_without_a_lan_address_keeps_the_bind_address`: no LAN
  address does not break the banner (the printed code still works).
- `test_auth_store_reloading_a_token_reports_no_minting`: a plain restart leaves
  the one open door shut.
- `test_auth_store_reports_minting_when_the_store_is_unusable`: a malformed or
  empty-token store invalidates every client, so pairing must re-open.
- `test_parse_pair_link_survives_a_mangled_percent_escape`: `%é`, `%`, `%4`,
  `abc%`, `%zz` parse rather than panic.
- `test_parse_pair_link_decodes_a_multibyte_name`: an accented backend name
  round-trips through the link.
- `test_ping_backend_reaches_health_without_a_token`: the open probe is reachable
  unpaired, so "not paired" never reads as "offline".
- `test_ping_backend_carries_the_bearer_once_paired`: and still carries the bearer
  when there is one.
- `test_a_plain_call_reports_not_paired_on_401`: a revoked token is typed, not a
  bare status line.
- `test_scan_reports_not_paired_on_401`: the `/scan` feed treats a 401 as terminal
  like `/spotify/now-playing` does.
- `test_activate_backend_pauses_the_previous_one_with_its_own_token`: the backend
  being left behind is quietened with its own credential.
- `test_upsert_from_pair_link_records_the_qr_as_the_new_backend_method` and
  `test_upsert_from_pair_link_keeps_the_method_of_a_known_backend`.

## Final Status
- `cargo fmt --check`: ✅ clean
- `cargo test --workspace`: ✅ 510 passed, 0 failed (1 ignored — needs a live
  PipeWire daemon), up from 490
- `cargo clippy --workspace --all-targets` (with the project's `-W` set): ✅ clean
- `cargo build --workspace`: ✅ exit 0
- `dx build --platform android`: ✅ success
