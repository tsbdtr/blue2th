# Graph Report - blue2th  (2026-08-27)

## Corpus Check
- 50 files · ~99,240 words
- Verdict: corpus is large enough that graph structure adds value.

## Summary
- 1572 nodes · 3518 edges · 64 communities (62 shown, 2 thin omitted)
- Extraction: 100% EXTRACTED · 0% INFERRED · 0% AMBIGUOUS · INFERRED: 13 edges (avg confidence: 0.8)
- Token cost: 0 input · 0 output

## Graph Freshness
- Built from commit: `aaa39b43`
- Run `git rev-parse HEAD` and compare to check if the graph is stale.
- Run `graphify update .` after code changes (no API cost).

## Community Hubs (Navigation)
- [[_COMMUNITY_Android BT JNI & Errors|Android BT JNI & Errors]]
- [[_COMMUNITY_Dioxus Framework Concepts|Dioxus Framework Concepts]]
- [[_COMMUNITY_UI App & State|UI App & State]]
- [[_COMMUNITY_TDD Workflow & Dev Standards|TDD Workflow & Dev Standards]]
- [[_COMMUNITY_Community 4|Community 4]]
- [[_COMMUNITY_Header SVG Assets|Header SVG Assets]]
- [[_COMMUNITY_Bluetooth Icon Assets|Bluetooth Icon Assets]]
- [[_COMMUNITY_i18n Translations|i18n Translations]]
- [[_COMMUNITY_Dioxus 0.7|Dioxus 0.7]]
- [[_COMMUNITY_Library Root|Library Root]]
- [[_COMMUNITY_Feature Spec|Feature Spec]]
- [[_COMMUNITY_Community 11|Community 11]]
- [[_COMMUNITY_Community 12|Community 12]]
- [[_COMMUNITY_Community 13|Community 13]]
- [[_COMMUNITY_Community 14|Community 14]]
- [[_COMMUNITY_Community 15|Community 15]]
- [[_COMMUNITY_Community 16|Community 16]]
- [[_COMMUNITY_Community 17|Community 17]]
- [[_COMMUNITY_Community 18|Community 18]]
- [[_COMMUNITY_Community 19|Community 19]]
- [[_COMMUNITY_Community 20|Community 20]]
- [[_COMMUNITY_Community 21|Community 21]]
- [[_COMMUNITY_Community 22|Community 22]]
- [[_COMMUNITY_Community 23|Community 23]]
- [[_COMMUNITY_Community 24|Community 24]]
- [[_COMMUNITY_Community 25|Community 25]]
- [[_COMMUNITY_Community 26|Community 26]]
- [[_COMMUNITY_Community 27|Community 27]]
- [[_COMMUNITY_Community 28|Community 28]]
- [[_COMMUNITY_Community 29|Community 29]]
- [[_COMMUNITY_Community 30|Community 30]]
- [[_COMMUNITY_Community 31|Community 31]]
- [[_COMMUNITY_Community 32|Community 32]]
- [[_COMMUNITY_Community 33|Community 33]]
- [[_COMMUNITY_Community 34|Community 34]]
- [[_COMMUNITY_Community 35|Community 35]]
- [[_COMMUNITY_Community 36|Community 36]]
- [[_COMMUNITY_Community 37|Community 37]]
- [[_COMMUNITY_Community 38|Community 38]]
- [[_COMMUNITY_Community 39|Community 39]]
- [[_COMMUNITY_Community 40|Community 40]]
- [[_COMMUNITY_Community 41|Community 41]]
- [[_COMMUNITY_Community 42|Community 42]]
- [[_COMMUNITY_Community 43|Community 43]]
- [[_COMMUNITY_Community 44|Community 44]]
- [[_COMMUNITY_Community 45|Community 45]]
- [[_COMMUNITY_Community 46|Community 46]]
- [[_COMMUNITY_Community 47|Community 47]]
- [[_COMMUNITY_Community 48|Community 48]]
- [[_COMMUNITY_Community 49|Community 49]]
- [[_COMMUNITY_Community 50|Community 50]]
- [[_COMMUNITY_Community 51|Community 51]]
- [[_COMMUNITY_Community 52|Community 52]]
- [[_COMMUNITY_Community 53|Community 53]]
- [[_COMMUNITY_Community 54|Community 54]]
- [[_COMMUNITY_Community 55|Community 55]]
- [[_COMMUNITY_Community 56|Community 56]]
- [[_COMMUNITY_Community 57|Community 57]]
- [[_COMMUNITY_Community 58|Community 58]]
- [[_COMMUNITY_Community 59|Community 59]]
- [[_COMMUNITY_Community 60|Community 60]]
- [[_COMMUNITY_Community 61|Community 61]]

## God Nodes (most connected - your core abstractions)
1. `connected()` - 58 edges
2. `AppState` - 52 edges
3. `BackendError` - 49 edges
4. `Result` - 44 edges
5. `two_backends()` - 36 edges
6. `AppSettings` - 30 edges
7. `State` - 29 edges
8. `authed_client()` - 28 edges
9. `Result` - 28 edges
10. `SpotifyAuth` - 27 edges

## Surprising Connections (you probably didn't know these)
- `discovered_from_txt()` --calls--> `Value`  [INFERRED]
  blue2th-proto/src/lib.rs → blue2th-frontend/src/backend.rs
- `parse_spotify_callback()` --calls--> `percent_decode()`  [INFERRED]
  blue2th-frontend/src/deep_link.rs → blue2th-proto/src/lib.rs
- `TransportBar()` --calls--> `playback()`  [INFERRED]
  blue2th-frontend/src/main.rs → blue2th-server/src/lib.rs
- `test_parse_spotify_callback_accepts_reversed_and_extra_params()` --calls--> `parse_spotify_callback()`  [INFERRED]
  blue2th-frontend/tests/spotify_deep_link.rs → blue2th-frontend/src/deep_link.rs
- `test_parse_spotify_callback_denial_is_reported()` --calls--> `parse_spotify_callback()`  [INFERRED]
  blue2th-frontend/tests/spotify_deep_link.rs → blue2th-frontend/src/deep_link.rs

## Import Cycles
- 1-file cycle: `blue2th-frontend/src/backend.rs -> blue2th-frontend/src/backend.rs`
- 1-file cycle: `blue2th-frontend/src/discovery.rs -> blue2th-frontend/src/discovery.rs`
- 1-file cycle: `blue2th-frontend/src/lifecycle.rs -> blue2th-frontend/src/lifecycle.rs`
- 1-file cycle: `blue2th-frontend/src/main.rs -> blue2th-frontend/src/main.rs`
- 1-file cycle: `blue2th-server/src/identity.rs -> blue2th-server/src/identity.rs`
- 1-file cycle: `blue2th-server/src/lib.rs -> blue2th-server/src/lib.rs`
- 1-file cycle: `blue2th-server/src/targets.rs -> blue2th-server/src/targets.rs`
- 1-file cycle: `blue2th-server/src/reconnect.rs -> blue2th-server/src/reconnect.rs`
- 1-file cycle: `blue2th-server/src/spotify.rs -> blue2th-server/src/spotify.rs`
- 1-file cycle: `blue2th-server/src/state_store.rs -> blue2th-server/src/state_store.rs`
- 1-file cycle: `blue2th-server/src/watchdog.rs -> blue2th-server/src/watchdog.rs`

## Communities (64 total, 2 thin omitted)

### Community 0 - "Android BT JNI & Errors"
Cohesion: 0.07
Nodes (90): HashMap, Option, Path, PathBuf, Result, Self, SpeakerTarget, String (+82 more)

### Community 1 - "Dioxus Framework Concepts"
Cohesion: 0.06
Nodes (53): Arc, AtomicBool, AudioError, Box, Default, Display, Error, Formatter (+45 more)

### Community 2 - "UI App & State"
Cohesion: 0.06
Nodes (45): Client, Default, Display, Error, Formatter, NowPlaying, Option, Path (+37 more)

### Community 3 - "TDD Workflow & Dev Standards"
Cohesion: 0.07
Nodes (53): Default, Display, Error, Formatter, Into, Option, Path, PathBuf (+45 more)

### Community 4 - "Community 4"
Cohesion: 0.06
Nodes (40): Default, Display, Error, Formatter, Option, Result, Self, SpeakerTarget (+32 more)

### Community 5 - "Header SVG Assets"
Cohesion: 0.04
Nodes (8): ClientPresence, OffsetRequest, PlaybackState, PlaybackStatus, PresenceRequest, SpotifyAuthState, SpotifyAuthStatus, VolumeRequest

### Community 6 - "Bluetooth Icon Assets"
Cohesion: 0.10
Nodes (31): Default, DiscoveredBackend, Display, Error, Formatter, Option, PairLink, Result (+23 more)

### Community 7 - "i18n Translations"
Cohesion: 0.09
Nodes (45): DeviceInfo, DiscoveredBackend, NowPlaying, Option, PlaybackState, String, TargetsState, Vec (+37 more)

### Community 8 - "Dioxus 0.7"
Cohesion: 0.10
Nodes (35): Duration, HashMap, Instant, Option, Self, String, Vec, AddressState (+27 more)

### Community 9 - "Library Root"
Cohesion: 0.09
Nodes (33): Default, Option, Path, PathBuf, Result, Self, ServerConfig, String (+25 more)

### Community 10 - "Feature Spec"
Cohesion: 0.07
Nodes (23): AppSettings, NowPlaying, activate_backend(), active_at(), active_with_token(), authed_base_from(), backend_error_for(), fetch_targets() (+15 more)

### Community 11 - "Community 11"
Cohesion: 0.07
Nodes (26): AuthStore, Builder, HealthStatus, Router, MethodRouter, ServerName, SpotifyAuth, app() (+18 more)

### Community 12 - "Community 12"
Cohesion: 0.14
Nodes (41): AppError, AdapterInfo, Address, AuthUrlResponse, Path, PlaybackState, Result, ServerConfig (+33 more)

### Community 13 - "Community 13"
Cohesion: 0.15
Nodes (30): AuthUrlResponse, Client, Display, PlaybackState, Self, SpotifyAuthState, SpotifyState, TargetsState (+22 more)

### Community 14 - "Community 14"
Cohesion: 0.10
Nodes (30): HealthStatus, Into, JoinHandle, String, F, RequestBuilder, auth_header_value(), base_url_from() (+22 more)

### Community 15 - "Community 15"
Cohesion: 0.13
Nodes (27): AppSettings, DiscoveredBackend, Option, found(), known_salon(), known_salon_without_id(), test_a_backend_added_from_discovery_is_never_added_twice(), test_a_repaired_backend_is_up_to_date_on_the_next_scan() (+19 more)

### Community 16 - "Community 16"
Cohesion: 0.06
Nodes (31): AppSettings, test_a_repaired_url_survives_the_settings_blob_round_trip(), test_activate_keeps_exactly_one_backend_active(), test_activate_unknown_index_is_refused(), test_active_token_follows_the_active_backend(), test_active_url_returns_the_active_entry_url(), test_add_accepts_a_name_freed_by_a_deletion(), test_add_rejects_a_duplicate_name() (+23 more)

### Community 17 - "Community 17"
Cohesion: 0.10
Nodes (18): AtomicUsize, Arc, AtomicBool, ClientPresence, Drop, Duration, Instant, Mutex (+10 more)

### Community 18 - "Community 18"
Cohesion: 0.10
Nodes (18): DiscoveredBackend, Display, Drop, Duration, Error, Formatter, Option, Result (+10 more)

### Community 20 - "Community 20"
Cohesion: 0.14
Nodes (23): Default, Option, Path, PathBuf, Result, Self, String, BackendIdentity (+15 more)

### Community 21 - "Community 21"
Cohesion: 0.14
Nodes (25): ClientPresence, DeviceInfo, Error, Formatter, Option, Response, Result, Vec (+17 more)

### Community 22 - "Community 22"
Cohesion: 0.21
Nodes (26): Builder, Result, Router, ServerConfig, StatusCode, String, authorized(), build_app() (+18 more)

### Community 23 - "Community 23"
Cohesion: 0.15
Nodes (25): AdvertisedService, Box, Option, String, Vec, Ipv4Addr, ServiceDaemon, advertise() (+17 more)

### Community 24 - "Community 24"
Cohesion: 0.15
Nodes (22): AudioEngine, Arc, DeviceInfo, Mutex, SpeakerTarget, TargetsState, OffsetRequest, PresenceRequest (+14 more)

### Community 25 - "Community 25"
Cohesion: 0.15
Nodes (16): JNIEnv, JObject, Option, String, call_object(), clear_exception(), open_in_browser(), parse_spotify_callback() (+8 more)

### Community 26 - "Community 26"
Cohesion: 0.23
Nodes (21): AuthStore, Result, Router, StatusCode, String, SystemTime, app_with(), app_with_code_armed_at() (+13 more)

### Community 27 - "Community 27"
Cohesion: 0.13
Nodes (8): android, Blue2thPresenceService, MainActivity, IBinder, Int, Intent, Service, WryActivity

### Community 28 - "Community 28"
Cohesion: 0.14
Nodes (15): Option, Router, StatusCode, String, RouteSpec, build_app(), concrete_path(), status_of() (+7 more)

### Community 29 - "Community 29"
Cohesion: 0.16
Nodes (16): Display, Error, Formatter, Into, JNIEnv, Result, Self, String (+8 more)

### Community 30 - "Community 30"
Cohesion: 0.18
Nodes (14): AudioError, Error, Into, Response, Self, From, IntoResponse, Next (+6 more)

### Community 31 - "Community 31"
Cohesion: 0.12
Nodes (15): Assets, Async, Components, Context API, Dioxus Dependency, Errors, Fullstack, Hydration (+7 more)

### Community 32 - "Community 32"
Cohesion: 0.28
Nodes (15): AdapterInfo, Address, DeviceInfo, Item, Result, Stream, Vec, Device (+7 more)

### Community 33 - "Community 33"
Cohesion: 0.12
Nodes (15): Architecture, blue2th Roadmap — Multi-speaker audio via mobile remote + PC backend, Cross-cutting concerns, Legacy: the on-phone Android Bluetooth code, removed, Phase 0 — Foundations, Phase 1 — Bluetooth discovery (`bluer`), Phase 2 — Connect / disconnect a speaker, Phase 3 — Play audio to ONE speaker (PipeWire) (+7 more)

### Community 34 - "Community 34"
Cohesion: 0.12
Nodes (15): Acceptance Criteria, API / functions needed, Constraints & Notes, Description, Feature Name, Files to create, Files to modify, Integration tests (async) (+7 more)

### Community 35 - "Community 35"
Cohesion: 0.13
Nodes (14): 1. Determine the phase, 2. Validate the feature spec (skip if phase is `done`), 3. Create or reuse the feature worktree, 4.0 Determine the affected layers, 4. Spawn agents with targeted context, 5. Sequencing for `all`, 6. Final report, 7. `done` — cleanup after merge (+6 more)

### Community 36 - "Community 36"
Cohesion: 0.14
Nodes (13): After a Dioxus / `dx` upgrade — re-diff the frozen Android files, Architecture, Code Guidelines, Error Handling, graphify, Language, librespot stays a subprocess — never a crate, Naming (+5 more)

### Community 37 - "Community 37"
Cohesion: 0.14
Nodes (13): 01 — Going online — done, 02 — Continuous integration, 03 — Release management, 04 — User documentation, Branching model, Provenance, Publishing blue2th, Repository hygiene (+5 more)

### Community 38 - "Community 38"
Cohesion: 0.35
Nodes (11): ClientPresence, JNIEnv, JObject, Handle, arm(), Java_dev_dioxus_main_Blue2thPresenceService_nativeOnGone(), Java_dev_dioxus_main_MainActivity_nativeOnBackground(), Java_dev_dioxus_main_MainActivity_nativeOnForeground() (+3 more)

### Community 39 - "Community 39"
Cohesion: 0.17
Nodes (12): String, AdapterInfo, AuthCallbackRequest, AuthUrlResponse, ConfigRequest, normalize_pairing_code(), PairRequest, PairResponse (+4 more)

### Community 40 - "Community 40"
Cohesion: 0.26
Nodes (11): Builder, Response, Result, Router, String, TargetsState, authorized(), build_app() (+3 more)

### Community 41 - "Community 41"
Cohesion: 0.32
Nodes (9): Builder, Router, authorized(), build_app(), test_spotify_auth_callback_missing_code_returns_bad_request(), test_spotify_auth_status_on_fresh_server_is_disconnected(), test_spotify_auth_url_without_client_id_is_unavailable(), test_spotify_play_while_disconnected_returns_conflict() (+1 more)

### Community 42 - "Community 42"
Cohesion: 0.20
Nodes (11): BackendEntry, ServerConfig, ConfigRequest, config_body(), config_url(), entry(), is_last_reference(), push_active_config() (+3 more)

### Community 43 - "Community 43"
Cohesion: 0.22
Nodes (11): Option, DeviceInfo, hex_digit(), NowPlaying, NowPlayingState, PairLink, parse_pair_link(), percent_decode() (+3 more)

### Community 44 - "Community 44"
Cohesion: 0.33
Nodes (8): Builder, Router, authorized(), build_app(), test_deselect_last_speaker_empties_the_selection(), test_play_without_connected_speaker_returns_client_error(), test_playback_endpoint_returns_state(), test_volume_endpoint_rejects_malformed_body()

### Community 45 - "Community 45"
Cohesion: 0.22
Nodes (9): Option, PairLink, pair_link(), test_upsert_from_pair_link_accepts_a_nameless_link_for_a_known_url(), test_upsert_from_pair_link_creates_and_activates_the_backend(), test_upsert_from_pair_link_keeps_the_method_of_a_known_backend(), test_upsert_from_pair_link_matches_a_known_url_across_a_trailing_slash(), test_upsert_from_pair_link_records_the_qr_as_the_new_backend_method() (+1 more)

### Community 46 - "Community 46"
Cohesion: 0.32
Nodes (6): Into, Self, HealthStatus, test_health_status_ok_sets_status_field(), test_health_status_round_trips_through_json(), test_health_status_round_trips_with_auth_required()

### Community 47 - "Community 47"
Cohesion: 0.25
Nodes (8): discovered_from_txt(), DiscoveredBackend, test_discovered_from_txt_falls_back_to_the_default_name(), test_discovered_from_txt_ignores_extras_and_blank_names(), test_discovered_from_txt_reads_the_id_and_the_name(), test_discovered_from_txt_treats_a_blank_id_as_absent(), test_discovered_from_txt_without_an_id_is_not_a_rejection(), Value

### Community 48 - "Community 48"
Cohesion: 0.33
Nodes (6): Display, Error, Formatter, Result, NameError, validate_backend_name()

### Community 49 - "Community 49"
Cohesion: 0.48
Nodes (7): Item, Stream, Event, Infallible, scan(), spotify_now_playing(), Sse

### Community 50 - "Community 50"
Cohesion: 0.48
Nodes (6): Builder, Router, authorized(), build_app(), test_spotify_start_without_target_returns_bad_request(), test_spotify_status_on_fresh_server_is_stopped()

### Community 51 - "Community 51"
Cohesion: 0.29
Nodes (6): After the cycle, Before running, Rust / Dioxus notes, Structure, TDD Workflow — blue2th, Usage

### Community 52 - "Community 52"
Cohesion: 0.33
Nodes (5): Quality gates (run from the worktree root before committing), Rules, What you must NOT do, Workspace layers, Your role (GREEN phase)

### Community 53 - "Community 53"
Cohesion: 0.33
Nodes (5): Quality gates (run from the worktree root), Rules, What you must NOT do, Workspace layers, Your role (REFACTOR phase)

### Community 54 - "Community 54"
Cohesion: 0.33
Nodes (5): Hardware is NOT testable in this sandbox, Rules, What you must NOT do, Workspace layers, Your role (RED phase)

### Community 55 - "Community 55"
Cohesion: 0.53
Nodes (5): String, read(), test_application_identifier_is_pinned(), test_launcher_label_does_not_come_from_a_generated_resource(), test_launcher_label_is_the_display_name()

### Community 56 - "Community 56"
Cohesion: 0.33
Nodes (5): Affected Layers, Final Status, Issues Found & Fixed, New Tests Added, Review Report — Auto-reconnect remembered speakers

### Community 57 - "Community 57"
Cohesion: 0.40
Nodes (4): Box, Error, Result, main()

### Community 58 - "Community 58"
Cohesion: 0.50
Nodes (4): Vec, RoutingMode, SpeakerTarget, TargetsState

### Community 59 - "Community 59"
Cohesion: 0.67
Nodes (3): Option, PathBuf, state_store_path()

### Community 60 - "Community 60"
Cohesion: 0.50
Nodes (4): pair_deep_link(), test_pair_deep_link_carries_the_code(), test_pair_deep_link_round_trips_through_parse_pair_link(), test_parse_pair_link_decodes_a_multibyte_name()

## Knowledge Gaps
- **238 isolated node(s):** `android`, `IBinder`, `Int`, `Into`, `Display` (+233 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **2 thin communities (<3 nodes) omitted from report** — run `graphify query` to explore isolated nodes.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `TransportBar()` connect `i18n Translations` to `TDD Workflow & Dev Standards`, `Community 12`?**
  _High betweenness centrality (0.054) - this node is a cross-community bridge._
- **Why does `playback()` connect `Community 12` to `Community 24`, `Community 11`, `i18n Translations`?**
  _High betweenness centrality (0.046) - this node is a cross-community bridge._
- **Why does `RoutingMode` connect `Android BT JNI & Errors` to `Community 11`?**
  _High betweenness centrality (0.034) - this node is a cross-community bridge._
- **What connects `android`, `IBinder`, `Int` to the rest of the system?**
  _238 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `Android BT JNI & Errors` be split into smaller, more focused modules?**
  _Cohesion score 0.06613879832324174 - nodes in this community are weakly interconnected._
- **Should `Dioxus Framework Concepts` be split into smaller, more focused modules?**
  _Cohesion score 0.06205311542390194 - nodes in this community are weakly interconnected._
- **Should `UI App & State` be split into smaller, more focused modules?**
  _Cohesion score 0.05759493670886076 - nodes in this community are weakly interconnected._