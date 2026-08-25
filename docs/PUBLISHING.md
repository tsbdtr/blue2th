# Publishing blue2th

Reference for taking the project from a local repository to something that is
continuously integrated and installable. **This document holds findings, not
tasks**: what was verified, what was decided, and what still has to be checked
against the real toolchain. Anything with a closing condition lives in
[the issue tracker](https://github.com/tsbdtr/blue2th/issues), under the
`v0.2.0` and `public` milestones.

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

`graphify-out/20*/` is ignored since `1e42923`. The 15 dated directories were
point-in-time copies of the five root files that `graphify update` rewrites on
every run; nothing read them, and no `graphify` command looks at them —
`query`, `path` and `explain` all default to `graphify-out/graph.json`.
`graph.json`, `graph.html` and `GRAPH_REPORT.md` stay tracked: the project
instructions rely on them.

The hooks in `.githooks/` are opt-in — they only exist once
`git config core.hooksPath` has been set. On a shared repository they guarantee
nothing, which is why CI has to replay the same gates (#4).

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

- **Nothing to arrange for `assets/tailwind.css`.** The file is generated and
  untracked, but `build.rs` creates an empty one when it is missing — precisely so
  a fresh clone can run `cargo test`. CI is already covered.
- **One ignored test** (`blue2th-server/tests/transport.rs:130`, needs a live
  PipeWire daemon). It stays ignored: the hardware boundary is not simulable,
  which is the project's stated philosophy.
- **Optional**: a job validating the Conventional Commits format on the PR title,
  to compensate for the local hook not being guaranteed.

### The Android case

`dx build --platform android` needs the NDK *and* `dx` itself (installable through
`cargo-binstall dioxus-cli`). Budget several minutes of setup per run. Keep it out
of the PR pipeline and reserve it for the release tag, with a `workflow_dispatch`
entry point for the times a PR touches the mobile layer.

The version of `dx` must be pinned, or the frozen Android files drift without
breaking the build — the failure lands at runtime (#2).

---

## 03 — Release management

A `v0.2.0` tag produces a signed APK and a server binary, attached to a GitHub
Release.

Two prerequisites are tracked separately: the version scheme (#1) and the
keystore (#3). The keystore is the only irreversible item in the whole plan.

### The tag workflow

- **Trigger**: `push` on `tags: ['v*']`, two parallel jobs then a publish job.
- **Server binary**: `cargo build --release -p blue2th-server` on `ubuntu-latest`,
  with the same system packages as CI. The direction of glibc compatibility works
  in our favour — built on Ubuntu 24.04 (glibc 2.39), the binary runs on the target
  Fedora 43 (glibc 2.42); the reverse would have broken. Ship a `.tar.gz`.
- **APK**: `dx build --platform android --release`, signed with the decoded
  keystore, then `zipalign`/`apksigner` depending on what `dx` actually emits.
- **Publish**: `gh release create` with both artifacts and release notes.

> **To check before writing the workflow.** The Android `versionCode` — an integer
> that must increase on every publication, or the phone refuses the update. It does
> not appear in the frozen manifest, so `dx` generates it; what remains to establish
> is *where* it takes it from and whether it follows `Cargo.toml`. Verify on a local
> build before building the release on top of it.

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

The licence headers are in an inconsistent state and have to be settled before the
flip (#5).

## Provenance

**Verified in the repository**: `bluer` and `rodio` link D-Bus and ALSA;
`README.md` is the unmodified `dx` template; no secret in any tracked file (the
Spotify client id comes from an environment variable, tokens live in
`$XDG_STATE_HOME`, `.env*` is ignored); a single `#[ignore]` test, on PipeWire; the
three crates sit at `0.1.0` with no `[workspace.package]`; 16 of 37 tracked `.rs`
files carry the Apache header, all of the mobile crate plus `watchdog.rs`;
`.githooks/pre-commit` omits `--workspace`; 652 tests green on `develop`.

**Still to check**: where `dx` takes the Android `versionCode` from; what
`dx build --release` actually emits regarding signing and alignment; the real
duration of an Android CI build, which decides whether it stays out of the PR
pipeline; whether `graphify update` should keep running locally once CI is in
place.
