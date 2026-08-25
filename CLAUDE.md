## Language

All files in this repository must be written in **English**: source code, comments, commit messages, configuration files, agent definitions, skill files, and documentation.

Commit messages must follow the **Conventional Commits** specification:
`<type>(<scope>): <description>` — e.g. `feat(bluetooth): add device signal strength`, `fix(ui): handle connection error state`, `test(scan): add unit tests for deduplication`.
Common types: `feat`, `fix`, `refactor`, `test`, `chore`, `docs`, `style`, `perf`.

## Architecture

A single **cargo workspace** with three layers (full vision in `docs/ROADMAP.md`):

- **mobile** — `blue2th` (root crate): Dioxus 0.7 Android remote. Code in `src/`,
  tests in top-level `tests/`. Talks to the backend over HTTP (`reqwest`). The
  Android Bluetooth JNI stack in `src/bluetooth.rs` is kept as legacy.
- **server** — `blue2th-server`: Axum/Tokio backend on the Linux PC. Drives BlueZ
  (`bluer`) and audio (`rodio`/PipeWire). This is where the audio engine lives.
- **proto** — `blue2th-proto`: serde DTOs shared by mobile and server. **Must stay
  target-agnostic** — no platform or hardware dependencies, ever.

A feature may touch one, two, or all three layers. Quality commands run across the
whole workspace; the Android NDK build runs only when the mobile layer changed.

### librespot stays a subprocess — never a crate

`librespot` is **GPL-3.0**. The server spawns it as a child process (`librespot
--name …` in argv, see `blue2th-server/src/config.rs`), so it sits behind a
process boundary: nothing links it, and its licence does not reach our binary.

That boundary is the only reason blue2th can be distributed under
`MIT OR Apache-2.0`. **Never add `librespot` (or any GPL crate) to a
`Cargo.toml`** — linking it would make the whole server binary GPL-3.0 and
invalidate the project's licence. If a feature seems to need librespot as a
library, say so and stop: it is a licensing decision, not an implementation
detail.

## Quality Commands

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings -W clippy::unwrap_used -W clippy::expect_used -W clippy::panic -W clippy::todo -W clippy::unreachable -W clippy::unimplemented
cargo test --workspace
```

`--workspace` is required: a plain `cargo test` from the root only covers `blue2th`
and `blue2th-proto` — it silently skips every `blue2th-server` test.

When the **mobile** layer changed, also cross-compile for the real target:

```bash
dx build --platform android
```

Install the pre-commit hook once with: `git config core.hooksPath .githooks`

`assets/tailwind.css` is **generated, not tracked**: `dx` rebuilds it from the root
`tailwind.css` on every Android build. `build.rs` creates an empty one when it is
missing, because `asset!("/assets/tailwind.css")` fails at macro expansion — without
it a fresh clone could not even run `cargo test`. Never commit that file.

### After a Dioxus / `dx` upgrade — re-diff the frozen Android files

`android/AndroidManifest.xml` and `android/MainActivity.kt` are **copies of dx's own
templates**, declared in `Dioxus.toml` (`[application] android_manifest` /
`android_main_activity`). dx **replaces** rather than merges them, so they no longer
follow template changes — and a stale copy fails at runtime, not at build time
(missing permission, `UnsatisfiedLinkError`, dead deep link).

Why each is frozen — keep the delta this small:
- **manifest**: declares `Blue2thPresenceService` (see below) and adds
  `android:launchMode="singleTop"` (no dx config key for it), without
  which the Spotify OAuth redirect stacks a second activity instead of reaching
  `onNewIntent`. Also carries the permissions and the `blue2th://` intent-filter, since
  `[android.raw]` and `[deep_links]` are inert while a custom manifest is set.
- **MainActivity.kt**: adds `onNewIntent` → `setIntent`, without which the base
  `Activity` leaves `getIntent()` on the launcher intent and `src/deep_link.rs` never
  sees the OAuth redirect; plus `onStart`/`onStop`/`onDestroy` → the
  `nativeOn{Foreground,Background,Gone}` JNI hooks in `src/lifecycle.rs`, which tell
  the backend a frozen app from a dead one (see the watchdog in
  `blue2th-server/src/watchdog.rs`). It also carries `Blue2thPresenceService`, whose
  `onTaskRemoved` is the only reliable signal for a swipe out of recents — dx copies
  this single file, and Kotlin allows several top-level classes per file, so a second
  class has nowhere else to go.

Checklist after bumping `dioxus` or `dx`:

```bash
# 1. See what dx generates now: comment out both keys in [application], then
dx build --platform android
diff android/AndroidManifest.xml target/dx/blue2th/debug/android/app/app/src/main/AndroidManifest.xml
diff android/MainActivity.kt     target/dx/blue2th/debug/android/app/app/src/main/kotlin/dev/dioxus/main/MainActivity.kt
# 2. Port any template change into our copies, restore the keys, rebuild.
# 3. The JNI symbols must be exported, or the app crashes on background/redirect:
nm -D --defined-only target/dx/blue2th/debug/android/app/app/src/main/jniLibs/<abi>/libmain.so \
  | grep Java_dev_dioxus_main_MainActivity
```

Watch the `typealias BuildConfig = <namespace>.BuildConfig` line in `MainActivity.kt`:
the namespace comes from the bundle identifier in the generated `build.gradle.kts`, and
the wry Kotlin glue needs it. Check the `<abi>` directory is the one just rebuilt — dx
only rebuilds the ABI it targets, so a stale sibling can look like a missing symbol.

## Code Guidelines

### Error Handling
- **No `unwrap()` or `expect()` outside `#[cfg(test)]`** — use `?` to propagate, or handle `None`/`Err` explicitly
- **Server (Axum) handlers** return `Result<Json<T>, AppError>` (`AppError: IntoResponse`); internal logic uses typed errors (`bluer::Result<_>`, `Result<_, AudioError>`). Propagate with `?`, never panic
- **Proto** types carry no error handling — they are plain serde DTOs and must not depend on platform/hardware crates
- **UI event handlers** that call the backend (over `reqwest`) must handle both `Ok` and `Err` — surface errors to the user via state (e.g. `Signal<Option<String>>` for an error message), never silently drop them
- **`panic!`, `todo!`, `unreachable!`** are forbidden outside test code

### Naming
- Functions, variables, modules: `snake_case`
- Types, traits, enums, variants: `PascalCase`
- Constants, statics: `SCREAMING_SNAKE_CASE`
- Test functions: `test_<subject>_<expected_outcome>`

### Rust Style
- Prefer `?` over `match`/`if let` for error propagation
- No `clone()` without a comment explaining why it is necessary
- No `println!` or `dbg!` in production code
- Use owned types (`String`, `Vec<T>`) for Dioxus props; use `&str`, `&[T]` in pure functions

## tdd
- **tdd** (`.claude/skills/tdd/SKILL.md`) — orchestrate the TDD cycle (red/green/refactor). Trigger: `/tdd`
- When a user describes a new feature, **before** filling `tdd/feature.md` and before running `/tdd`:
  1. Ask the user to describe the **nominal usage scenario** (the happy path: who does what, what happens, what they see).
  2. Identify the **obvious non-nominal cases** (errors, empty states, permission denied, unavailable hardware…) and ask the user how each should be handled.
  3. Run a **code impact analysis**: query the knowledge graph (`graphify query`) and read the relevant source files to identify which functions/files need to change, determine **which layers are touched** (mobile / server / proto), and flag any risks (breaking changes, Android-only JNI paths, Axum route/handler changes, BlueZ/PipeWire hardware boundaries, shared-DTO contract changes, UI state implications).
  4. Only once the scenarios and impact are clear: fill `tdd/feature.md` — including the **Layers touched** checkboxes — and tell the user to run `/tdd all`.
- Agents are defined in `.claude/agents/`: `tdd-test-writer`, `tdd-implementer`, `tdd-reviewer`.
When the user types `/tdd`, invoke the Skill tool with `skill: "tdd"` before doing anything else.

## graphify

This project has a knowledge graph at graphify-out/ with god nodes, community structure, and cross-file relationships.

Rules:
- For codebase questions, first run `graphify query "<question>"` when graphify-out/graph.json exists. Use `graphify path "<A>" "<B>"` for relationships and `graphify explain "<concept>"` for focused concepts. These return a scoped subgraph, usually much smaller than GRAPH_REPORT.md or raw grep output.
- If graphify-out/wiki/index.md exists, use it for broad navigation instead of raw source browsing.
- Read graphify-out/GRAPH_REPORT.md only for broad architecture review or when query/path/explain do not surface enough context.
- After modifying code, run `graphify update .` to keep the graph current (AST-only, no API cost).
