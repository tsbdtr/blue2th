# Review Report — Phase 5.2 — Spotify OAuth (PKCE) + Web API transport & now-playing over SSE

## Affected Layers
mobile, server, proto (all three reviewed)

## Issues Found & Fixed
- [docs] `blue2th-server/src/spotify_auth.rs:10` — module doc still claimed the helper bodies were `todo!()` stubs "written test-first"; the helpers are fully implemented → removed the stale paragraph.
- [convention] `blue2th-server/src/spotify_auth.rs:349,356` — two `String` clones (`access_token`, `refresh_token`) had no justification comment, which CLAUDE.md requires → added brief comments explaining each clone is necessary (returning owned data / reusing after `self.tokens` is reassigned).
- [edge case] `parse_now_playing` mapped a body with no `item` and a malformed body to Idle, but neither path was covered → added `test_parse_now_playing_no_item_is_idle` and `test_parse_now_playing_malformed_body_is_idle`.
- [build drift] `assets/tailwind.css` — 85-line regenerated Tailwind artifact (generic `absolute`/`grow`/`lowercase`/`filter`/`transition` utilities + `@property` blocks from a newer Tailwind); the phase 5.2 UI uses custom classes in `main.css`, none of these utilities → reverted to base (consistent with phase 4 / 5.1 reviews). Android build still succeeds with the base file.
- [process] `graphify-out/` was bundled into the implementation commit; project convention regenerates the graph in a separate `chore(graph): …` commit during `/tdd done` → reset `graphify-out/` back to base so it regenerates cleanly on develop after merge.

## Security review (no code change needed — verified clean)
- No client secret anywhere; only the public `client_id` (env-overridable) is used, consistent with the PKCE public-client flow.
- `code_verifier` never leaves the server: minted in `authorize_url`, stored in `Pending`, sent only to the token endpoint; only the derived `code_challenge` is placed in the authorize URL.
- CSRF `state` is validated on callback: `exchange_code` does `pending.take().filter(|p| p.state == state)`, rejecting a mismatch with a 502-mapped `Exchange` error and consuming the pending authorization.
- Tokens are in-memory only (`Option<Tokens>`); dropping them on `disconnect()` returns to Disconnected.
- No tokens/codes are logged: `SpotifyApiError`'s `Display` and the `Exchange(String)` payload carry only reqwest/status text, and `AppError::into_response` logs that message — never a token or code. No `tracing` call touches the code/token.
- SSE resilience: `spotify_now_playing` ignores per-tick `Err` (incl. `NotConnected` while Disconnected) and keeps the stream alive via keep-alive; a dropped client just ends the stream. Note: the single `Arc<Mutex<SpotifyAuth>>` means a slow now-playing HTTP fetch briefly blocks a concurrent transport call for that request's duration — acceptable for this single-user backend and out of scope to redesign here.

## New Tests Added
- `test_parse_now_playing_no_item_is_idle`: a 200 `{}` body (no active track) maps to `NowPlayingState::Idle` with no title/artist.
- `test_parse_now_playing_malformed_body_is_idle`: a non-JSON body is treated as Idle, so a transient bad payload cannot break the SSE feed.

## Final Status
- `cargo test --workspace`: ✅ all passed (server lib now 63, incl. 2 new)
- `cargo fmt --check`: ✅ clean (only pre-existing nightly-only rustfmt option warnings)
- `cargo clippy --workspace --all-targets -- -D warnings …`: ✅ clean
- `dx build --platform android`: ✅ success
