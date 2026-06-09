# Graph Report - .  (2026-06-07)

## Corpus Check
- Corpus is ~9,137 words - fits in a single context window. You may not need a graph.

## Summary
- 87 nodes · 123 edges · 11 communities (8 shown, 3 thin omitted)
- Extraction: 90% EXTRACTED · 10% INFERRED · 0% AMBIGUOUS · INFERRED: 12 edges (avg confidence: 0.85)
- Token cost: 0 input · 0 output

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

## God Nodes (most connected - your core abstractions)
1. `enable_bluetooth_inner()` - 12 edges
2. `DeviceItem()` - 12 edges
3. `BluetoothError` - 10 edges
4. `scan_devices()` - 7 edges
5. `Home()` - 7 edges
6. `ConfirmModal()` - 7 edges
7. `Result` - 6 edges
8. `connect_device()` - 5 edges
9. `disconnect_device()` - 5 edges
10. `enable_bluetooth()` - 5 edges

## Surprising Connections (you probably didn't know these)
- `test_bt_enabled_stays_false_when_enable_bluetooth_returns_false` --semantically_similar_to--> `ConfirmModal()`  [INFERRED] [semantically similar]
  tests/bluetooth_integration.rs → src/main.rs
- `tdd-test-writer Agent (RED phase)` --references--> `scan_devices()`  [EXTRACTED]
  /home/orel/workspace/blue2th/.claude/agents/tdd-test-writer.md → src/bluetooth.rs
- `Review Report — Real BT Adapter State Detection` --references--> `enable_bluetooth_inner()`  [EXTRACTED]
  tdd/REVIEW.md → src/bluetooth.rs
- `README Project Overview` --references--> `App()`  [INFERRED]
  /home/orel/workspace/blue2th/README.md → src/main.rs
- `test_no_fake_activation_when_bt_returns_error()` --semantically_similar_to--> `ConfirmModal()`  [INFERRED] [semantically similar]
  tests/bluetooth_integration.rs → src/main.rs

## Import Cycles
- None detected.

## Hyperedges (group relationships)
- **Dioxus Reactive State System** — agents_signal, agents_use_signal, agents_use_memo, agents_context_api [INFERRED 0.85]
- **Dioxus Fullstack Rendering Pipeline** — agents_fullstack, agents_server_functions, agents_hydration, agents_use_server_future [EXTRACTED 1.00]
- **Quality gates enforced at commit time and in TDD agents: fmt, clippy, test, conventional commits** — githooks_pre_commit_quality_gate, githooks_commit_msg_conventional_commits, concept_clippy_quality_gate, concept_conventional_commits, claude_md_project_instructions [EXTRACTED 1.00]
- **TDD Cycle for Bluetooth Adapter State Detection** — agents_tdd_implementer, agents_tdd_reviewer, concept_tdd_red_green_refactor, tests_bluetooth_integration_test_enable_bluetooth_returns_bool, src_bluetooth_enable_bluetooth_inner, tdd_review_report [INFERRED 0.85]
- **Android Bluetooth JNI Detection Flow** — src_bluetooth_enable_bluetooth, src_bluetooth_enable_bluetooth_inner, concept_jni_android_bluetooth, concept_platform_conditional_compilation [EXTRACTED 0.95]
- **UI Bluetooth Enable Confirmation Flow** — src_main_home, src_main_confirmmodal, src_bluetooth_enable_bluetooth, src_main_connectionstatus [EXTRACTED 0.95]

## Communities (11 total, 3 thin omitted)

### Community 0 - "Android BT JNI & Errors"
Cohesion: 0.17
Nodes (18): JNI Android Bluetooth Detection, Platform-Conditional Compilation (#[cfg(target_os)]), Display, Error, Formatter, Into, Result, Self (+10 more)

### Community 1 - "Dioxus Framework Concepts"
Cohesion: 0.13
Nodes (16): App Root Component, asset! Macro, Component Model, Context API (use_context_provider / use_context), dioxus::launch Entry Point, Fullstack Mode (Server + Client Split), Hydration (SSR to Client Takeover), Dioxus Routing (Routable Enum) (+8 more)

### Community 2 - "UI App & State"
Cohesion: 0.26
Nodes (13): Element, README Project Overview, Signal, App(), ConnectionStatus, DeviceItem(), DeviceSettings(), Home() (+5 more)

### Community 3 - "TDD Workflow & Dev Standards"
Cohesion: 0.22
Nodes (11): tdd-implementer agent (GREEN phase), tdd-reviewer agent (REFACTOR phase), tdd-test-writer Agent (RED phase), CLAUDE.md Project Instructions, Clippy strict quality gate, Conventional Commits specification, TDD Red-Green-Refactor Cycle, Conventional Commits Validation Hook (+3 more)

### Community 4 - "BT Integration Tests & UI Flows"
Cohesion: 0.25
Nodes (7): EventHandler, ConfirmModal(), Review Report — Real BT Adapter State Detection, test_bt_enabled_stays_false_when_enable_bluetooth_returns_false, test_enable_bluetooth_returns_bool(), test_enable_bluetooth_simulation_fallback(), test_no_fake_activation_when_bt_returns_error()

### Community 5 - "Header SVG Assets"
Cohesion: 0.40
Nodes (6): Blue2th Brand Name (text), Dark Background with Cyan and Red-Orange Accent Colors, Animated Device / Screen UI Element, Lightbulb Icon (Bluetooth symbol motif), Animated SVG Header Logo, Tagline / Subtitle Text (is inter / is intersectable / is interesting)

### Community 6 - "Bluetooth Icon Assets"
Cohesion: 0.50
Nodes (4): 24x24 Icon Format, Blue Color Scheme (#2563eb), Bluetooth Symbol, Bluetooth SVG Icon

## Knowledge Gaps
- **30 isolated node(s):** `Display`, `Formatter`, `Error`, `Into`, `Self` (+25 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **3 thin communities (<3 nodes) omitted from report** — run `graphify query` to explore isolated nodes.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `scan_devices()` connect `Android BT JNI & Errors` to `UI App & State`, `TDD Workflow & Dev Standards`?**
  _High betweenness centrality (0.113) - this node is a cross-community bridge._
- **Why does `enable_bluetooth_inner()` connect `Android BT JNI & Errors` to `BT Integration Tests & UI Flows`?**
  _High betweenness centrality (0.107) - this node is a cross-community bridge._
- **Why does `tdd-test-writer Agent (RED phase)` connect `TDD Workflow & Dev Standards` to `Android BT JNI & Errors`?**
  _High betweenness centrality (0.087) - this node is a cross-community bridge._
- **What connects `Display`, `Formatter`, `Error` to the rest of the system?**
  _32 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `Dioxus Framework Concepts` be split into smaller, more focused modules?**
  _Cohesion score 0.13333333333333333 - nodes in this community are weakly interconnected._