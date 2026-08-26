# TDD Feature Specification

> **Instructions**: Describe the feature to Claude (in conversation) and it will fill this file.
> Then run: `/tdd all`

---

## Feature Name
<!-- Short name, e.g. "Filter devices by name" -->
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
<!-- mobile = blue2th (Dioxus/Android, src/) · server = blue2th-server (Axum/PipeWire) · proto = blue2th-proto (shared serde DTOs) -->
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
<!-- Per layer: mobile async fns + dispatcher (bluetooth.rs), server Axum routes/handlers + audio/transport logic, proto DTOs -->
- PENDING

## Test Strategy

### Unit tests (synchronous, pure logic)
<!-- Tests with no async, no network -->
- PENDING

### Integration tests (async)
<!-- mobile: tests/ + #[tokio::test] · server: blue2th-server/tests/ (e.g. route tests via tower oneshot) · proto: serde round-trip in lib.rs -->
<!-- Hardware (BlueZ/PipeWire/audio device) is NOT test-runnable: cover pure logic, leave the hardware boundary to manual testing. -->
- PENDING

## Constraints & Notes
<!-- Technical constraints, edge cases to handle -->
- Must pass `cargo test --workspace`
- Must pass `cargo clippy --workspace --all-targets -- -D warnings -W clippy::unwrap_used -W clippy::expect_used -W clippy::panic -W clippy::todo -W clippy::unreachable -W clippy::unimplemented`
- If the **mobile** layer is touched: must pass `dx build --platform android --package blue2th-frontend`
- Mobile code follows Dioxus 0.7 patterns (no cx/Scope/use_state); `blue2th-proto` stays target-agnostic
- PENDING
