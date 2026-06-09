# Graph Report - blue2th  (2026-06-10)

## Corpus Check
- 16 files · ~11,395 words
- Verdict: corpus is large enough that graph structure adds value.

## Summary
- 190 nodes · 263 edges · 18 communities (14 shown, 4 thin omitted)
- Extraction: 92% EXTRACTED · 8% INFERRED · 0% AMBIGUOUS · INFERRED: 20 edges (avg confidence: 0.83)
- Token cost: 0 input · 0 output

## Graph Freshness
- Built from commit: `ca4bd62a`
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

## God Nodes (most connected - your core abstractions)
1. `BluetoothError` - 15 edges
2. `enable_bluetooth_inner()` - 14 edges
3. `scan_devices()` - 12 edges
4. `scan_devices_inner()` - 12 edges
5. `DeviceItem()` - 12 edges
6. `Result` - 10 edges
7. `android_jni_env()` - 9 edges
8. `request_enable_bluetooth_inner()` - 9 edges
9. `bt_err_clear()` - 8 edges
10. `Steps` - 8 edges

## Surprising Connections (you probably didn't know these)
- `test_bt_enabled_stays_false_when_enable_bluetooth_returns_false` --semantically_similar_to--> `ConfirmModal()`  [INFERRED] [semantically similar]
  tests/bluetooth_integration.rs → src/main.rs
- `README Project Overview` --references--> `App()`  [INFERRED]
  README.md → src/main.rs
- `test_no_fake_activation_when_bt_returns_error()` --semantically_similar_to--> `ConfirmModal()`  [INFERRED] [semantically similar]
  tests/bluetooth_integration.rs → src/main.rs
- `tdd-test-writer Agent (RED phase)` --references--> `scan_devices()`  [EXTRACTED]
  .claude/agents/tdd-test-writer.md → src/bluetooth.rs
- `test_scan_devices_error_path_does_not_panic()` --calls--> `scan_devices()`  [INFERRED]
  tests/bluetooth_integration.rs → src/bluetooth.rs

## Import Cycles
- None detected.

## Hyperedges (group relationships)
- **Dioxus Reactive State System** — agents_signal, agents_use_signal, agents_use_memo, agents_context_api [INFERRED 0.85]
- **Dioxus Fullstack Rendering Pipeline** — agents_fullstack, agents_server_functions, agents_hydration, agents_use_server_future [EXTRACTED 1.00]
- **Quality gates enforced at commit time and in TDD agents: fmt, clippy, test, conventional commits** — githooks_pre_commit_quality_gate, githooks_commit_msg_conventional_commits, concept_clippy_quality_gate, concept_conventional_commits, claude_md_project_instructions [EXTRACTED 1.00]
- **TDD Cycle for Bluetooth Adapter State Detection** — agents_tdd_implementer, agents_tdd_reviewer, concept_tdd_red_green_refactor, tests_bluetooth_integration_test_enable_bluetooth_returns_bool, src_bluetooth_enable_bluetooth_inner, tdd_review_report [INFERRED 0.85]
- **Android Bluetooth JNI Detection Flow** — src_bluetooth_enable_bluetooth, src_bluetooth_enable_bluetooth_inner, concept_jni_android_bluetooth, concept_platform_conditional_compilation [EXTRACTED 0.95]
- **UI Bluetooth Enable Confirmation Flow** — src_main_home, src_main_confirmmodal, src_bluetooth_enable_bluetooth, src_main_connectionstatus [EXTRACTED 0.95]

## Communities (18 total, 4 thin omitted)

### Community 0 - "Android BT JNI & Errors"
Cohesion: 0.11
Nodes (28): Display, Error, Formatter, Into, JavaVM, JNIEnv, Result, Self (+20 more)

### Community 1 - "Dioxus Framework Concepts"
Cohesion: 0.08
Nodes (26): App Root Component, asset! Macro, Assets, Async, Component Model, Components, Context API, Dioxus Dependency (+18 more)

### Community 2 - "UI App & State"
Cohesion: 0.22
Nodes (16): Element, EventHandler, README Project Overview, Signal, enable_bluetooth(), App(), ConfirmModal(), ConnectionStatus (+8 more)

### Community 3 - "TDD Workflow & Dev Standards"
Cohesion: 0.12
Nodes (17): Project context, Rules, What you must NOT do, Your role (GREEN phase), Project context, Rules, What you must NOT do, Your role (REFACTOR phase) (+9 more)

### Community 4 - "BT Integration Tests & UI Flows"
Cohesion: 0.13
Nodes (13): JNI Android Bluetooth Detection, Platform-Conditional Compilation (#[cfg(target_os)]), enable_bluetooth_inner(), test_enable_bluetooth_inner_returns_ok_on_non_android(), bluetooth module (lib re-export), Review Report — Real BT Adapter State Detection, test_bt_enabled_stays_false_when_enable_bluetooth_returns_false, test_enable_bluetooth_returns_bool() (+5 more)

### Community 5 - "Header SVG Assets"
Cohesion: 0.40
Nodes (6): Blue2th Brand Name (text), Dark Background with Cyan and Red-Orange Accent Colors, Animated Device / Screen UI Element, Lightbulb Icon (Bluetooth symbol motif), Animated SVG Header Logo, Tagline / Subtitle Text (is inter / is intersectable / is interesting)

### Community 6 - "Bluetooth Icon Assets"
Cohesion: 0.50
Nodes (4): 24x24 Icon Format, Blue Color Scheme (#2563eb), Bluetooth Symbol, Bluetooth SVG Icon

### Community 11 - "Community 11"
Cohesion: 0.14
Nodes (13): 1. Validate the feature spec, 2. Determine the phase, 3. Create or reuse the feature worktree, 4. Spawn agents with targeted context, 5. Sequencing for `all`, 6. Final report, 7. `done` — cleanup after merge, GREEN — tdd-implementer (+5 more)

### Community 12 - "Community 12"
Cohesion: 0.15
Nodes (12): Acceptance Criteria, Constraints & Notes, Description, Feature Name, Files to create, Files to modify, Integration tests (async, server functions), Server functions needed (+4 more)

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
Cohesion: 0.53
Nodes (5): Final Status, Issues Found & Fixed, New Tests Added, Review Report — Load bonded Bluetooth devices, Review Report — Replace confirm modal with Android Bluetooth enable dialog

## Knowledge Gaps
- **79 isolated node(s):** `Display`, `Formatter`, `Into`, `Self`, `JavaVM` (+74 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **4 thin communities (<3 nodes) omitted from report** — run `graphify query` to explore isolated nodes.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `scan_devices()` connect `Android BT JNI & Errors` to `UI App & State`, `TDD Workflow & Dev Standards`, `BT Integration Tests & UI Flows`?**
  _High betweenness centrality (0.064) - this node is a cross-community bridge._
- **Why does `enable_bluetooth_inner()` connect `BT Integration Tests & UI Flows` to `Android BT JNI & Errors`, `UI App & State`?**
  _High betweenness centrality (0.046) - this node is a cross-community bridge._
- **Why does `tdd-test-writer Agent (RED phase)` connect `TDD Workflow & Dev Standards` to `Android BT JNI & Errors`?**
  _High betweenness centrality (0.045) - this node is a cross-community bridge._
- **Are the 3 inferred relationships involving `scan_devices()` (e.g. with `test_scan_devices_dispatches_to_inner()` and `test_scan_devices_error_path_does_not_panic()`) actually correct?**
  _`scan_devices()` has 3 INFERRED edges - model-reasoned connections that need verification._
- **Are the 2 inferred relationships involving `scan_devices_inner()` (e.g. with `test_scan_devices_dispatches_to_inner()` and `test_scan_devices_simulation_non_empty()`) actually correct?**
  _`scan_devices_inner()` has 2 INFERRED edges - model-reasoned connections that need verification._
- **What connects `Display`, `Formatter`, `Into` to the rest of the system?**
  _81 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `Android BT JNI & Errors` be split into smaller, more focused modules?**
  _Cohesion score 0.11261261261261261 - nodes in this community are weakly interconnected._