# blue2th — core

Memories here are pointers. The versioned sources are authoritative; on any conflict, the file wins.

## Authoritative files (read these, not a memory copy)

- `CLAUDE.md` — project rules: layers, quality commands, code guidelines, pull-request rules, `/tdd` and graphify workflows, the frozen Android files and identifier.
- `docs/ARCHITECTURE.md` — design record: control API, auth/pairing, audio path (combined sink, repair, auto-reconnect, Spotify), presence/watchdog, testing philosophy, known limits.
- `docs/RELEASING.md` — branching model (`develop` / `main`, tags), delivery.
- `docs/INSTALL.md`, `docs/PAIRING.md` — operator-facing setup.
- `AGENTS.md` — Dioxus 0.7 API reference for the mobile layer.

## Source map

Virtual cargo workspace, no root package. The files named below are the entry points, not the full list: `git ls-files '<crate>/src'` gives that.

- `blue2th-server/src/` — Axum/Tokio backend. `lib.rs` (routes, handlers, `AppState`, background tasks), `audio.rs` (`AudioRouter`), `router_actor.rs` (the router run on the graph thread), `router_handle.rs`, `graph.rs` (`Graph` trait, `FakeGraph`), `graph_pw.rs` (PipeWire loop thread), `targets.rs`, `spotify*.rs`, `reconnect.rs`, `watchdog.rs`, `auth.rs`, `config.rs`, `tone.rs`. Integration tests in `blue2th-server/tests/`.
- `blue2th-proto/src/lib.rs` — shared serde DTOs, single file.
- `blue2th-frontend/src/` — Dioxus Android remote. `main.rs` (UI), `backend.rs` (HTTP), `settings.rs`, `discovery.rs`, `deep_link.rs`, `jni_util.rs`, `lifecycle.rs`. `android/` holds the frozen dx templates.
- `tdd/` — `/tdd` templates and `cleanup.sh`; `.claude/agents/`, `.claude/skills/tdd/` — the cycle's agents and skill.
- `scripts/` — release and CI helpers.

## Invariants that are easy to break

- `blue2th-proto` stays target-agnostic: no platform or hardware dependency.
- `librespot` is a subprocess, never a crate (GPL boundary). No GPL dependency in any `Cargo.toml`.
- Android application identifier in `Dioxus.toml` is frozen.
- Several source files are very large (`lib.rs`, `graph_pw.rs`, `audio.rs` over 200 KB, `router_actor.rs` and the frontend's `main.rs` over 100 KB): navigate by symbol, never read whole.

## Further memories

- Stack, versions and where the pins live: `mem:tech_stack`.
- Commands to build, test, lint and query the knowledge graph: `mem:suggested_commands`.
- Code, comment, commit and branch rules, as an index into `CLAUDE.md`: `mem:conventions`.
- The gate to pass before calling a coding task done: `mem:task_completion`.
