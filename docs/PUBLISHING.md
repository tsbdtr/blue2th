# Publishing blue2th

Reference for taking the project from a local repository to something that is
continuously integrated and installable. **This document holds findings, not
tasks**: what was verified, what was decided, and what still has to be checked
against the real toolchain. Anything with a closing condition lives in
[the issue tracker](https://github.com/tsbdtr/blue2th/issues), under the
`v0.1.0` and `public` milestones.

Standing decisions:

- The GitHub repository is **private first**, public later.
- A release ships a **signed APK and a server binary**, built from a tag.
- Phase 6.7 (background listening reliability) is deliberately left open — see
  `ROADMAP.md`.

## Sequence and gating

| # | Workstream | Status |
|---|---|---|
| 01 | Going online | **Done** |
| 02 | Continuous integration | Open — a workflow is only validated by running |
| 03 | Release | Needs 02 green, plus a settled version scheme (#1) |
| 04 | User documentation | Open. **This is what gates the flip to public.** |

---

## 01 — Going online — done

`tsbdtr/blue2th`, private, `main` and `develop` pushed, **`develop` as the default
branch**. `main` stays at the init commit until the first delivery fast-forwards
it — it is the stable line, not the working one.

### Branching model

`main` is what has been delivered, one `vX.Y.Z` tag per delivery. `develop` is the
development line. `feature/*` branches off `develop` and merges back into it.

`hotfix/*` branches off `main`, goes through a PR into `main` (plus a tag), and is
then propagated with **`git merge main` on `develop`** — not a second
`hotfix → develop` PR. Absorbing `main`'s merge commit is what keeps `main` a
strict ancestor of `develop`, so every delivery stays a `merge --ff-only` and
cannot conflict. Merging only the fix leaves the two lines diverged.

No `release/*` branches: the tag on `main` already triggers the build.

**Never squash a branch-to-branch merge.** A squash-merge of `develop → main`
creates a commit with no ancestry link, the two lines diverge permanently, and
every subsequent delivery starts in conflict. `gh pr merge --merge` throughout.

### Repository hygiene

`graphify-out/` is ignored in full. It began with the dated directories
(`graphify-out/20*/`, since `1e42923`): 15 point-in-time copies of the files
`graphify update` rewrites on every run, which nothing read and no `graphify`
command looks at. Then the two large blobs, `graph.json` and `graph.html`, at
3.6 MB per regeneration. Then the remainder, once the argument for keeping it
turned out not to survive reading:

- `.graphify_python` stores an absolute path into one machine's uv toolchain, so
  it is wrong on every other clone and rewritten by the next update;
- `cost.json` is a local ledger of token counts per run;
- `GRAPH_REPORT.md` was kept for having a readable diff, but `/tdd cleanup`
  regenerates it *after* the merge, in a commit of its own — that diff is never
  in front of a reviewer.

Nothing in the build, the tests or CI reads any of it, and `graphify update .`
rebuilds the lot locally in seconds with no API call. The project instructions
now say to run it once on a fresh clone. Dropping the last tracked files also
removed the `chore(graph)` commit `/tdd cleanup` made straight on `develop`,
which was the one write bypassing the pull-request rule above.

The hooks in `.githooks/` are opt-in — they only exist once
`git config core.hooksPath` has been set. On a shared repository they guarantee
nothing, which is why CI has to replay the same gates (#4).

`post-merge` needs a second, separate opt-in
(`git config --bool blue2th.tddAutoCleanup true`) because it is the only hook that
*does* something rather than refusing something: it runs `/tdd cleanup` when a
merged `develop` lands, deleting the feature worktree and its branch. It never
fails the merge that called it, and it refuses to remove a worktree holding
untracked files.

---

## 02 — Continuous integration

Replay the project's three quality gates on every PR, without paying for the
Android toolchain each time.

### The verified trap

A bare `ubuntu-latest` runner **does not compile the workspace**. `bluer` (feature
`bluetoothd`) links D-Bus and `rodio` (feature `playback`) links ALSA. Without
those packages the failure lands at link time, with a message that says little:

```yaml
- run: sudo apt-get update && sudo apt-get install -y \
    libdbus-1-dev libasound2-dev pkg-config
```

### The PR workflow

One job, on `pull_request` and on `push` to `develop` and `main`: checkout, stable
toolchain with `rustfmt` and `clippy`, cache (`Swatinem/rust-cache`), then the
project's three commands verbatim — `cargo fmt --check`, the `clippy --workspace`
invocation with its full set of forbidden lints, and `cargo test --workspace`.

The `--workspace` flag is not cosmetic: without it every `blue2th-server` test is
silently skipped. The local pre-commit hook currently gets this wrong (#4).

- **Nothing to arrange for `blue2th-frontend/assets/tailwind.css`.** The file is generated and
  untracked, but `build.rs` creates an empty one when it is missing — precisely so
  a fresh clone can run `cargo test`. CI is already covered.
- **One ignored test** (`blue2th-server/tests/transport.rs:130`, needs a live
  PipeWire daemon). It stays ignored: the hardware boundary is not simulable,
  which is the project's stated philosophy.
- **Optional**: a job validating the Conventional Commits format on the PR title,
  to compensate for the local hook not being guaranteed.

### The Android case

`dx build --platform android --package blue2th-frontend` needs the NDK *and* `dx`
itself (installable through
`cargo-binstall dioxus-cli`). Budget several minutes of setup per run. Keep it out
of the PR pipeline and reserve it for the release tag, with a `workflow_dispatch`
entry point for the times a PR touches the mobile layer.

The version of `dx` must be pinned in that workflow, or the frozen Android files
drift without breaking the build — the failure lands at runtime, on the phone.

Note what the pin does and does not buy. It prevents *accidental* drift; it does not
detect drift. The moment the pin is raised deliberately, the re-diff checklist in
`CLAUDE.md` has to be walked by hand, with nothing verifying that it was. Catching
drift rather than merely postponing it would take a job that regenerates dx's
templates and diffs them against our copies — which needs the Android toolchain in
CI, so it is not free.

---

## 03 — Release management

A `v0.1.0` tag produces a signed APK and a server binary, attached to a GitHub
Release.

Two prerequisites are tracked separately: the version scheme (#1) and the
keystore (#3). The keystore is the only irreversible item in the whole plan.

### The tag workflow

- **Trigger**: `push` on `tags: ['v*']`, two parallel jobs then a publish job.
- **Server binary**: `cargo build --release -p blue2th-server` on `ubuntu-latest`,
  with the same system packages as CI. The direction of glibc compatibility works
  in our favour — built on Ubuntu 24.04 (glibc 2.39), the binary runs on the target
  Fedora 43 (glibc 2.42); the reverse would have broken. Ship a `.tar.gz`.
- **APK**: `dx build --platform android --package blue2th-frontend --release`, signed with the decoded
  keystore, then `zipalign`/`apksigner` depending on what `dx` actually emits.
- **Publish**: `gh release create` with both artifacts and release notes.

### The Android `versionCode` — `dx` does not derive it

`versionCode` is the integer Android compares to decide whether an APK is an
update. It must increase on every publication, or the phone refuses to install
over the app already there.

`dx` does not derive it from anything. In the Handlebars template embedded in the
`dx` binary, the line is a **literal**, sitting between two genuine placeholders:

```kotlin
applicationId = "{{ application_id }}"
minSdk = {{ min_sdk }}
targetSdk = {{ target_sdk }}
versionCode = 1
versionName = "{{ version }}"
```

No configuration key can reach it, because there is no substitution point to
reach. `[bundle] version` feeds `versionName`, `[android] identifier` feeds
`applicationId`; nothing feeds `versionCode`. It is `1` in debug and in release
alike, and stays `1` on every rebuild. Verified against `dx` 0.7.10, in the
generated `target/dx/blue2th-frontend/debug/android/app/app/build.gradle.kts`.

Left alone, the first release ships `versionCode = 1` and the **second one cannot
be installed over it**.

Two routes look like solutions and are not:

- **Declaring `android:versionCode` in the frozen manifest.** The AGP DSL value
  takes precedence over the manifest attribute whenever it is set — and it is set,
  to `1`.
- **Patching the generated `build.gradle.kts`, then re-running `dx`.** `dx`
  rewrites that file from the template on every build, discarding the patch.

What makes it tractable is that the generated Android project is a **standalone
Gradle project**, `gradlew` included, under
`target/dx/<package>/<profile>/android/app/`. The release workflow can let `dx`
build once, rewrite the line, then drive Gradle directly — the Rust `.so` files
are already staged in `jniLibs`, so only the Android packaging runs again:

```bash
dx build --platform android --package blue2th-frontend --release
sed -i "s/versionCode = 1/versionCode = ${VERSION_CODE}/" \
  target/dx/blue2th-frontend/release/android/app/app/build.gradle.kts
(cd target/dx/blue2th-frontend/release/android/app && ./gradlew assembleRelease)
```

`VERSION_CODE` comes from the tag once the version scheme (#1) is settled. Use
**`major * 1000000 + minor * 1000 + patch`** — not a formula of our own. It is the
derivation the upstream fix uses (see below), so the value we compute by hand today
is the value `dx` will compute by itself tomorrow, and the migration changes
nothing. It also stays inside Play's `1..=2100000000` range, and leaves room for
999 minors and 999 patches where a tighter formula would cap them at 99.

> **To confirm on the first real release run**, since neither has been exercised
> yet: that the second Gradle invocation reuses the staged `jniLibs` rather than
> rebuilding, and which of the two APKs ends up where.

### This workaround has an expiry date

[DioxusLabs/dioxus#5735](https://github.com/DioxusLabs/dioxus/pull/5735) — *CLI:
configurable Android versionCode and iOS/macOS CFBundleVersion* — fixes it upstream.
Open since 2026-08-04, not merged as of this writing. It resolves the value in three
steps:

1. `--version-code <u32>`, or the `DX_ANDROID_VERSION_CODE` environment variable;
2. `[android] version_code` in `Dioxus.toml`;
3. failing both, `major * 1000000 + minor * 1000 + patch` from the crate version.

Once it is merged **and released**, drop the `sed` and the second Gradle invocation
and pass the environment variable instead. That is strictly better for us: it
mutates no generated file, so nothing depends on the internal layout of `target/dx`
or on the exact text of a line `dx` owns — a coupling whose failure mode is silent,
since a wrong `versionCode` builds and signs perfectly and only fails on the phone.

Being on a `dx` version that carries the fix is a prerequisite here, so it is tied to
the pin in the release workflow (#23): raising the pin is what enables the
simplification, and the two must move together.

---

## 04 — User documentation

This does not block going online, but it is what gates the flip to public.

> **Replace first.** `README.md` is still the `dx` template — "Your new bare-bones
> project includes minimal organization with a single `main.rs` file". It describes
> a project that no longer exists, and it is the first page anyone will see.

What needs writing:

- **README**: what blue2th does in two sentences, the three-layer architecture, a
  screenshot of the app, the quick start. `ROADMAP.md` remains the development
  document and does not replace this page.
- **Backend installation guide**: PipeWire and BlueZ, registering a Spotify
  application, and the `BLUE2TH_SPOTIFY_CLIENT_ID` variable. The least guessable
  point is already recorded in the roadmap and belongs here too: **in Development
  mode, the Spotify account must appear in the application's allowlist**, without
  which playback fails for no visible reason.
- **Pairing guide**: first launch, the six-character code or the QR, finding the
  backend on the network over mDNS, and what "not paired" means as opposed to
  "unreachable".
- **Known limitations**: phase 6.7 as it stands — Android freezes a backgrounded
  app, which presence reporting and the 30-minute grace period compensate for.
  Saying so plainly beats receiving the bug report.

The licence question is settled: `MIT OR Apache-2.0`, one SPDX line at the top of
every `.rs` file, enforced by the `licence-headers` CI job (#5). What still gates
the flip is the documentation itself — `README.md` is still the `dx` template (#9).

## Provenance

**Verified in the repository**, as of 2026-08-29: `bluer` and `rodio` link D-Bus
and ALSA; `README.md` is still the unmodified `dx` template (#9); no secret in any
tracked file (the Spotify client id comes from an environment variable, tokens live
in `$XDG_STATE_HOME`, `.env*` is ignored); a single `#[ignore]` test, on PipeWire;
the three crates share one `[workspace.package] version`, and none of them is
publishable (#1); `dx` 0.7.10 hardcodes `versionCode = 1` in its Gradle template;
all 35 tracked `.rs` files carry the SPDX header (#5); `.githooks/pre-commit` runs
`cargo test --workspace`; 589 tests green on `develop`, one ignored.

> This list is a snapshot, and it rots quietly. Three of its entries were still
> describing 25 August when they were corrected on the 29th — the headers and the
> hook had been fixed by merged pull requests, the test count had moved with the
> legacy removal. Re-measure before citing it, and carry the date forward when you
> do.

**Still to check**: what `dx build --release` actually emits regarding signing and
alignment; the real duration of an Android CI build, which decides whether it stays
out of the PR pipeline; whether `graphify update` should keep running locally once
CI is in place.
