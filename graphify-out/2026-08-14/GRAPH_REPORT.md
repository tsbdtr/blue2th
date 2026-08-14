# Graph Report - blue2th  (2026-08-14)

## Corpus Check
- 40 files · ~53,756 words
- Verdict: corpus is large enough that graph structure adds value.

## Summary
- 1025 nodes · 2363 edges · 46 communities (40 shown, 6 thin omitted)
- Extraction: 98% EXTRACTED · 2% INFERRED · 0% AMBIGUOUS · INFERRED: 39 edges (avg confidence: 0.82)
- Token cost: 0 input · 0 output

## Graph Freshness
- Built from commit: `97e2be1b`
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

## God Nodes (most connected - your core abstractions)
1. `AppState` - 45 edges
2. `BackendError` - 32 edges
3. `BluetoothError` - 32 edges
4. `Result` - 29 edges
5. `Result` - 29 edges
6. `connected()` - 28 edges
7. `Json` - 26 edges
8. `SpotifyAuth` - 26 edges
9. `State` - 25 edges
10. `AppError` - 24 edges

## Surprising Connections (you probably didn't know these)
- `README Project Overview` --references--> `App()`  [INFERRED]
  README.md → src/main.rs
- `test_bt_enabled_stays_false_when_enable_bluetooth_returns_false` --semantically_similar_to--> `ConfirmModal()`  [INFERRED] [semantically similar]
  tests/bluetooth_integration.rs → src/main.rs
- `tdd-test-writer Agent (RED phase)` --references--> `scan_devices()`  [EXTRACTED]
  .claude/agents/tdd-test-writer.md → src/bluetooth.rs
- `Review Report — Real BT Adapter State Detection` --references--> `enable_bluetooth_inner()`  [EXTRACTED]
  tdd/REVIEW.md → src/bluetooth.rs
- `test_connect_device_simulated_ok_on_non_android()` --calls--> `connect_device()`  [INFERRED]
  tests/a2dp_dex_listener.rs → src/bluetooth.rs

## Import Cycles
- 1-file cycle: `blue2th-server/src/lib.rs -> blue2th-server/src/lib.rs`
- 1-file cycle: `blue2th-server/src/targets.rs -> blue2th-server/src/targets.rs`
- 1-file cycle: `blue2th-server/src/main.rs -> blue2th-server/src/main.rs`
- 1-file cycle: `blue2th-server/src/spotify.rs -> blue2th-server/src/spotify.rs`
- 1-file cycle: `src/main.rs -> src/main.rs`
- 1-file cycle: `blue2th-server/src/state_store.rs -> blue2th-server/src/state_store.rs`
- 1-file cycle: `blue2th-server/src/watchdog.rs -> blue2th-server/src/watchdog.rs`
- 1-file cycle: `src/backend.rs -> src/backend.rs`
- 1-file cycle: `src/lifecycle.rs -> src/lifecycle.rs`

## Hyperedges (group relationships)
- **Dioxus Reactive State System** — agents_signal, agents_use_signal, agents_use_memo, agents_context_api [INFERRED 0.85]
- **Dioxus Fullstack Rendering Pipeline** — agents_fullstack, agents_server_functions, agents_hydration, agents_use_server_future [EXTRACTED 1.00]
- **Quality gates enforced at commit time and in TDD agents: fmt, clippy, test, conventional commits** — githooks_pre_commit_quality_gate, githooks_commit_msg_conventional_commits, concept_clippy_quality_gate, concept_conventional_commits, claude_md_project_instructions [EXTRACTED 1.00]
- **TDD Cycle for Bluetooth Adapter State Detection** — agents_tdd_implementer, agents_tdd_reviewer, concept_tdd_red_green_refactor, tests_bluetooth_integration_test_enable_bluetooth_returns_bool, src_bluetooth_enable_bluetooth_inner, tdd_review_report [INFERRED 0.85]
- **Android Bluetooth JNI Detection Flow** — src_bluetooth_enable_bluetooth, src_bluetooth_enable_bluetooth_inner, concept_jni_android_bluetooth, concept_platform_conditional_compilation [EXTRACTED 0.95]
- **UI Bluetooth Enable Confirmation Flow** — src_main_home, src_main_confirmmodal, src_bluetooth_enable_bluetooth, src_main_connectionstatus [EXTRACTED 0.95]

## Communities (46 total, 6 thin omitted)

### Community 0 - "Android BT JNI & Errors"
Cohesion: 0.07
Nodes (87): JNI Android Bluetooth Detection, Platform-Conditional Compilation (#[cfg(target_os)]), Display, Error, Formatter, GlobalRef, Into, JavaVM (+79 more)

### Community 1 - "Dioxus Framework Concepts"
Cohesion: 0.08
Nodes (26): App Root Component, asset! Macro, Assets, Async, Component Model, Components, Context API, Dioxus Dependency (+18 more)

### Community 2 - "UI App & State"
Cohesion: 0.10
Nodes (49): Element, EventHandler, HashSet, NowPlayingState, README Project Overview, Signal, SpotifyAction, App() (+41 more)

### Community 3 - "TDD Workflow & Dev Standards"
Cohesion: 0.09
Nodes (21): Project context, Quality gates (run from the worktree root before committing), Rules, What you must NOT do, Workspace layers, Your role (GREEN phase), Project context, Quality gates (run from the worktree root) (+13 more)

### Community 4 - "Community 4"
Cohesion: 0.10
Nodes (54): Option, Path, PathBuf, Result, Self, SpeakerTarget, String, TargetsState (+46 more)

### Community 5 - "Header SVG Assets"
Cohesion: 0.40
Nodes (6): Blue2th Brand Name (text), Dark Background with Cyan and Red-Orange Accent Colors, Animated Device / Screen UI Element, Lightbulb Icon (Bluetooth symbol motif), Animated SVG Header Logo, Tagline / Subtitle Text (is inter / is intersectable / is interesting)

### Community 6 - "Bluetooth Icon Assets"
Cohesion: 0.50
Nodes (4): 24x24 Icon Format, Blue Color Scheme (#2563eb), Bluetooth Symbol, Bluetooth SVG Icon

### Community 11 - "Community 11"
Cohesion: 0.12
Nodes (16): 1. Determine the phase, 1. Validate the feature spec, 2. Determine the phase, 2. Validate the feature spec (skip if phase is `done`), 3. Create or reuse the feature worktree, 4.0 Determine the affected layers, 4. Spawn agents with targeted context, 5. Sequencing for `all` (+8 more)

### Community 12 - "Community 12"
Cohesion: 0.11
Nodes (17): Acceptance Criteria, API / functions needed, Constraints & Notes, Description, Feature Name, Files to create, Files to modify, Integration tests (async) (+9 more)

### Community 13 - "Community 13"
Cohesion: 0.18
Nodes (10): After a Dioxus / `dx` upgrade — re-diff the frozen Android files, Architecture, Code Guidelines, Error Handling, graphify, Language, Naming, Quality Commands (+2 more)

### Community 14 - "Community 14"
Cohesion: 0.29
Nodes (6): After the cycle, Before running, Rust / Dioxus notes, Structure, TDD Workflow — blue2th, Usage

### Community 15 - "Community 15"
Cohesion: 0.29
Nodes (6): Hardware is NOT testable in this sandbox, Project context, Rules, What you must NOT do, Workspace layers, Your role (RED phase)

### Community 16 - "Community 16"
Cohesion: 0.21
Nodes (22): Affected Layers, Correctness Review (SpotifyBackend lifecycle), Final Status, Issues Found & Fixed, Items Reviewed — No Change Needed, Manual (hardware/process) seam — not CI-testable, Minor Notes (not blocking), New Tests Added (+14 more)

### Community 19 - "Community 19"
Cohesion: 0.39
Nodes (3): BluetoothProfile, A2dpServiceListener, Override

### Community 21 - "Community 21"
Cohesion: 0.08
Nodes (60): F, backend_base_url(), backend_error_message(), BackendError, connect_device(), describe(), deselect_target(), device_action_url() (+52 more)

### Community 22 - "Community 22"
Cohesion: 0.12
Nodes (16): Architecture, blue2th Roadmap — Multi-speaker audio via mobile remote + PC backend, Cross-cutting concerns, Legacy: existing Android Bluetooth code, Phase 0 — Foundations, Phase 1 — Bluetooth discovery (`bluer`), Phase 2 — Connect / disconnect a speaker, Phase 3 — Play audio to ONE speaker (PipeWire) (+8 more)

### Community 23 - "Community 23"
Cohesion: 0.19
Nodes (29): AppError, AuthUrlResponse, PlaybackState, Result, SpotifyAuthState, SpotifyState, Bytes, Json (+21 more)

### Community 24 - "Community 24"
Cohesion: 0.06
Nodes (27): Into, Option, Self, String, Vec, AdapterInfo, AuthCallbackRequest, AuthUrlResponse (+19 more)

### Community 26 - "Community 26"
Cohesion: 0.27
Nodes (14): AdapterInfo, Address, DeviceInfo, Item, Result, connect_device(), disconnect_device(), Stream (+6 more)

### Community 27 - "Community 27"
Cohesion: 0.06
Nodes (55): AtomicBool, Arc, AtomicBool, AudioError, Box, Default, Display, Error (+47 more)

### Community 28 - "Community 28"
Cohesion: 0.16
Nodes (15): AudioError, Error, Into, Response, Self, Error, Self, From (+7 more)

### Community 29 - "Community 29"
Cohesion: 0.31
Nodes (7): Router, build_app(), test_deselect_last_speaker_empties_the_selection(), test_play_links_running_output_node_to_pipewire_sink(), test_play_without_connected_speaker_returns_client_error(), test_playback_endpoint_returns_state(), test_volume_endpoint_rejects_malformed_body()

### Community 30 - "Community 30"
Cohesion: 0.09
Nodes (27): Default, Display, Error, Formatter, Option, Result, Self, SpeakerTarget (+19 more)

### Community 31 - "Community 31"
Cohesion: 0.60
Nodes (4): Router, build_app(), test_spotify_start_without_target_returns_bad_request(), test_spotify_status_on_fresh_server_is_stopped()

### Community 32 - "Community 32"
Cohesion: 0.06
Nodes (44): Default, Display, Error, Formatter, NowPlaying, Option, Path, PathBuf (+36 more)

### Community 33 - "Community 33"
Cohesion: 0.10
Nodes (18): AtomicUsize, Arc, AtomicBool, ClientPresence, Duration, Mutex, Option, Self (+10 more)

### Community 34 - "Community 34"
Cohesion: 0.14
Nodes (18): call_object(), clear_exception(), hex_digit(), open_in_browser(), parse_spotify_callback(), percent_decode(), JNIEnv, JObject (+10 more)

### Community 35 - "Community 35"
Cohesion: 0.13
Nodes (8): android, Blue2thPresenceService, MainActivity, IBinder, Int, Intent, Service, WryActivity

### Community 36 - "Community 36"
Cohesion: 0.35
Nodes (11): Handle, arm(), Java_dev_dioxus_main_Blue2thPresenceService_nativeOnGone(), Java_dev_dioxus_main_MainActivity_nativeOnBackground(), Java_dev_dioxus_main_MainActivity_nativeOnForeground(), Java_dev_dioxus_main_MainActivity_nativeOnGone(), report(), report_gone_blocking() (+3 more)

### Community 37 - "Community 37"
Cohesion: 0.31
Nodes (7): Router, build_app(), test_spotify_auth_callback_missing_code_returns_bad_request(), test_spotify_auth_status_on_fresh_server_is_disconnected(), test_spotify_auth_url_without_client_id_is_unavailable(), test_spotify_play_while_disconnected_returns_conflict(), test_spotify_transport_actions_while_disconnected_return_conflict()

### Community 38 - "Community 38"
Cohesion: 0.13
Nodes (17): Arc, Box, HealthStatus, Mutex, Router, Mutex, SpeakerTargets, SpotifyAuth (+9 more)

### Community 39 - "Community 39"
Cohesion: 0.16
Nodes (21): AdapterInfo, Address, Box, DeviceInfo, HealthStatus, Result, app(), main() (+13 more)

### Community 40 - "Community 40"
Cohesion: 0.26
Nodes (12): Address, Path, String, TargetsState, OffsetRequest, Path, connect(), deselect_target() (+4 more)

### Community 41 - "Community 41"
Cohesion: 0.23
Nodes (13): AudioEngine, Arc, Option, SpeakerTarget, PresenceRequest, SpotifyBackend, apply_offset_live(), apply_selection_change() (+5 more)

### Community 42 - "Community 42"
Cohesion: 0.33
Nodes (10): Item, Stream, Item, Stream, Event, Infallible, scan(), spotify_now_playing() (+2 more)

### Community 43 - "Community 43"
Cohesion: 0.29
Nodes (9): Response, Result, Router, String, TargetsState, build_app(), targets_state(), test_offset_route_on_unselected_speaker_reports_empty_selection() (+1 more)

### Community 44 - "Community 44"
Cohesion: 0.40
Nodes (6): AdapterInfo, DeviceInfo, Vec, adapters(), devices(), sync_connected()

### Community 45 - "Community 45"
Cohesion: 0.67
Nodes (3): Option, PathBuf, state_store_path()

## Knowledge Gaps
- **208 isolated node(s):** `android`, `IBinder`, `Int`, `Into`, `Self` (+203 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **6 thin communities (<3 nodes) omitted from report** — run `graphify query` to explore isolated nodes.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `Duration` connect `Community 39` to `Android BT JNI & Errors`, `Community 21`, `Community 38`?**
  _High betweenness centrality (0.114) - this node is a cross-community bridge._
- **Why does `NowPlayingState` connect `UI App & State` to `Community 32`?**
  _High betweenness centrality (0.080) - this node is a cross-community bridge._
- **What connects `android`, `IBinder`, `Int` to the rest of the system?**
  _210 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `Android BT JNI & Errors` be split into smaller, more focused modules?**
  _Cohesion score 0.0748040313549832 - nodes in this community are weakly interconnected._
- **Should `Dioxus Framework Concepts` be split into smaller, more focused modules?**
  _Cohesion score 0.08262108262108261 - nodes in this community are weakly interconnected._
- **Should `UI App & State` be split into smaller, more focused modules?**
  _Cohesion score 0.09545454545454546 - nodes in this community are weakly interconnected._
- **Should `TDD Workflow & Dev Standards` be split into smaller, more focused modules?**
  _Cohesion score 0.09486166007905138 - nodes in this community are weakly interconnected._