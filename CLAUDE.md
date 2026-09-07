## Language

All files in this repository must be written in **English**: source code, comments, commit messages, configuration files, agent definitions, skill files, and documentation.

Commit messages must follow the **Conventional Commits** specification:
`<type>(<scope>): <description>` — e.g. `feat(bluetooth): add device signal strength`, `fix(ui): handle connection error state`, `test(scan): add unit tests for deduplication`.
Common types: `feat`, `fix`, `refactor`, `test`, `chore`, `docs`, `style`, `perf`.

## Architecture

A single **cargo workspace** with three layers (full vision in `docs/ROADMAP.md`):

- **mobile** — `blue2th-frontend`: Dioxus 0.7 Android remote. Code in
  `blue2th-frontend/src/`, tests in `blue2th-frontend/tests/`. Talks to the
  backend over HTTP (`reqwest`). It drives no Bluetooth of its own: the on-phone
  JNI/A2DP stack was deleted once the backend took the audio path over. The only
  JNI left is `jni_util.rs` (multicast lock) and `lifecycle.rs` (presence hooks).
  The workspace root holds no package of its own — it is a virtual manifest.
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

`--workspace` is kept for explicitness, not for effect: since the mobile crate moved
out and the root became a virtual manifest, a bare `cargo test` already runs every
member. It used to matter — the root was a package, so `cargo test` tested only it and
silently skipped `blue2th-server`. Keep the flag, and do not restore that reasoning:
the day someone puts a package back at the root, the flag is what keeps the command
meaning the same thing.

When the **mobile** layer changed, also cross-compile for the real target:

```bash
dx build --platform android --package blue2th-frontend
```

Install the hooks once with: `git config core.hooksPath .githooks` — that gets the
pre-commit gate and the commit-message check, both of which only ever refuse a bad
commit.

`.githooks/post-merge` is a third hook, and it stays inert until it is turned on
separately: `git config --bool blue2th.tddAutoCleanup true`. It runs `/tdd cleanup`
when a merged `develop` lands, which deletes a worktree and a branch — nobody
should inherit that by having opted into a pre-commit gate.

`blue2th-frontend/assets/tailwind.css` is **generated, not tracked**: `dx` rebuilds
it from `blue2th-frontend/tailwind.css` on every Android build.
`blue2th-frontend/build.rs` creates an empty one when it is missing, because
`asset!("/assets/tailwind.css")` fails at macro expansion — without it a fresh clone
could not even run `cargo test`. Never commit that file.

### After a Dioxus / `dx` upgrade — re-diff the frozen Android files

`blue2th-frontend/android/AndroidManifest.xml` and
`blue2th-frontend/android/MainActivity.kt` are **copies of dx's own templates**, declared in `Dioxus.toml` (`[application] android_manifest` /
`android_main_activity`). dx **replaces** rather than merges them, so they no longer
follow template changes — and a stale copy fails at runtime, not at build time
(missing permission, `UnsatisfiedLinkError`, dead deep link).

Why each is frozen — keep the delta this small:
- **manifest**: declares `Blue2thPresenceService` (see below) and adds
  `android:launchMode="singleTop"` (no dx config key for it), without
  which the Spotify OAuth redirect stacks a second activity instead of reaching
  `onNewIntent`. It also carries `android:label` as a **literal**: dx regenerates
  `res/values/strings.xml` on every build and derives `app_name` from the cargo
  package name, and `[application] name` in `Dioxus.toml` — despite being
  documented as the display name — does not reach the Android resources. Also carries the permissions and the `blue2th://` intent-filter, since
  `[android.raw]` and `[deep_links]` are inert while a custom manifest is set.
- **MainActivity.kt**: adds `onNewIntent` → `setIntent`, without which the base
  `Activity` leaves `getIntent()` on the launcher intent and `blue2th-frontend/src/deep_link.rs` never
  sees the OAuth redirect; plus `onStart`/`onStop`/`onDestroy` → the
  `nativeOn{Foreground,Background,Gone}` JNI hooks in `blue2th-frontend/src/lifecycle.rs`, which tell
  the backend a frozen app from a dead one (see the watchdog in
  `blue2th-server/src/watchdog.rs`). It also carries `Blue2thPresenceService`, whose
  `onTaskRemoved` is the only reliable signal for a swipe out of recents — dx copies
  this single file, and Kotlin allows several top-level classes per file, so a second
  class has nowhere else to go.

Checklist after bumping `dioxus` or `dx`:

```bash
# 1. See what dx generates now: comment out both keys in [application], then
dx build --platform android --package blue2th-frontend
diff blue2th-frontend/android/AndroidManifest.xml target/dx/blue2th-frontend/debug/android/app/app/src/main/AndroidManifest.xml
diff blue2th-frontend/android/MainActivity.kt target/dx/blue2th-frontend/debug/android/app/app/src/main/kotlin/dev/dioxus/main/MainActivity.kt
# 2. Port any template change into our copies, restore the keys, rebuild.
# 3. The JNI symbols must be exported, or the app crashes on background/redirect:
nm -D --defined-only target/dx/blue2th-frontend/debug/android/app/app/src/main/jniLibs/<abi>/libmain.so \
  | grep Java_dev_dioxus_main_MainActivity
```

### The application identifier is frozen

`Dioxus.toml` pins `[android] identifier = "io.github.tsbdtr.blue2th"`. **Never
change it, and never let a refactor change it.** Android tells applications apart
by this string: a different identifier is a different app, so existing installs
can no longer be updated and their `SharedPreferences` — the paired backends and
their tokens — are lost. It has the same one-way property as the signing key.

It is pinned because it used to be implicit: `dx` derived it from the cargo
package name, which made renaming the crate a silent, irreversible break. The
`typealias BuildConfig = <namespace>.BuildConfig` line in `MainActivity.kt` must
match it.

Watch the `typealias BuildConfig = <namespace>.BuildConfig` line in `MainActivity.kt`:
the namespace comes from the bundle identifier in the generated `build.gradle.kts`, and
the wry Kotlin glue needs it. Check the `<abi>` directory is the one just rebuilt — dx
only rebuilds the ABI it targets, so a stale sibling can look like a missing symbol.

## Code Guidelines

### Licence header

**Every `.rs` file starts with exactly this line, and it must be the first line:**

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
```

No copyright block, no Apache boilerplate — the full texts live in
`LICENSE-APACHE` and `LICENSE-MIT`, and the manifests declare
`license.workspace = true`. A new file without it fails the `licence-headers` job
in CI.

The project is dual-licensed `MIT OR Apache-2.0`, the Rust ecosystem convention:
MIT is short and familiar, Apache-2.0 carries an explicit patent grant, and the
user picks. This is only possible because `librespot` runs as a subprocess rather
than a linked crate — see the architecture section above.

Config files, the frozen `android/` templates and Markdown carry no header: the
templates are dx's code, not ours.

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

### Comments

A comment explains **why**. Those age well, because a reason does not change when
the code does — the GPL boundary around `librespot`, why the Android templates are
frozen, why `--workspace` stays on the test command. Every one of them has held.

What rots is a comment that **asserts something a machine could check**, and it
rots dangerously: a reader believes it and stops checking. `sink_volume` and
`parse_first_percent` both documented `0.0..=1.0`; `pactl` prints `153%` for an
over-amplified sink, and that false claim is exactly what made the missing range
check look unnecessary. The comment caused the bug. **If a comment states a fact
about values, make it a test instead** — a doctest on a public item, or a unit
test whose *name* is the claim.

**No future tense.** "Phase 4 will route to a combined sink instead of hijacking
the system default" outlived phase 4 by two milestones and became issue #66. A
comment describes what *is*; an intention goes to the tracker, which has an owner
and a closing condition. A comment has neither.

**Referencing an issue is fine — claiming its state is not.** `(#33)` says "this
exists because of #33" and stays true forever, including after #33 closes. "The
open question in #54" claims #54 is open, and was already wrong. Prefer the bare
provenance form.

### The empty value is a wildcard, not an edge case

Every predicate built on a prefix or a substring says **yes** to the empty
string: everything starts with `""`, everything contains `""`. So a check that
looks total is not, and it fails open — on the widest possible match rather than
on none.

It has cost this project three defects in one milestone, in all three positions
an empty value can occupy:

- **as an input** — `resolve_target_sink("")` matched every sink through
  `starts_with`, so it resolved to the PC's own speakers, and
  `spotify_target_sink(&[])` returns exactly that empty string;
- **as a parsed field** — `pactl` accepts a `sink=` carrying no value, and the
  unload pattern built from it, `"sink="`, is a substring of *every* loopback
  line: one such module would have unloaded them all;
- **as a result** — two absent values compare equal, so an `assert_eq!` between
  two things the code computed stayed green while the code returned nothing at
  all.

So: **guard the emptiness explicitly** in any prefix or substring predicate, and
**reject an empty field at the parser** rather than letting it travel. A value
read from a subprocess, a config file or the wire is empty far more often than a
test fixture suggests.

## Pull Requests

Every change reaches `develop` through a pull request; `main` only ever receives a
delivery, tagged `vX.Y.Z`. The branching model is in `docs/RELEASING.md`.

**Name the branch after what it delivers**, using the Conventional Commits type
as the prefix: `feat/<slug>`, `fix/<slug>`, `docs/<slug>`, `refactor/<slug>`, and
so on. The authoritative list is the one `.githooks/commit-msg` accepts — `feat`,
`fix`, `refactor`, `test`, `chore`, `docs`, `style`, `perf`, `build`, `ci`,
`revert`. `hotfix/<slug>` is the single exception: it comes from the branching
model, not from the commit spec, and it is the only prefix that branches off
`main`.

The prefix describes the branch, not each of its commits: a `feat/` branch
normally carries a `test:` commit, then `feat:`, then `refactor:`.

**Never merge a pull request.** Open it, report what it contains, and stop — no
`gh pr merge`, no auto-merge, not even on a green CI run. The merge is where a
human takes responsibility for the change. This is a governance rule, not a
statement about capability.

**Keep a pull request reviewable.** Reviewing is the bottleneck here, not writing:
a 2000-line diff produced in ten minutes costs hours to read honestly, and a
reviewer who cannot read it honestly starts rubber-stamping — at which point the
review guarantees nothing. One feature per pull request. If the work grows past
its spec, say so and ask; never widen the branch silently.

**Name what a diff hides.** A new dependency, a change to a public API or to a
shared DTO, a file touched outside the stated scope, a test deleted rather than
fixed — call these out in the pull request body. They are exactly what a reviewer
scanning a green CI run will miss.

**Answering review comments.** Say what changed and why; never just "done" — a
reply that cannot be checked is worse than no reply. Making the symptom disappear
instead of the cause is a regression dressed as a fix. A comment that changes
behaviour goes back through the TDD cycle, with a failing test first, rather than
being patched directly. And disagreement is allowed: when a comment is wrong,
reply with the reason instead of complying. Compliance is not engineering.

## tdd
- **tdd** (`.claude/skills/tdd/SKILL.md`) — orchestrate the whole cycle: tracking issue → red/green/refactor → pull request → cleanup. Trigger: `/tdd`
- When a user describes a new feature, **before** filling `tdd/feature.md` and before running `/tdd`:
  1. Ask the user to describe the **nominal usage scenario** (the happy path: who does what, what happens, what they see).
  2. Identify the **obvious non-nominal cases** (errors, empty states, permission denied, unavailable hardware…) and ask the user how each should be handled.
  3. If the change is visible on screen (the **mobile** layer), ask **how it is represented**: which surface carries it (a status dot, a standing banner, a toast, a disabled control), what colour, and where it sits relative to what is already there. Write the answer into the **nominal scenario** and the **acceptance criteria** — not into a prompt. Dioxus components are not test-runnable here, so for anything rendered the spec is the *only* record; left unasked, the least verifiable half of a feature ends up the least specified. Assume nothing reaches the user through an HTML `title`: a phone has no hover.
  4. Run a **code impact analysis**: query the knowledge graph (`graphify query`) and read the relevant source files to identify which functions/files need to change, determine **which layers are touched** (mobile / server / proto), and flag any risks (breaking changes, Android-only JNI paths, Axum route/handler changes, BlueZ/PipeWire hardware boundaries, shared-DTO contract changes, UI state implications).
  5. Only once the scenarios and impact are clear: fill `tdd/feature.md` — the **Change Type** (the Conventional Commits type the branch and the pull request will carry), the **Tracking Issue** (`#N` when the work is already filed, `none` otherwise), the **Layers touched** checkboxes, and the **Manual verification** section with what no test can check *and how to trigger it*.
  6. Then **stop**. Show what was written and wait for the user's go-ahead before running anything. This is the cheapest checkpoint in the cycle: three agents work from that file, and a wrong spec costs three phases. Do not fill the file and start the phases in one move.
- `/tdd all` ends by opening the pull request, and stops there. **Never merge it** — that is the last human checkpoint. Once a human has, run `/tdd cleanup`, which reads the pull request state first and refuses on one that is open or closed-without-merge rather than deleting the work.
- Agents are defined in `.claude/agents/`: `tdd-test-writer`, `tdd-implementer`, `tdd-reviewer`. Every `gh` call stays in the orchestrating skill: the three agents run headless, where a permission prompt has nobody to answer it.
When the user types `/tdd`, invoke the Skill tool with `skill: "tdd"` before doing anything else.

## graphify

This project has a knowledge graph at graphify-out/ with god nodes, community structure, and cross-file relationships.

Rules:
- **`graphify-out/` is not tracked at all.** On a fresh clone the directory does not
  exist and every command below has nothing to read: run `graphify update .` once
  first, rather than falling back to grep. It is AST-only, costs no API call, and
  takes seconds.
- For codebase questions, first run `graphify query "<question>"`. Use
  `graphify path "<A>" "<B>"` for relationships and `graphify explain "<concept>"`
  for focused concepts. These return a scoped subgraph, usually much smaller than
  GRAPH_REPORT.md or raw grep output.
- If graphify-out/wiki/index.md exists, use it for broad navigation instead of raw
  source browsing.
- Read graphify-out/GRAPH_REPORT.md only for broad architecture review or when
  query/path/explain do not surface enough context. For architecture written by a
  human rather than derived, `docs/ROADMAP.md` and this file come first.
- After modifying code, run `graphify update .` to keep the graph current.
- Nothing under `graphify-out/` is ever committed, and nothing outside a local
  session reads it — not the build, not the tests, not CI. The two large blobs were
  untracked first (3.6 MB per regeneration, a 63k-line diff for a directory rename);
  the rest followed once it was clear that `.graphify_python` carried an absolute
  path to one machine's toolchain, `cost.json` a local token ledger, and
  `GRAPH_REPORT.md` a diff that never reached a reviewer — `/tdd cleanup`
  regenerates it *after* the merge, in a commit of its own.
