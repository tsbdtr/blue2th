# Graph Report - blue2th  (2026-06-17)

## Corpus Check
- 24 files · ~21,287 words
- Verdict: corpus is large enough that graph structure adds value.

## Summary
- 377 nodes · 702 edges · 27 communities (21 shown, 6 thin omitted)
- Extraction: 95% EXTRACTED · 5% INFERRED · 0% AMBIGUOUS · INFERRED: 32 edges (avg confidence: 0.82)
- Token cost: 0 input · 0 output

## Graph Freshness
- Built from commit: `3d72a99a`
- Run `git rev-parse HEAD` and compare to check if the graph is stale.
- Run `graphify update .` after code changes (no API cost).

## Community Hubs (Navigation)
- [[_COMMUNITY_Android BT JNI & Errors|Android BT JNI & Errors]]
- [[_COMMUNITY_Dioxus Framework Concepts|Dioxus Framework Concepts]]
- [[_COMMUNITY_UI App & State|UI App & State]]
- [[_COMMUNITY_TDD Workflow & Dev Standards|TDD Workflow & Dev Standards]]
- [[_COMMUNITY_BT Integration Tests & UI Flows|BT Integration Tests & UI Flows]]
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

## God Nodes (most connected - your core abstractions)
1. `BluetoothError` - 30 edges
2. `Result` - 22 edges
3. `bt_err_clear()` - 20 edges
4. `Result` - 19 edges
5. `enable_bluetooth_inner()` - 16 edges
6. `android_jni_env()` - 15 edges
7. `obtain_profile_proxy()` - 15 edges
8. `scan_devices()` - 14 edges
9. `connected_device_names()` - 14 edges
10. `find_device_by_name()` - 14 edges

## Surprising Connections (you probably didn't know these)
- `test_bt_enabled_stays_false_when_enable_bluetooth_returns_false` --semantically_similar_to--> `ConfirmModal()`  [INFERRED] [semantically similar]
  tests/bluetooth_integration.rs → src/main.rs
- `test_no_fake_activation_when_bt_returns_error()` --semantically_similar_to--> `ConfirmModal()`  [INFERRED] [semantically similar]
  tests/bluetooth_integration.rs → src/main.rs
- `README Project Overview` --references--> `App()`  [INFERRED]
  README.md → src/main.rs
- `tdd-test-writer Agent (RED phase)` --references--> `scan_devices()`  [EXTRACTED]
  .claude/agents/tdd-test-writer.md → src/bluetooth.rs
- `test_connect_device_simulated_ok_on_non_android()` --calls--> `connect_device()`  [INFERRED]
  tests/a2dp_dex_listener.rs → src/bluetooth.rs

## Import Cycles
- 1-file cycle: `blue2th-server/src/main.rs -> blue2th-server/src/main.rs`
- 1-file cycle: `src/backend.rs -> src/backend.rs`

## Hyperedges (group relationships)
- **Dioxus Reactive State System** — agents_signal, agents_use_signal, agents_use_memo, agents_context_api [INFERRED 0.85]
- **Dioxus Fullstack Rendering Pipeline** — agents_fullstack, agents_server_functions, agents_hydration, agents_use_server_future [EXTRACTED 1.00]
- **Quality gates enforced at commit time and in TDD agents: fmt, clippy, test, conventional commits** — githooks_pre_commit_quality_gate, githooks_commit_msg_conventional_commits, concept_clippy_quality_gate, concept_conventional_commits, claude_md_project_instructions [EXTRACTED 1.00]
- **TDD Cycle for Bluetooth Adapter State Detection** — agents_tdd_implementer, agents_tdd_reviewer, concept_tdd_red_green_refactor, tests_bluetooth_integration_test_enable_bluetooth_returns_bool, src_bluetooth_enable_bluetooth_inner, tdd_review_report [INFERRED 0.85]
- **Android Bluetooth JNI Detection Flow** — src_bluetooth_enable_bluetooth, src_bluetooth_enable_bluetooth_inner, concept_jni_android_bluetooth, concept_platform_conditional_compilation [EXTRACTED 0.95]
- **UI Bluetooth Enable Confirmation Flow** — src_main_home, src_main_confirmmodal, src_bluetooth_enable_bluetooth, src_main_connectionstatus [EXTRACTED 0.95]

## Communities (27 total, 6 thin omitted)

### Community 0 - "Android BT JNI & Errors"
Cohesion: 0.10
Nodes (68): Display, Duration, Error, Formatter, GlobalRef, Into, JavaVM, JClass (+60 more)

### Community 1 - "Dioxus Framework Concepts"
Cohesion: 0.08
Nodes (26): App Root Component, asset! Macro, Assets, Async, Component Model, Components, Context API, Dioxus Dependency (+18 more)

### Community 2 - "UI App & State"
Cohesion: 0.13
Nodes (31): Element, EventHandler, README Project Overview, Signal, enable_bluetooth(), App(), BackendOnline, BackendScan() (+23 more)

### Community 3 - "TDD Workflow & Dev Standards"
Cohesion: 0.12
Nodes (17): Project context, Rules, What you must NOT do, Your role (GREEN phase), Project context, Rules, What you must NOT do, Your role (REFACTOR phase) (+9 more)

### Community 4 - "BT Integration Tests & UI Flows"
Cohesion: 0.12
Nodes (17): JNI Android Bluetooth Detection, Platform-Conditional Compilation (#[cfg(target_os)]), connected_device_names(), enable_bluetooth_inner(), bluetooth module (lib re-export), Review Report — Real BT Adapter State Detection, test_bt_enabled_stays_false_when_enable_bluetooth_returns_false, test_connected_device_names_dispatches_to_inner() (+9 more)

### Community 5 - "Header SVG Assets"
Cohesion: 0.40
Nodes (6): Blue2th Brand Name (text), Dark Background with Cyan and Red-Orange Accent Colors, Animated Device / Screen UI Element, Lightbulb Icon (Bluetooth symbol motif), Animated SVG Header Logo, Tagline / Subtitle Text (is inter / is intersectable / is interesting)

### Community 6 - "Bluetooth Icon Assets"
Cohesion: 0.50
Nodes (4): 24x24 Icon Format, Blue Color Scheme (#2563eb), Bluetooth Symbol, Bluetooth SVG Icon

### Community 11 - "Community 11"
Cohesion: 0.12
Nodes (15): 1. Determine the phase, 1. Validate the feature spec, 2. Determine the phase, 2. Validate the feature spec (skip if phase is `done`), 3. Create or reuse the feature worktree, 4. Spawn agents with targeted context, 5. Sequencing for `all`, 6. Final report (+7 more)

### Community 12 - "Community 12"
Cohesion: 0.13
Nodes (14): Acceptance Criteria, Constraints & Notes, Description, Feature Name, Files to create, Files to modify, Integration tests (async, server functions), Nominal Scenario (+6 more)

### Community 13 - "Community 13"
Cohesion: 0.22
Nodes (8): Code Guidelines, Error Handling, graphify, Language, Naming, Quality Commands, Rust Style, tdd

### Community 14 - "Community 14"
Cohesion: 0.29
Nodes (6): After the cycle, Before running, Rust / Dioxus notes, Structure, TDD Workflow — blue2th, Usage

### Community 15 - "Community 15"
Cohesion: 0.40
Nodes (4): Project context, Rules, What you must NOT do, Your role (RED phase)

### Community 16 - "Community 16"
Cohesion: 0.40
Nodes (10): Final Status, Issues Found & Fixed, Items Reviewed — No Change Needed, New Tests Added, Review focus verification (no change needed), Review Report — Load bonded Bluetooth devices, Review Report — Real A2DP connect/disconnect via embedded ServiceListener `.dex`, Review Report — Real Bluetooth A2DP Connection (+2 more)

### Community 19 - "Community 19"
Cohesion: 0.39
Nodes (3): BluetoothProfile, A2dpServiceListener, Override

### Community 21 - "Community 21"
Cohesion: 0.12
Nodes (18): backend_base_url(), BackendError, describe(), health_url(), ping_backend(), DeviceInfo, Display, Error (+10 more)

### Community 22 - "Community 22"
Cohesion: 0.12
Nodes (15): Architecture, blue2th Roadmap — Multi-speaker audio via mobile remote + PC backend, Cross-cutting concerns, Legacy: existing Android Bluetooth code, Phase 0 — Foundations, Phase 1 — Bluetooth discovery (`bluer`), Phase 2 — Connect / disconnect a speaker, Phase 3 — Play audio to ONE speaker (PipeWire) (+7 more)

### Community 23 - "Community 23"
Cohesion: 0.09
Nodes (28): AppError, AdapterInfo, DeviceInfo, Error, HealthStatus, Item, Result, Self (+20 more)

### Community 24 - "Community 24"
Cohesion: 0.22
Nodes (9): Into, Option, Self, String, AdapterInfo, DeviceInfo, HealthStatus, test_health_status_ok_sets_status_field() (+1 more)

### Community 26 - "Community 26"
Cohesion: 0.27
Nodes (11): AdapterInfo, DeviceInfo, Item, Result, Stream, Vec, Device, device_info() (+3 more)

## Knowledge Gaps
- **131 isolated node(s):** `Into`, `Self`, `Option`, `AdapterInfo`, `Stream` (+126 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **6 thin communities (<3 nodes) omitted from report** — run `graphify query` to explore isolated nodes.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `Duration` connect `Android BT JNI & Errors` to `Community 21`, `Community 23`?**
  _High betweenness centrality (0.140) - this node is a cross-community bridge._
- **What connects `Into`, `Self`, `Option` to the rest of the system?**
  _133 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `Android BT JNI & Errors` be split into smaller, more focused modules?**
  _Cohesion score 0.10093896713615023 - nodes in this community are weakly interconnected._
- **Should `Dioxus Framework Concepts` be split into smaller, more focused modules?**
  _Cohesion score 0.08262108262108261 - nodes in this community are weakly interconnected._
- **Should `UI App & State` be split into smaller, more focused modules?**
  _Cohesion score 0.12802275960170698 - nodes in this community are weakly interconnected._
- **Should `TDD Workflow & Dev Standards` be split into smaller, more focused modules?**
  _Cohesion score 0.11695906432748537 - nodes in this community are weakly interconnected._
- **Should `BT Integration Tests & UI Flows` be split into smaller, more focused modules?**
  _Cohesion score 0.11904761904761904 - nodes in this community are weakly interconnected._