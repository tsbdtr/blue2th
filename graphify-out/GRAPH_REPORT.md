# Graph Report - .  (2026-06-06)

## Corpus Check
- Corpus is ~4,123 words - fits in a single context window. You may not need a graph.

## Summary
- 38 nodes · 39 edges · 7 communities
- Extraction: 87% EXTRACTED · 13% INFERRED · 0% AMBIGUOUS · INFERRED: 5 edges (avg confidence: 0.85)
- Token cost: 49,788 input · 0 output

## Community Hubs (Navigation)
- [[_COMMUNITY_Reactive State & Component Model|Reactive State & Component Model]]
- [[_COMMUNITY_Project Setup & Configuration|Project Setup & Configuration]]
- [[_COMMUNITY_Brand Header Visual Design|Brand Header Visual Design]]
- [[_COMMUNITY_Core App Entry Point|Core App Entry Point]]
- [[_COMMUNITY_UI Rendering Primitives|UI Rendering Primitives]]
- [[_COMMUNITY_Fullstack & SSR Pipeline|Fullstack & SSR Pipeline]]
- [[_COMMUNITY_Server Types & Error Handling|Server Types & Error Handling]]

## God Nodes (most connected - your core abstractions)
1. `Signal (Reactive State)` - 5 edges
2. `Animated SVG Header Logo` - 5 edges
3. `echo_server()` - 4 edges
4. `RSX Macro (UI DSL)` - 4 edges
5. `Automatic Tailwind Integration (Dioxus 0.7+)` - 4 edges
6. `Element` - 3 edges
7. `Component Model` - 3 edges
8. `use_resource Hook (Async State)` - 3 edges
9. `Hydration (SSR to Client Takeover)` - 3 edges
10. `App()` - 2 edges

## Surprising Connections (you probably didn't know these)
- `dx serve CLI Command` --semantically_similar_to--> `dioxus::launch Entry Point`  [INFERRED] [semantically similar]
  README.md → AGENTS.md
- `Project Structure (main.rs, assets, Cargo.toml)` --references--> `Dioxus 0.7 Framework`  [EXTRACTED]
  README.md → AGENTS.md
- `Automatic Tailwind Integration (Dioxus 0.7+)` --references--> `Dioxus 0.7 Framework`  [EXTRACTED]
  README.md → AGENTS.md

## Import Cycles
- None detected.

## Hyperedges (group relationships)
- **Dioxus Reactive State System** — agents_signal, agents_use_signal, agents_use_memo, agents_context_api [INFERRED 0.85]
- **Dioxus Fullstack Rendering Pipeline** — agents_fullstack, agents_server_functions, agents_hydration, agents_use_server_future [EXTRACTED 1.00]

## Communities (7 total, 0 thin omitted)

### Community 0 - "Reactive State & Component Model"
Cohesion: 0.33
Nodes (6): Component Model, Context API (use_context_provider / use_context), Dioxus Routing (Routable Enum), Signal (Reactive State), use_memo Hook, use_signal Hook

### Community 1 - "Project Setup & Configuration"
Cohesion: 0.33
Nodes (6): Dioxus 0.7 Framework, dioxus.toml Configuration File, dx serve CLI Command, Project Structure (main.rs, assets, Cargo.toml), Automatic Tailwind Integration (Dioxus 0.7+), Tailwind Manual Install via CLI

### Community 2 - "Brand Header Visual Design"
Cohesion: 0.40
Nodes (6): Blue2th Brand Name (text), Dark Background with Cyan and Red-Orange Accent Colors, Animated Device / Screen UI Element, Lightbulb Icon (Bluetooth symbol motif), Animated SVG Header Logo, Tagline / Subtitle Text (is inter / is intersectable / is interesting)

### Community 3 - "Core App Entry Point"
Cohesion: 0.47
Nodes (4): Element, App(), Echo(), Hero()

### Community 4 - "UI Rendering Primitives"
Cohesion: 0.40
Nodes (5): App Root Component, asset! Macro, dioxus::launch Entry Point, RSX Macro (UI DSL), document::Stylesheet Component

### Community 5 - "Fullstack & SSR Pipeline"
Cohesion: 0.50
Nodes (5): Fullstack Mode (Server + Client Split), Hydration (SSR to Client Takeover), Server Functions (#[post] / #[get] macros), use_resource Hook (Async State), use_server_future Hook

### Community 6 - "Server Types & Error Handling"
Cohesion: 0.50
Nodes (4): Result, ServerFnError, echo_server(), String

## Knowledge Gaps
- **15 isolated node(s):** `String`, `Result`, `ServerFnError`, `use_signal Hook`, `use_memo Hook` (+10 more)
  These have ≤1 connection - possible missing edges or undocumented components.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `RSX Macro (UI DSL)` connect `UI Rendering Primitives` to `Reactive State & Component Model`?**
  _High betweenness centrality (0.191) - this node is a cross-community bridge._
- **Why does `Signal (Reactive State)` connect `Reactive State & Component Model` to `Fullstack & SSR Pipeline`?**
  _High betweenness centrality (0.183) - this node is a cross-community bridge._
- **Why does `Component Model` connect `Reactive State & Component Model` to `UI Rendering Primitives`?**
  _High betweenness centrality (0.179) - this node is a cross-community bridge._
- **What connects `String`, `Result`, `ServerFnError` to the rest of the system?**
  _16 weakly-connected nodes found - possible documentation gaps or missing edges._