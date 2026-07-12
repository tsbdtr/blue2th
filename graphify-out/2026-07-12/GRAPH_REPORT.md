# Graph Report - blue2th  (2026-06-25)

## Corpus Check
- 28 files · ~33,018 words
- Verdict: corpus is large enough that graph structure adds value.

## Summary
- 638 nodes · 1431 edges · 30 communities (24 shown, 6 thin omitted)
- Extraction: 98% EXTRACTED · 2% INFERRED · 0% AMBIGUOUS · INFERRED: 33 edges (avg confidence: 0.82)
- Token cost: 0 input · 0 output

## Graph Freshness
- Built from commit: `f622f101`
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

## God Nodes (most connected - your core abstractions)
1. `BluetoothError` - 32 edges
2. `Result` - 26 edges
3. `AppState` - 23 edges
4. `Result` - 22 edges
5. `Result` - 21 edges
6. `AudioEngine` - 20 edges
7. `BackendError` - 20 edges
8. `bt_err_clear()` - 20 edges
9. `Json` - 19 edges
10. `connected()` - 18 edges

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
- 1-file cycle: `blue2th-server/src/main.rs -> blue2th-server/src/main.rs`
- 1-file cycle: `src/backend.rs -> src/backend.rs`
- 1-file cycle: `src/main.rs -> src/main.rs`

## Hyperedges (group relationships)
- **Dioxus Reactive State System** — agents_signal, agents_use_signal, agents_use_memo, agents_context_api [INFERRED 0.85]
- **Dioxus Fullstack Rendering Pipeline** — agents_fullstack, agents_server_functions, agents_hydration, agents_use_server_future [EXTRACTED 1.00]
- **Quality gates enforced at commit time and in TDD agents: fmt, clippy, test, conventional commits** — githooks_pre_commit_quality_gate, githooks_commit_msg_conventional_commits, concept_clippy_quality_gate, concept_conventional_commits, claude_md_project_instructions [EXTRACTED 1.00]
- **TDD Cycle for Bluetooth Adapter State Detection** — agents_tdd_implementer, agents_tdd_reviewer, concept_tdd_red_green_refactor, tests_bluetooth_integration_test_enable_bluetooth_returns_bool, src_bluetooth_enable_bluetooth_inner, tdd_review_report [INFERRED 0.85]
- **Android Bluetooth JNI Detection Flow** — src_bluetooth_enable_bluetooth, src_bluetooth_enable_bluetooth_inner, concept_jni_android_bluetooth, concept_platform_conditional_compilation [EXTRACTED 0.95]
- **UI Bluetooth Enable Confirmation Flow** — src_main_home, src_main_confirmmodal, src_bluetooth_enable_bluetooth, src_main_connectionstatus [EXTRACTED 0.95]

## Communities (30 total, 6 thin omitted)

### Community 0 - "Android BT JNI & Errors"
Cohesion: 0.08
Nodes (84): JNI Android Bluetooth Detection, Platform-Conditional Compilation (#[cfg(target_os)]), Display, Error, Formatter, GlobalRef, Into, JavaVM (+76 more)

### Community 1 - "Dioxus Framework Concepts"
Cohesion: 0.08
Nodes (26): App Root Component, asset! Macro, Assets, Async, Component Model, Components, Context API, Dioxus Dependency (+18 more)

### Community 2 - "UI App & State"
Cohesion: 0.11
Nodes (40): Element, EventHandler, HashSet, README Project Overview, Signal, App(), BackendDeviceItem(), BackendOnline (+32 more)

### Community 3 - "TDD Workflow & Dev Standards"
Cohesion: 0.09
Nodes (21): Project context, Quality gates (run from the worktree root before committing), Rules, What you must NOT do, Workspace layers, Your role (GREEN phase), Project context, Quality gates (run from the worktree root) (+13 more)

### Community 4 - "Community 4"
Cohesion: 0.15
Nodes (27): Result, Self, SpeakerTarget, String, TargetsState, Vec, RoutingMode, clamp_offset() (+19 more)

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
Cohesion: 0.20
Nodes (9): Architecture, Code Guidelines, Error Handling, graphify, Language, Naming, Quality Commands, Rust Style (+1 more)

### Community 14 - "Community 14"
Cohesion: 0.29
Nodes (6): After the cycle, Before running, Rust / Dioxus notes, Structure, TDD Workflow — blue2th, Usage

### Community 15 - "Community 15"
Cohesion: 0.29
Nodes (6): Hardware is NOT testable in this sandbox, Project context, Rules, What you must NOT do, Workspace layers, Your role (RED phase)

### Community 16 - "Community 16"
Cohesion: 0.31
Nodes (14): Affected Layers, Final Status, Issues Found & Fixed, Items Reviewed — No Change Needed, New Tests Added, Notes, Review focus verification (no change needed), Review Report — Fan-out playback to two speakers (+6 more)

### Community 19 - "Community 19"
Cohesion: 0.39
Nodes (3): BluetoothProfile, A2dpServiceListener, Override

### Community 21 - "Community 21"
Cohesion: 0.12
Nodes (36): backend_base_url(), BackendError, connect_device(), describe(), deselect_target(), device_action_url(), disconnect_device(), fetch_devices() (+28 more)

### Community 22 - "Community 22"
Cohesion: 0.12
Nodes (16): Architecture, blue2th Roadmap — Multi-speaker audio via mobile remote + PC backend, Cross-cutting concerns, Legacy: existing Android Bluetooth code, Phase 0 — Foundations, Phase 1 — Bluetooth discovery (`bluer`), Phase 2 — Connect / disconnect a speaker, Phase 3 — Play audio to ONE speaker (PipeWire) (+8 more)

### Community 23 - "Community 23"
Cohesion: 0.07
Nodes (74): AppError, Arc, AudioEngine, AdapterInfo, Address, Arc, Box, DeviceInfo (+66 more)

### Community 24 - "Community 24"
Cohesion: 0.09
Nodes (17): Into, Option, Self, String, Vec, AdapterInfo, DeviceInfo, HealthStatus (+9 more)

### Community 26 - "Community 26"
Cohesion: 0.27
Nodes (14): AdapterInfo, Address, DeviceInfo, Item, Result, connect_device(), disconnect_device(), Stream (+6 more)

### Community 27 - "Community 27"
Cohesion: 0.07
Nodes (48): AtomicBool, Arc, AudioError, Box, Display, Error, Formatter, Option (+40 more)

### Community 28 - "Community 28"
Cohesion: 0.17
Nodes (13): AudioError, Error, Into, Self, Error, Self, From, IntoResponse (+5 more)

### Community 29 - "Community 29"
Cohesion: 0.33
Nodes (6): Router, build_app(), test_play_links_running_output_node_to_pipewire_sink(), test_play_without_connected_speaker_returns_client_error(), test_playback_endpoint_returns_state(), test_volume_endpoint_rejects_malformed_body()

## Knowledge Gaps
- **170 isolated node(s):** `Into`, `Self`, `Option`, `VolumeRequest`, `Vec` (+165 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **6 thin communities (<3 nodes) omitted from report** — run `graphify query` to explore isolated nodes.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `Duration` connect `Community 23` to `Android BT JNI & Errors`, `Community 21`?**
  _High betweenness centrality (0.116) - this node is a cross-community bridge._
- **Why does `obtain_profile_proxy()` connect `Android BT JNI & Errors` to `Community 23`?**
  _High betweenness centrality (0.050) - this node is a cross-community bridge._
- **Why does `obtain_a2dp_proxy()` connect `Android BT JNI & Errors` to `Community 23`?**
  _High betweenness centrality (0.035) - this node is a cross-community bridge._
- **What connects `Into`, `Self`, `Option` to the rest of the system?**
  _172 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `Android BT JNI & Errors` be split into smaller, more focused modules?**
  _Cohesion score 0.07596751075011944 - nodes in this community are weakly interconnected._
- **Should `Dioxus Framework Concepts` be split into smaller, more focused modules?**
  _Cohesion score 0.08262108262108261 - nodes in this community are weakly interconnected._
- **Should `UI App & State` be split into smaller, more focused modules?**
  _Cohesion score 0.10545790934320073 - nodes in this community are weakly interconnected._