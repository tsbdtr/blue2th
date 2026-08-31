# TDD Feature Specification

> **Instructions**: this file is the template. Describe the feature to Claude in
> conversation; it copies this file to `tdd/feature.md` — a working file, ignored
> by git — fills it, and stops so you can read it before anything runs.
> Then: `/tdd all`.

---

## Feature Name
<!-- Short name, e.g. "Filter devices by name" -->
PENDING

## Change Type
<!-- The Conventional Commits type this change is, one word. It becomes the -->
<!-- branch prefix (`<type>/<slug>`) AND the pull-request title (`<type>: …`), -->
<!-- so a cleanup is not announced as a feature. One of: -->
<!-- feat fix refactor test chore docs style perf build ci revert -->
<!-- `feat` adds behaviour; deleting dead code is `refactor`; tooling is `chore`. -->
PENDING

## Tracking Issue
<!-- `#N` when the work is already filed — /tdd reuses that issue instead of -->
<!-- opening a second one, and the pull request closes it. `none` when there is -->
<!-- no issue yet: /tdd opens one from the Feature Name and Description below. -->
PENDING

## Description
<!-- 2-5 sentences describing the feature in detail -->
PENDING

## Nominal Scenario
<!-- The happy path: step-by-step description of what the user does and what they see -->
PENDING

## Non-nominal Scenarios
<!-- Edge cases and error cases, each with its expected behaviour -->
- PENDING

## Acceptance Criteria
<!-- Each criterion must map to one or more tests -->
- [ ] PENDING

## Layers touched
<!-- Source of truth for which crates the TDD agents build/test. Check all that apply. -->
<!-- mobile = blue2th-frontend (Dioxus/Android) · server = blue2th-server (Axum/PipeWire) · proto = blue2th-proto (shared serde DTOs) -->
- [ ] mobile (`blue2th-frontend`)
- [ ] server (`blue2th-server/`)
- [ ] proto (`blue2th-proto/`)

## Technical Scope

### Files to modify
<!-- Existing files that need changes, grouped by layer -->
- PENDING

### Files to create
<!-- New files if any, grouped by layer -->
- PENDING

### API / functions needed
<!-- Per layer: mobile async fns + UI state (backend.rs for HTTP, jni_util.rs for any JNI), server Axum routes/handlers + audio/transport logic, proto DTOs -->
- PENDING

## Test Strategy

### Unit tests (synchronous, pure logic)
<!-- Tests with no async, no network -->
- PENDING

### Integration tests (async)
<!-- mobile: tests/ + #[tokio::test] · server: blue2th-server/tests/ (e.g. route tests via tower oneshot) · proto: serde round-trip in lib.rs -->
<!-- Hardware (BlueZ/PipeWire/audio device) is NOT test-runnable: cover pure logic, leave the hardware boundary to manual testing. -->
- PENDING

## Manual verification
<!-- What no test here can check, and how to check it by hand. -->
<!-- LEAVE EMPTY if everything is covered by tests. Non-empty means `/tdd all` -->
<!-- stops after GREEN and prints this section, so the behaviour can be tried -->
<!-- before REFACTOR rewrites the code that produces it. -->
<!-- -->
<!-- Each entry MUST say how to *trigger* the case, not only what to look at: -->
<!-- "test the incompatible backend" is not actionable when both binaries -->
<!-- compile the same constant. Give the patch, the fixture or the gesture. -->
<!-- -->
<!-- Anything rendered (Dioxus components) and anything behind hardware -->
<!-- (BlueZ, PipeWire, a real speaker) belongs here by construction. -->
- PENDING

## Constraints & Notes
<!-- Technical constraints, edge cases to handle -->
- Must pass `cargo test --workspace`
- Must pass `cargo clippy --workspace --all-targets -- -D warnings -W clippy::unwrap_used -W clippy::expect_used -W clippy::panic -W clippy::todo -W clippy::unreachable -W clippy::unimplemented`
- If the **mobile** layer is touched: must pass `dx build --platform android --package blue2th-frontend`
- Mobile code follows Dioxus 0.7 patterns (no cx/Scope/use_state); `blue2th-proto` stays target-agnostic
- PENDING
