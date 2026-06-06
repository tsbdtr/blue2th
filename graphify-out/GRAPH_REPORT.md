# Graph Report - .  (2026-06-07)

## Corpus Check
- Corpus is ~8,111 words - fits in a single context window. You may not need a graph.

## Summary
- 74 nodes · 122 edges · 11 communities (10 shown, 1 thin omitted)
- Extraction: 95% EXTRACTED · 5% INFERRED · 0% AMBIGUOUS · INFERRED: 6 edges (avg confidence: 0.83)
- Token cost: 0 input · 0 output

## Community Hubs (Navigation)
- [[_COMMUNITY_Dioxus App Architecture|Dioxus App Architecture]]
- [[_COMMUNITY_TDD Workflow & Quality Gates|TDD Workflow & Quality Gates]]
- [[_COMMUNITY_Bluetooth Server API|Bluetooth Server API]]
- [[_COMMUNITY_Device UI Components|Device UI Components]]
- [[_COMMUNITY_Branding & Visual Identity|Branding & Visual Identity]]
- [[_COMMUNITY_Dioxus Type System|Dioxus Type System]]
- [[_COMMUNITY_Application Entry Point|Application Entry Point]]
- [[_COMMUNITY_Bluetooth Icon Design|Bluetooth Icon Design]]
- [[_COMMUNITY_Reactive State|Reactive State]]
- [[_COMMUNITY_Internationalization|Internationalization]]
- [[_COMMUNITY_Dioxus Framework|Dioxus Framework]]

## God Nodes (most connected - your core abstractions)
1. `scan_devices()` - 9 edges
2. `use_locale()` - 9 edges
3. `Home()` - 8 edges
4. `DeviceItem()` - 8 edges
5. `TDD Workflow Skill (/tdd)` - 8 edges
6. `DeviceItem` - 7 edges
7. `tdd-implementer Agent (GREEN phase)` - 7 edges
8. `connect_device()` - 6 edges
9. `disconnect_device()` - 6 edges
10. `ConnectionStatus` - 6 edges

## Surprising Connections (you probably didn't know these)
- `README Project Overview` --references--> `App()`  [INFERRED]
  README.md → src/main.rs
- `tdd-implementer Agent (GREEN phase)` --references--> `scan_devices()`  [EXTRACTED]
  .claude/agents/tdd-implementer.md → src/bluetooth.rs
- `tdd-test-writer Agent (RED phase)` --references--> `scan_devices()`  [EXTRACTED]
  .claude/agents/tdd-test-writer.md → src/bluetooth.rs
- `tdd-implementer Agent (GREEN phase)` --references--> `ConnectionStatus`  [EXTRACTED]
  .claude/agents/tdd-implementer.md → src/main.rs
- `tdd-test-writer Agent (RED phase)` --references--> `ConnectionStatus`  [EXTRACTED]
  .claude/agents/tdd-test-writer.md → src/main.rs

## Import Cycles
- None detected.

## Hyperedges (group relationships)
- **Dioxus Reactive State System** — agents_signal, agents_use_signal, agents_use_memo, agents_context_api [INFERRED 0.85]
- **Dioxus Fullstack Rendering Pipeline** — agents_fullstack, agents_server_functions, agents_hydration, agents_use_server_future [EXTRACTED 1.00]
- **TDD Red-Green-Refactor pipeline: skill orchestrates three agents sequentially in isolated worktree** — claude_skills_tdd_skill, claude_agents_tdd_test_writer, claude_agents_tdd_implementer, claude_agents_tdd_reviewer, concept_git_worktree_isolation [EXTRACTED 1.00]
- **Bluetooth API: four server functions expose BT management endpoints called by Home and DeviceItem UI components** — src_bluetooth_scan_devices, src_bluetooth_connect_device, src_bluetooth_disconnect_device, src_bluetooth_enable_bluetooth, src_main_home, src_main_device_item [EXTRACTED 1.00]
- **Quality gates enforced at commit time and in TDD agents: fmt, clippy, test, conventional commits** — githooks_pre_commit_quality_gate, githooks_commit_msg_conventional_commits, concept_clippy_quality_gate, concept_conventional_commits, claude_md_project_instructions [EXTRACTED 1.00]

## Communities (11 total, 1 thin omitted)

### Community 0 - "Dioxus App Architecture"
Cohesion: 0.13
Nodes (16): App Root Component, asset! Macro, Component Model, Context API (use_context_provider / use_context), dioxus::launch Entry Point, Fullstack Mode (Server + Client Split), Hydration (SSR to Client Takeover), Dioxus Routing (Routable Enum) (+8 more)

### Community 1 - "TDD Workflow & Quality Gates"
Cohesion: 0.32
Nodes (13): tdd-implementer Agent (GREEN phase), tdd-reviewer Agent (REFACTOR phase), tdd-test-writer Agent (RED phase), CLAUDE.md Project Instructions, TDD Workflow Skill (/tdd), Clippy strict quality gate, Conventional Commits specification, Git Worktree Feature Isolation (+5 more)

### Community 2 - "Bluetooth Server API"
Cohesion: 0.44
Nodes (9): Dioxus Server Functions (#[post] macros), Result, ServerFnError, connect_device(), disconnect_device(), enable_bluetooth(), String, Vec (+1 more)

### Community 3 - "Device UI Components"
Cohesion: 0.48
Nodes (7): ConfirmModal, ConnectionStatus, DeviceItem, DeviceSettings, Home(), status_icon(), use_locale()

### Community 4 - "Branding & Visual Identity"
Cohesion: 0.40
Nodes (6): Blue2th Brand Name (text), Dark Background with Cyan and Red-Orange Accent Colors, Animated Device / Screen UI Element, Lightbulb Icon (Bluetooth symbol motif), Animated SVG Header Logo, Tagline / Subtitle Text (is inter / is intersectable / is interesting)

### Community 5 - "Dioxus Type System"
Cohesion: 0.40
Nodes (5): Element, EventHandler, ConfirmModal(), DeviceSettings(), String

### Community 6 - "Application Entry Point"
Cohesion: 0.50
Nodes (3): README Project Overview, App(), Route

### Community 7 - "Bluetooth Icon Design"
Cohesion: 0.50
Nodes (4): 24x24 Icon Format, Blue Color Scheme (#2563eb), Bluetooth Symbol, Bluetooth SVG Icon

### Community 8 - "Reactive State"
Cohesion: 0.50
Nodes (4): Signal, ConnectionStatus, DeviceItem(), Vec

### Community 9 - "Internationalization"
Cohesion: 1.00
Nodes (3): English translations (en.yaml), French translations (fr.yaml), rust-i18n integration

## Knowledge Gaps
- **21 isolated node(s):** `Vec`, `EventHandler`, `Signal`, `Vec`, `Dioxus 0.7 Framework` (+16 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **1 thin communities (<3 nodes) omitted from report** — run `graphify query` to explore isolated nodes.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `ConnectionStatus` connect `Device UI Components` to `TDD Workflow & Quality Gates`, `Application Entry Point`?**
  _High betweenness centrality (0.100) - this node is a cross-community bridge._
- **Why does `Home()` connect `Device UI Components` to `Bluetooth Server API`, `Dioxus Type System`, `Application Entry Point`?**
  _High betweenness centrality (0.096) - this node is a cross-community bridge._
- **Why does `scan_devices()` connect `Bluetooth Server API` to `TDD Workflow & Quality Gates`, `Device UI Components`?**
  _High betweenness centrality (0.094) - this node is a cross-community bridge._
- **What connects `Vec`, `EventHandler`, `Signal` to the rest of the system?**
  _22 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `Dioxus App Architecture` be split into smaller, more focused modules?**
  _Cohesion score 0.13333333333333333 - nodes in this community are weakly interconnected._