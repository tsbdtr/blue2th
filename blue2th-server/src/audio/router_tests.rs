// SPDX-License-Identifier: MIT OR Apache-2.0

use super::*;
use crate::graph::fake::{FakeGraph, GraphCall, GraphOp};
use std::sync::{Arc, Mutex};

const COMBINED: &str = "blue2th_combined";
const MAC_A: &str = "AA:BB:CC:DD:EE:01";
const MAC_B: &str = "AA:BB:CC:DD:EE:02";
const SINK_A: &str = "bluez_output.AA_BB_CC_DD_EE_01.1";
const SINK_B: &str = "bluez_output.AA_BB_CC_DD_EE_02.1";

fn target(mac: &str, offset_ms: u32) -> SpeakerTarget {
    SpeakerTarget {
        address: mac.to_string(),
        offset_ms,
    }
}

/// A router over `fake`. The fake is cloned because a clone is a handle onto
/// the same state: the router owns one, the test keeps the other to read the
/// recorded calls.
fn router_on(fake: &FakeGraph) -> AudioRouter {
    router_with_clock(fake).0
}

/// A router over `fake` whose clock stands still until the test moves it
/// with [`advance`]: no confirming reload falls due by itself.
fn router_with_clock(fake: &FakeGraph) -> (AudioRouter, Arc<Mutex<Instant>>) {
    let now = Arc::new(Mutex::new(Instant::now()));
    let clock = Arc::clone(&now);
    let router = AudioRouter::with_clock(
        Box::new(fake.clone()),
        Box::new(move || *clock.lock().unwrap()),
    );
    (router, now)
}

/// Move a test router's clock forward by `by`.
fn advance(clock: &Arc<Mutex<Instant>>, by: Duration) {
    let mut now = clock.lock().unwrap();
    *now += by;
}

fn create(sink_name: &str) -> GraphCall {
    GraphCall::CreateCombinedSink {
        sink_name: sink_name.to_string(),
    }
}

fn load(real_sink: &str, latency_ms: u32) -> GraphCall {
    GraphCall::LoadBranch {
        sink_name: COMBINED.to_string(),
        real_sink: real_sink.to_string(),
        latency_ms,
    }
}

fn unload(id: u32) -> GraphCall {
    GraphCall::UnloadBranch { id }
}

fn teardown(sink_name: &str) -> GraphCall {
    GraphCall::Teardown {
        sink_name: sink_name.to_string(),
    }
}

fn clear_stale(sink_name: &str) -> GraphCall {
    GraphCall::ClearStaleDefaultSink {
        sink_name: sink_name.to_string(),
    }
}

fn set_delay(id: u32, delay_ms: u32) -> GraphCall {
    GraphCall::SetBranchDelay { id, delay_ms }
}

/// The branches loaded for the combined sink, as `(id, sink, delay)`.
fn loaded_delays(fake: &FakeGraph) -> Vec<(u32, String, u32)> {
    fake.loaded(COMBINED)
        .into_iter()
        .map(|l| (l.id, l.branch.sink, l.branch.latency_ms))
        .collect()
}

/// The id of the branch loaded into `sink`, asserting there is exactly one.
fn branch_into(fake: &FakeGraph, sink: &str) -> u32 {
    let ids: Vec<u32> = fake
        .loaded(COMBINED)
        .into_iter()
        .filter(|l| l.branch.sink == sink)
        .map(|l| l.id)
        .collect();
    assert_eq!(ids.len(), 1, "one branch into {sink}: {ids:?}");
    ids.first().copied().unwrap_or_default()
}

/// Whether `call` removes, adds or rewrites something.
fn changes_the_graph(call: &GraphCall) -> bool {
    call.is_mutating()
}

/// A graph where both speakers are connected and the combined sink carries a
/// live branch for each, at the delays of offsets 0 and 30. Returns the fake
/// and the two branch ids.
fn steady_graph(live: Option<bool>) -> (FakeGraph, u32, u32) {
    let fake = FakeGraph::with_sinks(&["alsa_output.pci.analog-stereo", SINK_A, SINK_B]);
    fake.add_sink(COMBINED);
    let a = fake.seed_branch(COMBINED, SINK_A, 0, live);
    let b = fake.seed_branch(COMBINED, SINK_B, 30, live);
    (fake, a, b)
}

fn steady_selection() -> Vec<SpeakerTarget> {
    vec![target(MAC_A, 0), target(MAC_B, 30)]
}

// Criterion: on an empty graph, `route_for_targets` first clears a stale
// configured default (#66), then creates the sink and loads one branch per
// reachable speaker at a delay of its offset — and writes no default sink.
// The teardown is today's guard against stacking modules on a leftover.
#[test]
fn test_route_on_an_empty_graph_creates_the_sink_then_loads_each_branch() {
    let fake = FakeGraph::with_sinks(&["alsa_output.pci.analog-stereo", SINK_A, SINK_B]);
    let mut router = router_on(&fake);

    let result = router.route_for_targets(&[target(MAC_A, 0), target(MAC_B, 250)]);

    assert!(result.is_ok(), "route failed: {result:?}");
    assert_eq!(
        fake.routing_calls(),
        vec![
            clear_stale(COMBINED),
            teardown(COMBINED),
            create(COMBINED),
            load(SINK_A, 0),
            load(SINK_B, 250),
        ]
    );
    assert_eq!(fake.configured_default(), None, "no default was written");
}

/// `default.configured.audio.sink` as earlier versions of blue2th left it,
/// captured with `pw-metadata -n default 0` on the dev PC on 2026-09-24 and
/// 2026-09-26.
const STALE_DEFAULT: &str = r#"{"name":"blue2th_combined"}"#;

/// A configured default the user chose: the PC's own speakers.
const PC_DEFAULT: &str = r#"{"name":"alsa_output.pci-0000_00_1f.3.analog-stereo"}"#;

// Criterion (guard, the router never writes the default): across a build,
// a reconcile, a retune and a confirming reload, the configured default
// the user chose is never rewritten, and no call but the build's one
// clear concerns the default at all.
#[test]
fn test_route_never_writes_the_default_sink() {
    let fake = FakeGraph::with_sinks(&["alsa_output.pci.analog-stereo", SINK_A, SINK_B]);
    fake.set_configured_default(Some(PC_DEFAULT));
    let (mut router, clock) = router_with_clock(&fake);

    // A build.
    let result = router.route_for_targets(&steady_selection());
    assert!(result.is_ok(), "build failed: {result:?}");
    assert_eq!(
        fake.routing_calls(),
        vec![
            clear_stale(COMBINED),
            teardown(COMBINED),
            create(COMBINED),
            load(SINK_A, 0),
            load(SINK_B, 30),
        ]
    );
    let a = branch_into(&fake, SINK_A);

    // A reconcile, within the gap: nothing at all.
    advance(&clock, Duration::from_secs(1));
    fake.clear_calls();
    let result = router.route_for_targets(&steady_selection());
    assert!(result.is_ok(), "reconcile failed: {result:?}");
    assert_eq!(fake.calls(), Vec::<GraphCall>::new());

    // A retune: the delay, and nothing else.
    fake.clear_calls();
    let result = router.retune_branch(
        COMBINED,
        &CombineBranch {
            sink: bluez_sink_prefix(MAC_A),
            latency_ms: 120,
        },
    );
    assert!(result.is_ok(), "retune failed: {result:?}");
    assert_eq!(fake.calls(), vec![set_delay(a, 120)]);

    // The confirming reload, a gap after the build: branches only.
    advance(&clock, CONFIRM_GAP);
    fake.clear_calls();
    let result = router.route_for_targets(&[target(MAC_A, 120), target(MAC_B, 30)]);
    assert!(result.is_ok(), "confirming pass failed: {result:?}");
    let calls = fake.calls();
    assert_eq!(calls.len(), 4, "both branches reloaded: {calls:?}");
    assert!(
        calls.iter().all(|c| matches!(
            c,
            GraphCall::UnloadBranch { .. } | GraphCall::LoadBranch { .. }
        )),
        "a confirmation touches branches only: {calls:?}"
    );

    assert_eq!(
        fake.configured_default().as_deref(),
        Some(PC_DEFAULT),
        "the user's default was rewritten"
    );
}

// Criterion: `build_combined` clears a configured default naming the
// combined sink exactly, and does so before it creates the sink.
#[test]
fn test_build_clears_a_stale_default_naming_the_combined_sink_before_creating_it() {
    let fake = FakeGraph::with_sinks(&[SINK_A, SINK_B]);
    fake.set_configured_default(Some(STALE_DEFAULT));
    let mut router = router_on(&fake);

    let result = router.route_for_targets(&steady_selection());

    assert!(result.is_ok(), "build failed: {result:?}");
    let calls = fake.calls();
    let cleared_at = calls.iter().position(|c| *c == clear_stale(COMBINED));
    let created_at = calls.iter().position(|c| *c == create(COMBINED));
    assert!(
        matches!((cleared_at, created_at), (Some(cleared), Some(created)) if cleared < created),
        "the stale default is cleared before the sink is created: {calls:?}"
    );
    assert_eq!(
        calls
            .iter()
            .filter(|c| **c == clear_stale(COMBINED))
            .count(),
        1,
        "cleared once: {calls:?}"
    );
    assert_eq!(fake.configured_default(), None, "the stale key is gone");
}

// Criterion (non-nominal): a configured default naming another sink —
// here the near-miss sharing the combined sink's opening characters, and
// the PC's speakers — is left exactly as it is, although the build asked.
#[test]
fn test_build_leaves_a_default_naming_another_sink() {
    for other in [r#"{"name":"blue2th_combined_old"}"#, PC_DEFAULT] {
        let fake = FakeGraph::with_sinks(&[SINK_A, SINK_B]);
        fake.set_configured_default(Some(other));
        let mut router = router_on(&fake);

        let result = router.route_for_targets(&steady_selection());

        assert!(result.is_ok(), "build failed: {result:?}");
        assert!(
            fake.calls().contains(&clear_stale(COMBINED)),
            "the build asked: {:?}",
            fake.calls()
        );
        assert_eq!(fake.configured_default().as_deref(), Some(other));
    }
}

// Criterion (non-nominal): reading or clearing the metadata fails — the
// build goes on, creates the sink and loads every branch, and the route
// answers `Ok`: losing the cleanup never costs the speakers their sound.
#[test]
fn test_build_goes_on_when_clearing_the_default_fails() {
    let fake = FakeGraph::with_sinks(&[SINK_A, SINK_B]);
    fake.set_configured_default(Some(STALE_DEFAULT));
    fake.fail(GraphOp::ClearStaleDefaultSink);
    let mut router = router_on(&fake);

    let result = router.route_for_targets(&steady_selection());

    assert!(
        result.is_ok(),
        "a failed clear failed the build: {result:?}"
    );
    assert_eq!(
        fake.routing_calls(),
        vec![
            clear_stale(COMBINED),
            teardown(COMBINED),
            create(COMBINED),
            load(SINK_A, 0),
            load(SINK_B, 30),
        ]
    );
    let sinks: Vec<String> = fake
        .loaded(COMBINED)
        .into_iter()
        .map(|l| l.branch.sink)
        .collect();
    assert_eq!(sinks, vec![SINK_A, SINK_B]);
}

// Criterion (guard, only on a build, never on reconcile): a reconcile pass
// does not even ask — the stale value is seeded, so a router clearing on
// every pass would remove it here.
#[test]
fn test_reconcile_never_touches_the_default() {
    let (fake, _, _) = steady_graph(Some(true));
    fake.set_configured_default(Some(STALE_DEFAULT));
    let mut router = router_on(&fake);

    let result = router.route_for_targets(&[target(MAC_A, 0), target(MAC_B, 120)]);

    assert!(result.is_ok(), "reconcile failed: {result:?}");
    assert!(
        !fake
            .all_calls()
            .iter()
            .any(|c| matches!(c, GraphCall::ClearStaleDefaultSink { .. })),
        "a reconcile asked to clear the default: {:?}",
        fake.all_calls()
    );
    let b = branch_into(&fake, SINK_B);
    assert_eq!(fake.calls(), vec![set_delay(b, 120)]);
    assert_eq!(fake.configured_default().as_deref(), Some(STALE_DEFAULT));
}

fn retarget(sink_name: &str) -> GraphCall {
    GraphCall::RetargetStreams {
        sink_name: sink_name.to_string(),
    }
}

/// How many times the router asked the graph to re-target the streams.
fn retargets(fake: &FakeGraph) -> usize {
    fake.all_calls()
        .iter()
        .filter(|c| matches!(c, GraphCall::RetargetStreams { .. }))
        .count()
}

// Criterion (#139): a build asks the graph to re-target the streams to the
// combined sink, once, after it created the sink — where it sits among the
// branch loads is the router's choice. The rest of the build is as before.
#[test]
fn test_build_retargets_the_streams_to_the_combined_sink_after_creating_it() {
    let fake = FakeGraph::with_sinks(&[SINK_A, SINK_B]);
    let mut router = router_on(&fake);

    let result = router.route_for_targets(&steady_selection());

    assert!(result.is_ok(), "build failed: {result:?}");
    let calls = fake.calls();
    let created_at = calls.iter().position(|c| *c == create(COMBINED));
    let retargeted_at = calls.iter().position(|c| *c == retarget(COMBINED));
    assert!(
        matches!((created_at, retargeted_at), (Some(created), Some(retargeted)) if created < retargeted),
        "the streams are re-targeted after the sink is created: {calls:?}"
    );
    assert_eq!(retargets(&fake), 1, "once: {calls:?}");
    assert_eq!(
        fake.routing_calls(),
        vec![
            clear_stale(COMBINED),
            teardown(COMBINED),
            create(COMBINED),
            load(SINK_A, 0),
            load(SINK_B, 30),
        ]
    );
}

// Criterion (#139, guard, re-target on creation only): a reconcile never
// re-targets — neither one that loads a missing branch and retunes
// another, which changes the graph as a build does, nor the confirming
// reload a gap after the build. A router re-targeting on every pass, or on
// every load, would do it here. The control: the build that started it
// did re-target.
#[test]
fn test_reconcile_never_retargets() {
    let fake = FakeGraph::with_sinks(&[SINK_A, SINK_B]);
    let (mut router, clock) = router_with_clock(&fake);
    let result = router.route_for_targets(&[target(MAC_A, 0)]);
    assert!(result.is_ok(), "build failed: {result:?}");
    assert_eq!(retargets(&fake), 1, "control: the build re-targeted");

    // A reconcile that loads B alone and retunes A in place.
    fake.clear_calls();
    let result = router.route_for_targets(&[target(MAC_A, 40), target(MAC_B, 30)]);
    assert!(result.is_ok(), "reconcile failed: {result:?}");
    assert!(
        fake.calls().contains(&load(SINK_B, 30)),
        "the reconcile loaded B: {:?}",
        fake.calls()
    );
    assert_eq!(retargets(&fake), 0, "calls: {:?}", fake.all_calls());

    // The confirming reloads, a gap after the loads.
    advance(&clock, CONFIRM_GAP);
    fake.clear_calls();
    let result = router.route_for_targets(&[target(MAC_A, 40), target(MAC_B, 30)]);
    assert!(result.is_ok(), "confirming pass failed: {result:?}");
    assert!(
        fake.calls()
            .iter()
            .any(|c| matches!(c, GraphCall::LoadBranch { .. })),
        "a confirming reload ran: {:?}",
        fake.calls()
    );
    assert_eq!(retargets(&fake), 0, "calls: {:?}", fake.all_calls());
}

// Criterion (#139): a build whose sink cannot be created re-targets
// nothing — there is no sink to move a stream onto — and reports no
// re-targeting failure: the route's own error says what went wrong.
#[test]
fn test_build_whose_sink_cannot_be_created_retargets_nothing() {
    let fake = FakeGraph::with_sinks(&[SINK_A, SINK_B]);
    fake.fail(GraphOp::CreateCombinedSink);
    let mut router = router_on(&fake);

    let result = router.route_for_targets(&steady_selection());

    assert!(result.is_err(), "the sink was not created: {result:?}");
    assert_eq!(retargets(&fake), 0, "calls: {:?}", fake.all_calls());
    assert!(!router.last_retarget_failed());
}

// Criterion (#139): re-targeting zero streams — a paused `librespot` holds
// none — is a success: the route is `Ok` and no failure is reported, so
// the pass has nothing to fall back for.
#[test]
fn test_build_retargeting_zero_streams_is_success() {
    let fake = FakeGraph::with_sinks(&[SINK_A, SINK_B]);
    let mut router = router_on(&fake);

    let result = router.route_for_targets(&steady_selection());

    assert!(result.is_ok(), "build failed: {result:?}");
    assert_eq!(retargets(&fake), 1, "the build did re-target");
    assert!(!router.last_retarget_failed());
}

// Criterion (#139): a re-targeting error does not fail the route — every
// branch is still loaded and the route answers `Ok` — but the router
// reports it, so the pass can fall back.
#[test]
fn test_build_with_a_failing_retarget_still_loads_every_branch_and_reports_it() {
    let fake = FakeGraph::with_sinks(&[SINK_A, SINK_B]);
    fake.fail(GraphOp::RetargetStreams);
    let mut router = router_on(&fake);

    let result = router.route_for_targets(&steady_selection());

    assert!(
        result.is_ok(),
        "a failed re-target failed the route: {result:?}"
    );
    assert_eq!(retargets(&fake), 1, "it was attempted");
    let sinks: Vec<String> = fake
        .loaded(COMBINED)
        .into_iter()
        .map(|l| l.branch.sink)
        .collect();
    assert_eq!(sinks, vec![SINK_A, SINK_B]);
    assert!(router.last_retarget_failed(), "the failure is reported");
}

// Criterion (#139): the failure reported is the last route's. The next
// route — here a reconcile, which re-targets nothing — reports none, so a
// pass never falls back on a failure an earlier build left behind.
#[test]
fn test_last_retarget_failed_is_cleared_by_the_next_route() {
    let fake = FakeGraph::with_sinks(&[SINK_A, SINK_B]);
    fake.fail(GraphOp::RetargetStreams);
    let mut router = router_on(&fake);
    let result = router.route_for_targets(&steady_selection());
    assert!(result.is_ok(), "build failed: {result:?}");
    assert!(
        router.last_retarget_failed(),
        "the build's failure is reported"
    );

    let result = router.route_for_targets(&steady_selection());

    assert!(result.is_ok(), "reconcile failed: {result:?}");
    assert!(!router.last_retarget_failed());
}

// Criterion: a build tears a leftover down first, so branches that outlived
// their null sink are not stacked under the new ones.
#[test]
fn test_route_without_a_combined_sink_clears_leftover_branches_before_building() {
    let fake = FakeGraph::with_sinks(&[SINK_A]);
    let leftover = fake.seed_branch(COMBINED, SINK_A, 50, Some(true));
    let mut router = router_on(&fake);

    let result = router.route_for_targets(&[target(MAC_A, 0)]);

    assert!(result.is_ok(), "route failed: {result:?}");
    let loaded = fake.loaded(COMBINED);
    assert_eq!(loaded.len(), 1, "one branch for one speaker: {loaded:?}");
    assert_ne!(loaded[0].id, leftover);
    assert_eq!(loaded[0].branch.sink, SINK_A);
}

// Criterion: on a graph that already matches the plan, a pass performs no
// mutating call at all — not even a write of the default sink (#66).
#[test]
fn test_route_on_a_steady_graph_makes_no_mutating_call() {
    let (fake, _, _) = steady_graph(Some(true));
    let mut router = router_on(&fake);

    let result = router.route_for_targets(&steady_selection());

    assert!(result.is_ok(), "route failed: {result:?}");
    assert_eq!(fake.calls(), Vec::<GraphCall>::new());
}

// Criterion: a steady graph stays steady pass after pass — the repair tick
// runs this every five seconds.
#[test]
fn test_route_on_a_steady_graph_stays_quiet_over_several_passes() {
    let (fake, a, b) = steady_graph(Some(true));
    let mut router = router_on(&fake);

    for pass in 0..3 {
        let result = router.route_for_targets(&steady_selection());
        assert!(result.is_ok(), "pass {pass} failed: {result:?}");
    }

    assert_eq!(fake.calls(), Vec::<GraphCall>::new());
    let ids: Vec<u32> = fake.loaded(COMBINED).iter().map(|l| l.id).collect();
    assert_eq!(ids, vec![a, b]);
}

// Criterion: a dead branch (`live == Some(false)`) — a link that could not
// be created counts as one — is unloaded by id before its replacement is
// loaded, and it is reloaded alone: the live branch keeps its id.
#[test]
fn test_route_unloads_a_dead_branch_before_loading_its_replacement() {
    let fake = FakeGraph::with_sinks(&[SINK_A, SINK_B, COMBINED]);
    let a = fake.seed_branch(COMBINED, SINK_A, 0, Some(true));
    let dead = fake.seed_branch(COMBINED, SINK_B, 30, Some(false));
    let mut router = router_on(&fake);

    let result = router.route_for_targets(&steady_selection());

    assert!(result.is_ok(), "route failed: {result:?}");
    assert_eq!(fake.calls(), vec![unload(dead), load(SINK_B, 30)]);
    // Exactly one branch per speaker is left: never two onto one.
    let sinks: Vec<String> = fake
        .loaded(COMBINED)
        .into_iter()
        .map(|l| l.branch.sink)
        .collect();
    assert_eq!(sinks, vec![SINK_A, SINK_B]);
    assert_eq!(branch_into(&fake, SINK_A), a);
}

// Non-nominal: the graph cannot be read (`Graph::sinks` errs on every read).
// "Cannot tell" is never "no sink exists": nothing is unloaded, torn down,
// created or loaded — even though a stale branch is there for the taking.
#[test]
fn test_route_with_an_unreadable_sink_list_unloads_nothing() {
    let (fake, a, b) = steady_graph(Some(true));
    let stale = fake.seed_branch(COMBINED, "bluez_output.AA_BB_CC_DD_EE_03.1", 50, Some(true));
    fake.fail(GraphOp::Sinks);
    let mut router = router_on(&fake);

    let _ = router.route_for_targets(&steady_selection());

    let calls = fake.calls();
    assert!(
        !calls.iter().any(changes_the_graph),
        "acted on an unreadable graph: {calls:?}"
    );
    assert!(
        fake.all_calls().contains(&GraphCall::Sinks),
        "the sink list was never asked for"
    );
    let ids: Vec<u32> = fake.loaded(COMBINED).iter().map(|l| l.id).collect();
    assert_eq!(ids, vec![a, b, stale]);
}

// Non-nominal (#152, changed): the sink list stops being readable in the
// middle of a pass — the reconciliation answers the read's own error
// (no longer `Ok(())`) without unloading anything. The route entry
// point's read of the combined sink went through; the reconciliation's
// own read is the one refused.
#[test]
fn test_route_with_a_sink_list_lost_mid_pass_answers_the_read_error_and_unloads_nothing() {
    let (fake, a, b) = steady_graph(Some(true));
    fake.fail_after(GraphOp::Sinks, 1);
    let mut router = router_on(&fake);

    let result = router.route_for_targets(&steady_selection());

    assert!(
        matches!(&result, Err(AudioError::PipeWire(m)) if m.contains("Sinks told to fail")),
        "got {result:?}"
    );
    let calls = fake.calls();
    assert!(
        !calls.iter().any(changes_the_graph),
        "acted on an unreadable graph: {calls:?}"
    );
    let ids: Vec<u32> = fake.loaded(COMBINED).iter().map(|l| l.id).collect();
    assert_eq!(ids, vec![a, b]);
}

// Non-nominal: the sink list reads back naming nothing — not even the
// combined sink the pass was entered for. A graph that names nothing
// describes nothing, so it is treated exactly like an unreadable list: the
// pass ends without unloading anything. Driven through
// `reconcile_combined` directly, since the route entry point would already
// have turned an empty list into a build.
#[test]
fn test_reconcile_with_a_sink_list_naming_nothing_unloads_nothing() {
    let (fake, a, b) = steady_graph(Some(true));
    for sink in fake.sink_names() {
        fake.remove_sink(&sink);
    }
    let mut router = router_on(&fake);

    let result = router.reconcile_combined(&combine_sink_plan(&steady_selection()));

    assert!(result.is_ok(), "reconcile failed: {result:?}");
    assert_eq!(fake.calls(), Vec::<GraphCall>::new());
    let ids: Vec<u32> = fake.loaded(COMBINED).iter().map(|l| l.id).collect();
    assert_eq!(ids, vec![a, b]);
}

// ─── #152: a stall is reported as one ───────────────────────────────────

// Criterion (#152): `reconcile_combined` answers the error of an
// unreadable first sink-list read — `Unanswered` when the read is
// unanswered — and makes no mutating graph call in that pass. The plan
// drops the B speaker, so the near miss is the unload of B's branch a
// pass that read the list would make: the early exit stays, only its
// answer changes.
#[test]
fn test_reconcile_over_an_unanswered_sink_list_answers_unanswered_and_changes_nothing() {
    let (fake, a, b) = steady_graph(Some(true));
    fake.fail_unanswered(GraphOp::Sinks);
    let mut router = router_on(&fake);

    let result = router.reconcile_combined(&combine_sink_plan(&[target(MAC_A, 0)]));

    assert_eq!(result, Err(AudioError::Unanswered));
    assert_eq!(fake.calls(), Vec::<GraphCall>::new());
    let ids: Vec<u32> = fake.loaded(COMBINED).iter().map(|l| l.id).collect();
    assert_eq!(ids, vec![a, b]);
}

// Criterion (#152): the same stall reached through `route_for_targets`,
// the way every routing message enters: the route's own lookup of the
// combined sink is answered, the reconciliation's first read is not. The
// route answers `Unanswered` (it answered `Ok(())`), and B's branch, no
// longer planned, is not unloaded.
#[test]
fn test_route_whose_reconcile_read_went_unanswered_answers_unanswered_and_changes_nothing() {
    let (fake, a, b) = steady_graph(Some(true));
    fake.fail_unanswered_after(GraphOp::Sinks, 1);
    let mut router = router_on(&fake);

    let result = router.route_for_targets(&[target(MAC_A, 0)]);

    assert_eq!(result, Err(AudioError::Unanswered));
    assert_eq!(fake.calls(), Vec::<GraphCall>::new());
    let ids: Vec<u32> = fake.loaded(COMBINED).iter().map(|l| l.id).collect();
    assert_eq!(ids, vec![a, b]);
}

// Criterion (#152, guard, an empty list is not an unreadable list): a
// sink list that reads fine and is empty still answers `Ok(())` with no
// mutating call, unchanged. The near miss sits in the same test: the same
// empty graph whose read is unanswered must answer `Err(Unanswered)` —
// an implementation that errs on both, or answers `Ok` on both, fails.
#[test]
fn test_reconcile_over_a_readable_empty_sink_list_answers_ok_and_an_unanswered_one_does_not() {
    let empty = FakeGraph::new();
    let answer = router_on(&empty).reconcile_combined(&combine_sink_plan(&steady_selection()));

    assert_eq!(answer, Ok(()));
    assert_eq!(empty.calls(), Vec::<GraphCall>::new());

    let stalled = FakeGraph::new();
    stalled.fail_unanswered(GraphOp::Sinks);
    let answer = router_on(&stalled).reconcile_combined(&combine_sink_plan(&steady_selection()));

    assert_eq!(answer, Err(AudioError::Unanswered));
    assert_eq!(stalled.calls(), Vec::<GraphCall>::new());
}

/// Speaker A's branch, as a plan carries it: its prefix, no card suffix.
fn branch_of_a() -> CombineBranch {
    CombineBranch {
        sink: bluez_sink_prefix(MAC_A),
        latency_ms: 0,
    }
}

// Criterion (#152): `resolve_branch_sink` answers the read error when the
// sink list cannot be read — `Unanswered` stays `Unanswered` — and never
// the "no PipeWire sink for prefix" of a speaker that is absent. A's sink
// is in the list, so a resolver that ignores the error and reads on
// would even find it.
#[test]
fn test_resolve_branch_sink_over_an_unanswered_sink_list_answers_unanswered() {
    let mut fake = FakeGraph::with_sinks(&[SINK_A]);
    fake.fail_unanswered(GraphOp::Sinks);

    assert_eq!(
        resolve_branch_sink(&mut fake, &branch_of_a()),
        Err(AudioError::Unanswered)
    );
}

// Criterion (#152): an answered read failure is propagated as it is too:
// the fake's `PipeWire("… told to fail")`, not rewritten into the
// message of an absent speaker.
#[test]
fn test_resolve_branch_sink_over_a_refused_sink_list_answers_the_read_s_own_error() {
    let mut fake = FakeGraph::with_sinks(&[SINK_A]);
    fake.fail(GraphOp::Sinks);

    let answer = resolve_branch_sink(&mut fake, &branch_of_a());

    assert!(
        matches!(&answer, Err(AudioError::PipeWire(m)) if m.contains("Sinks told to fail")),
        "got {answer:?}"
    );
}

// Criterion (#152, guard, a readable list without the prefix is not an
// unreadable list): a list that reads fine and holds no sink for A —
// only B's, whose name shares all of A's prefix but its last digit, and
// the PC's own — still fails with `no PipeWire sink for prefix …`. The
// control: with A's sink in the list, A resolves to its node.
#[test]
fn test_resolve_branch_sink_over_a_readable_list_without_the_prefix_names_the_prefix() {
    let mut fake = FakeGraph::with_sinks(&["alsa_output.pci.analog-stereo", SINK_B]);

    assert_eq!(
        resolve_branch_sink(&mut fake, &branch_of_a()),
        Err(AudioError::PipeWire(format!(
            "no PipeWire sink for prefix {}",
            bluez_sink_prefix(MAC_A)
        )))
    );

    let mut fake = FakeGraph::with_sinks(&[SINK_B, SINK_A]);
    assert_eq!(
        resolve_branch_sink(&mut fake, &branch_of_a()),
        Ok(SINK_A.to_string())
    );
}

// Criterion (#152): a routing pass whose branch resolution hit an
// unanswered read answers `Err(Unanswered)` through
// `BranchLoadReport::into_result`. A build from nothing reads the sink
// list once to look for the combined sink, then once per branch to
// resolve it: A's resolution is answered and A's branch loads, B's
// resolution stalls. Today that stall reads as "no sink for B" and the
// pass answers a `PipeWire` error — a 500 rather than a 503.
#[test]
fn test_route_whose_branch_resolution_went_unanswered_answers_unanswered() {
    let fake = FakeGraph::with_sinks(&[SINK_A, SINK_B]);
    fake.fail_unanswered_after(GraphOp::Sinks, 2);
    let mut router = router_on(&fake);

    let result = router.route_for_targets(&[target(MAC_A, 0), target(MAC_B, 30)]);

    assert_eq!(result, Err(AudioError::Unanswered));
    assert_eq!(
        fake.routing_calls(),
        vec![
            clear_stale(COMBINED),
            teardown(COMBINED),
            create(COMBINED),
            load(SINK_A, 0),
        ],
        "A loads, B is never loaded"
    );
}

// Non-nominal: liveness cannot be read (`live == None`) — every branch is
// kept, none is ruled dead.
#[test]
fn test_route_with_unknown_liveness_keeps_every_branch() {
    let (fake, a, b) = steady_graph(None);
    let mut router = router_on(&fake);

    let result = router.route_for_targets(&steady_selection());

    assert!(result.is_ok(), "route failed: {result:?}");
    assert_eq!(fake.calls(), Vec::<GraphCall>::new());
    let ids: Vec<u32> = fake.loaded(COMBINED).iter().map(|l| l.id).collect();
    assert_eq!(ids, vec![a, b]);
}

// Non-nominal: a planned speaker's sink is absent — it is dropped from the
// reachable plan, no rebuild is requested for it, the other branch is
// untouched.
#[test]
fn test_route_with_an_absent_speaker_leaves_the_other_branch_alone() {
    let fake = FakeGraph::with_sinks(&[SINK_A, COMBINED]);
    let a = fake.seed_branch(COMBINED, SINK_A, 0, Some(true));
    let mut router = router_on(&fake);

    for pass in 0..2 {
        let result = router.route_for_targets(&steady_selection());
        assert!(result.is_ok(), "pass {pass} failed: {result:?}");
    }

    assert_eq!(fake.calls(), Vec::<GraphCall>::new());
    let ids: Vec<u32> = fake.loaded(COMBINED).iter().map(|l| l.id).collect();
    assert_eq!(ids, vec![a]);
}

// Nominal: a selection that only lost a speaker unloads that branch by id
// and leaves the other streaming.
#[test]
fn test_route_after_a_deselection_unloads_only_that_branch() {
    let (fake, a, b) = steady_graph(Some(true));
    let mut router = router_on(&fake);

    let result = router.route_for_targets(&[target(MAC_A, 0)]);

    assert!(result.is_ok(), "route failed: {result:?}");
    assert_eq!(fake.calls(), vec![unload(b)]);
    let ids: Vec<u32> = fake.loaded(COMBINED).iter().map(|l| l.id).collect();
    assert_eq!(ids, vec![a]);
}

// Non-nominal: one branch fails to load — the other is still attempted, and
// the failure comes back as `PipeWire`.
#[test]
fn test_route_with_one_failing_branch_still_loads_the_other() {
    let fake = FakeGraph::with_sinks(&[SINK_A, SINK_B]);
    fake.fail_for(GraphOp::LoadBranch, SINK_A);
    let mut router = router_on(&fake);

    let result = router.route_for_targets(&steady_selection());

    assert_eq!(
        fake.routing_calls(),
        vec![
            clear_stale(COMBINED),
            teardown(COMBINED),
            create(COMBINED),
            load(SINK_A, 0),
            load(SINK_B, 30),
        ]
    );
    let message = match result {
        Err(AudioError::PipeWire(message)) => message,
        other => format!("not a PipeWire error: {other:?}"),
    };
    assert!(message.contains(SINK_A), "unexpected error: {message}");
    assert!(!message.contains(SINK_B), "unexpected error: {message}");
    let sinks: Vec<String> = fake
        .loaded(COMBINED)
        .into_iter()
        .map(|l| l.branch.sink)
        .collect();
    assert_eq!(sinks, vec![SINK_B]);
}

// Non-nominal: several failures come back joined in one `PipeWire` error.
#[test]
fn test_route_with_every_branch_failing_reports_all_failures_in_one_error() {
    let fake = FakeGraph::with_sinks(&[SINK_A, SINK_B]);
    fake.fail(GraphOp::LoadBranch);
    let mut router = router_on(&fake);

    let result = router.route_for_targets(&steady_selection());

    let message = match result {
        Err(AudioError::PipeWire(message)) => message,
        other => format!("not a PipeWire error: {other:?}"),
    };
    assert!(message.contains(SINK_A), "unexpected error: {message}");
    assert!(message.contains(SINK_B), "unexpected error: {message}");
    assert_eq!(
        fake.routing_calls(),
        vec![
            clear_stale(COMBINED),
            teardown(COMBINED),
            create(COMBINED),
            load(SINK_A, 0),
            load(SINK_B, 30),
        ]
    );
}

// Criterion (#147, 2026-10-03): a route that stalls on a branch load —
// the combined sink already in place, the second speaker's load
// unanswered — answers `Unanswered`, not the flattened `PipeWire` the
// handlers map to 500. The branch loaded before the stall is still
// recorded as loaded: in the graph, in the change count, and armed for
// its confirming reload.
#[test]
fn test_route_whose_branch_load_went_unanswered_answers_unanswered_and_keeps_the_load_before_it() {
    let fake = FakeGraph::with_sinks(&[SINK_A, SINK_B, COMBINED]);
    fake.fail_unanswered_for(GraphOp::LoadBranch, SINK_B);
    let mut router = router_on(&fake);

    let result = router.route_for_targets(&steady_selection());

    assert_eq!(result, Err(AudioError::Unanswered));
    assert_eq!(fake.calls(), vec![load(SINK_A, 0), load(SINK_B, 30)]);
    let sinks: Vec<String> = loaded_delays(&fake)
        .into_iter()
        .map(|(_, sink, _)| sink)
        .collect();
    assert_eq!(sinks, vec![SINK_A], "the unanswered load added nothing");
    assert_eq!(router.graph_changes(), 1, "A's load counts as a change");
    assert!(
        router.next_confirmation_due().is_some(),
        "A's load is recorded: its confirming reload is armed"
    );
}

// Criterion (#147, 2026-10-03): a route that stalls on the in-place
// retune of a kept branch answers `Unanswered`. The near miss rides in
// the same pass: the other kept branch's retune is refused with the
// fake's usual `PipeWire` *before* the stall — the only mix one deadline
// per message allows — and must not decide the answer.
#[test]
fn test_route_whose_in_place_retune_went_unanswered_after_a_refused_one_answers_unanswered() {
    let (fake, a, b) = steady_graph(Some(true));
    fake.fail_for(GraphOp::SetBranchDelay, &a.to_string());
    fake.fail_unanswered_for(GraphOp::SetBranchDelay, &b.to_string());
    let mut router = router_on(&fake);

    let result = router.route_for_targets(&[target(MAC_A, 50), target(MAC_B, 90)]);

    assert_eq!(result, Err(AudioError::Unanswered));
    assert_eq!(fake.calls(), vec![set_delay(a, 50), set_delay(b, 90)]);
}

// Criterion (#147, 2026-10-03): a route that stalls on the confirming
// reload answers `Unanswered`, and the reload that went through before
// the stall is still in the graph.
#[test]
fn test_route_whose_confirming_reload_went_unanswered_answers_unanswered() {
    let fake = FakeGraph::with_sinks(&[SINK_A, SINK_B, COMBINED]);
    let (mut router, clock) = router_with_clock(&fake);
    let result = router.route_for_targets(&steady_selection());
    assert!(result.is_ok(), "the first load failed: {result:?}");
    let a = branch_into(&fake, SINK_A);
    let b = branch_into(&fake, SINK_B);

    advance(&clock, CONFIRM_GAP);
    fake.fail_unanswered_for(GraphOp::LoadBranch, SINK_B);
    fake.clear_calls();
    let result = router.route_for_targets(&steady_selection());

    assert_eq!(result, Err(AudioError::Unanswered));
    assert_eq!(
        fake.calls(),
        vec![unload(a), unload(b), load(SINK_A, 0), load(SINK_B, 30)]
    );
    let sinks: Vec<String> = loaded_delays(&fake)
        .into_iter()
        .map(|(_, sink, _)| sink)
        .collect();
    assert_eq!(sinks, vec![SINK_A]);
}

// Non-nominal: empty selection — `NoSpeakerConnected`, and the graph receives
// no call at all, not even a read.
#[test]
fn test_route_with_an_empty_selection_never_calls_the_graph() {
    let (fake, _, _) = steady_graph(Some(true));
    let mut router = router_on(&fake);

    let result = router.route_for_targets(&[]);

    assert!(
        matches!(result, Err(AudioError::NoSpeakerConnected)),
        "unexpected result: {result:?}"
    );
    assert_eq!(fake.all_calls(), Vec::<GraphCall>::new());
}

// Non-nominal: an empty target resolves to nothing, and the trait never
// receives an empty node name.
#[test]
fn test_resolve_target_sink_with_an_empty_target_resolves_nothing() {
    let (fake, _, _) = steady_graph(Some(true));
    let mut router = router_on(&fake);

    let result = router.resolve_target_sink("");

    let message = match result {
        Err(AudioError::PipeWire(message)) => message,
        other => format!("not a PipeWire error: {other:?}"),
    };
    assert!(
        message.contains("no PipeWire sink"),
        "unexpected error: {message}"
    );
    assert_eq!(fake.empty_names_refused(), 0);
    assert_eq!(fake.calls(), Vec::<GraphCall>::new());
}

// Non-nominal: an empty sink name is not "the combined sink exists", and it
// never reaches the trait as a node name.
#[test]
fn test_empty_sink_name_never_reaches_the_graph() {
    let (fake, a, b) = steady_graph(Some(true));
    let mut router = router_on(&fake);

    assert!(matches!(router.combined_sink_exists(""), Ok(false)));
    let retuned = router.retune_branch(
        COMBINED,
        &CombineBranch {
            sink: String::new(),
            latency_ms: 70,
        },
    );

    let message = match retuned {
        Err(AudioError::PipeWire(message)) => message,
        other => format!("not a PipeWire error: {other:?}"),
    };
    assert!(
        message.contains("no PipeWire sink"),
        "unexpected error: {message}"
    );
    assert_eq!(fake.empty_names_refused(), 0);
    assert_eq!(fake.calls(), Vec::<GraphCall>::new());
    let ids: Vec<u32> = fake.loaded(COMBINED).iter().map(|l| l.id).collect();
    assert_eq!(ids, vec![a, b]);
}

// Criterion: `resolve_target_sink` resolves a `bluez_output.*` prefix to the
// live node and an exact node name to itself, reading the graph only.
#[test]
fn test_resolve_target_sink_resolves_a_prefix_and_an_exact_name() {
    let fake = FakeGraph::with_sinks(&["blue2th_combined_old", SINK_A, COMBINED]);
    let mut router = router_on(&fake);

    assert_eq!(
        router.resolve_target_sink(&bluez_sink_prefix(MAC_A)).ok(),
        Some(SINK_A.to_string())
    );
    assert_eq!(
        router.resolve_target_sink(COMBINED).ok(),
        Some(COMBINED.to_string())
    );
    assert!(matches!(
        router.resolve_target_sink(&bluez_sink_prefix(MAC_B)),
        Err(AudioError::PipeWire(_))
    ));
    assert_eq!(fake.calls(), Vec::<GraphCall>::new());
}

// Criterion: `combined_sink_exists` answers from the graph's sinks — a
// namesake sharing the opening characters is not the combined sink.
#[test]
fn test_combined_sink_exists_reads_the_graph() {
    let present = FakeGraph::with_sinks(&[SINK_A, COMBINED]);
    assert!(matches!(
        router_on(&present).combined_sink_exists(COMBINED),
        Ok(true)
    ));

    let namesake = FakeGraph::with_sinks(&[SINK_A, "blue2th_combined_old"]);
    assert!(matches!(
        router_on(&namesake).combined_sink_exists(COMBINED),
        Ok(false)
    ));
}

// Criterion (#147, guard, unreadable is not absent): a sink list that
// cannot be read is an `Err` carrying the graph's own failure — neither
// "exists" nor "absent". The near miss is the list beside it, which reads
// fine and does not hold the combined sink: that one is `Ok(false)`. A
// test with only the failing list passes if absence errs too.
#[test]
fn test_combined_sink_exists_over_an_unreadable_sink_list_is_an_error_not_an_absent_sink() {
    let unreadable = FakeGraph::with_sinks(&[SINK_A, COMBINED]);
    unreadable.fail(GraphOp::Sinks);
    let answer = router_on(&unreadable).combined_sink_exists(COMBINED);
    assert!(
        matches!(&answer, Err(AudioError::PipeWire(m)) if m.contains("Sinks told to fail")),
        "got {answer:?}"
    );
    assert_eq!(unreadable.all_calls(), vec![GraphCall::Sinks]);

    let absent = FakeGraph::with_sinks(&[SINK_A]);
    let answer = router_on(&absent).combined_sink_exists(COMBINED);
    assert!(matches!(&answer, Ok(false)), "got {answer:?}");
}

// Non-nominal: a speaker that came back is loaded alone, and armed alone.
// The first pass at least `CONFIRM_GAP` later reloads that one branch — one
// unload, one load — and nothing else; the other speaker keeps its node
// ids throughout, and the confirming reload does not arm itself.
#[test]
fn test_route_speaker_back_loads_it_alone_then_reloads_it_alone_after_the_gap() {
    let fake = FakeGraph::with_sinks(&[SINK_A, COMBINED]);
    let a = fake.seed_branch(COMBINED, SINK_A, 0, Some(true));
    let (mut router, clock) = router_with_clock(&fake);

    // Pass 1: speaker B is off. Nothing to do.
    let result = router.route_for_targets(&steady_selection());
    assert!(result.is_ok(), "pass 1 failed: {result:?}");
    assert_eq!(fake.calls(), Vec::<GraphCall>::new());

    // Pass 2: B is back, and its branch is loaded alone.
    fake.add_sink(SINK_B);
    fake.clear_calls();
    let result = router.route_for_targets(&steady_selection());
    assert!(result.is_ok(), "pass 2 failed: {result:?}");
    assert_eq!(fake.calls(), vec![load(SINK_B, 30)]);
    let b = branch_into(&fake, SINK_B);
    assert_eq!(branch_into(&fake, SINK_A), a);

    // Pass 3, a gap later: the confirming reload, of B's branch only.
    advance(&clock, CONFIRM_GAP);
    fake.clear_calls();
    let result = router.route_for_targets(&steady_selection());
    assert!(result.is_ok(), "pass 3 failed: {result:?}");
    let calls = fake.calls();
    let changes: Vec<GraphCall> = calls
        .iter()
        .filter(|c| changes_the_graph(c))
        // Cloned to compare against literals below.
        .cloned()
        .collect();
    assert_eq!(changes, vec![unload(b), load(SINK_B, 30)]);
    assert_eq!(branch_into(&fake, SINK_A), a, "A was never touched");
    let confirmed = branch_into(&fake, SINK_B);
    assert_ne!(confirmed, b, "B's branch was reloaded");

    // Pass 4, another gap later: the confirming reload did not arm another.
    advance(&clock, CONFIRM_GAP);
    fake.clear_calls();
    let result = router.route_for_targets(&steady_selection());
    assert!(result.is_ok(), "pass 4 failed: {result:?}");
    assert_eq!(fake.calls(), Vec::<GraphCall>::new());
    let ids: Vec<u32> = fake.loaded(COMBINED).iter().map(|l| l.id).collect();
    assert_eq!(ids, vec![a, confirmed]);
}

// Criterion (guard, the gap): the app selects, then plays, a moment
// apart — two passes within the gap. The second must reload nothing: a
// reload a few milliseconds after the load broke a start that worked (#75).
// Only a pass at least `CONFIRM_GAP` after the load reloads, then both
// branches loaded together are reloaded, each alone.
#[test]
fn test_route_select_then_play_within_the_gap_reloads_nothing() {
    let fake = FakeGraph::with_sinks(&[SINK_A, SINK_B, COMBINED]);
    let (mut router, clock) = router_with_clock(&fake);

    let result = router.route_for_targets(&steady_selection());
    assert!(result.is_ok(), "select failed: {result:?}");
    assert_eq!(fake.calls(), vec![load(SINK_A, 0), load(SINK_B, 30)]);
    let a = branch_into(&fake, SINK_A);
    let b = branch_into(&fake, SINK_B);

    // Play, one second later.
    advance(&clock, Duration::from_secs(1));
    fake.clear_calls();
    let result = router.route_for_targets(&steady_selection());
    assert!(result.is_ok(), "play failed: {result:?}");
    assert_eq!(fake.calls(), Vec::<GraphCall>::new(), "nothing reloaded");

    // The repair tick after the gap reloads both, each alone.
    advance(&clock, CONFIRM_GAP);
    fake.clear_calls();
    let result = router.route_for_targets(&steady_selection());
    assert!(result.is_ok(), "tick failed: {result:?}");
    let changes: Vec<GraphCall> = fake.calls().into_iter().filter(changes_the_graph).collect();
    assert_eq!(
        changes.len(),
        4,
        "one unload and one load each: {changes:?}"
    );
    for (id, sink, delay) in [(a, SINK_A, 0), (b, SINK_B, 30)] {
        let unloaded = changes.iter().position(|c| *c == unload(id));
        let reloaded = changes.iter().position(|c| *c == load(sink, delay));
        assert!(
            matches!((unloaded, reloaded), (Some(u), Some(l)) if u < l),
            "{sink} unloaded then reloaded: {changes:?}"
        );
    }
}

// Non-nominal: two speakers come back in the same pass — both are loaded
// and both armed; the next pass reloads both, each unloaded before its own
// reload, and nothing else: the third speaker, playing all along, is never
// touched.
#[test]
fn test_route_two_speakers_back_together_reload_both_and_nothing_else() {
    let sink_c = "bluez_output.AA_BB_CC_DD_EE_03.1";
    let fake = FakeGraph::with_sinks(&[sink_c, COMBINED]);
    let c = fake.seed_branch(COMBINED, sink_c, 60, Some(true));
    let selection = vec![
        target(MAC_A, 0),
        target(MAC_B, 30),
        target("AA:BB:CC:DD:EE:03", 60),
    ];
    let (mut router, clock) = router_with_clock(&fake);

    let result = router.route_for_targets(&selection);
    assert!(result.is_ok(), "pass 1 failed: {result:?}");
    assert_eq!(fake.calls(), Vec::<GraphCall>::new());

    fake.add_sink(SINK_A);
    fake.add_sink(SINK_B);
    fake.clear_calls();
    let result = router.route_for_targets(&selection);
    assert!(result.is_ok(), "pass 2 failed: {result:?}");
    assert_eq!(fake.calls(), vec![load(SINK_A, 0), load(SINK_B, 30)]);
    let a = branch_into(&fake, SINK_A);
    let b = branch_into(&fake, SINK_B);

    advance(&clock, CONFIRM_GAP);
    fake.clear_calls();
    let result = router.route_for_targets(&selection);
    assert!(result.is_ok(), "pass 3 failed: {result:?}");
    let changes: Vec<GraphCall> = fake.calls().into_iter().filter(changes_the_graph).collect();
    assert_eq!(
        changes.len(),
        4,
        "one unload and one load each: {changes:?}"
    );
    for (id, sink, delay_ms) in [(a, SINK_A, 0), (b, SINK_B, 30)] {
        let unloaded_at = changes.iter().position(|c| *c == unload(id));
        let loaded_at = changes.iter().position(|c| *c == load(sink, delay_ms));
        assert!(
            unloaded_at.is_some() && loaded_at.is_some() && unloaded_at < loaded_at,
            "{sink} is unloaded, then reloaded: {changes:?}"
        );
    }
    assert_eq!(branch_into(&fake, sink_c), c, "C was never touched");

    fake.clear_calls();
    let result = router.route_for_targets(&selection);
    assert!(result.is_ok(), "pass 4 failed: {result:?}");
    assert_eq!(fake.calls(), Vec::<GraphCall>::new());
}

// Nominal: a speaker added mid-playback is loaded alone; the branch
// already playing keeps its id, and the pass after it, still inside the
// gap, touches nothing.
#[test]
fn test_route_added_speaker_leaves_the_playing_branch_untouched() {
    let fake = FakeGraph::with_sinks(&[SINK_A, SINK_B, COMBINED]);
    let a = fake.seed_branch(COMBINED, SINK_A, 0, Some(true));
    let mut router = router_on(&fake);

    let result = router.route_for_targets(&[target(MAC_A, 0)]);
    assert!(result.is_ok(), "pass 1 failed: {result:?}");
    assert_eq!(fake.calls(), Vec::<GraphCall>::new());

    fake.clear_calls();
    let result = router.route_for_targets(&steady_selection());
    assert!(result.is_ok(), "pass 2 failed: {result:?}");
    assert_eq!(fake.calls(), vec![load(SINK_B, 30)]);
    assert_eq!(branch_into(&fake, SINK_A), a);

    fake.clear_calls();
    let result = router.route_for_targets(&steady_selection());
    assert!(result.is_ok(), "pass 3 failed: {result:?}");
    assert_eq!(fake.calls(), Vec::<GraphCall>::new());
}

// Nominal: moving one speaker's offset retunes its branch in place — one
// `set_branch_delay` on that branch, at exactly the offset, and nothing
// else. No branch is created or destroyed; the other is not touched.
#[test]
fn test_route_offset_change_calls_set_branch_delay_and_nothing_else() {
    let (fake, a, b) = steady_graph(Some(true));
    let mut router = router_on(&fake);

    let result = router.route_for_targets(&[target(MAC_A, 0), target(MAC_B, 120)]);

    assert!(result.is_ok(), "route failed: {result:?}");
    assert_eq!(fake.calls(), vec![set_delay(b, 120)]);
    assert_eq!(
        loaded_delays(&fake),
        vec![(a, SINK_A.to_string(), 0), (b, SINK_B.to_string(), 120)]
    );
}

// Criterion: `reconcile_combined` unloads dead branches, unloads
// `to_unload`, retunes `to_retune` and loads `to_load`, in that order —
// all four in one pass: C's branch is dead, D is deselected, A's offset
// moved and B has no branch yet.
#[test]
fn test_route_unloads_dead_then_unwanted_then_retunes_then_loads() {
    let sink_c = "bluez_output.AA_BB_CC_DD_EE_03.1";
    let sink_d = "bluez_output.AA_BB_CC_DD_EE_04.1";
    let fake = FakeGraph::with_sinks(&[SINK_A, SINK_B, sink_c, sink_d, COMBINED]);
    let a = fake.seed_branch(COMBINED, SINK_A, 0, Some(true));
    let dead_c = fake.seed_branch(COMBINED, sink_c, 60, Some(false));
    let d = fake.seed_branch(COMBINED, sink_d, 30, Some(true));
    let mut router = router_on(&fake);

    let result = router.route_for_targets(&[
        target(MAC_A, 100),
        target(MAC_B, 30),
        target("AA:BB:CC:DD:EE:03", 60),
    ]);

    assert!(result.is_ok(), "route failed: {result:?}");
    assert_eq!(
        fake.calls(),
        vec![
            unload(dead_c),
            unload(d),
            set_delay(a, 100),
            load(SINK_B, 30),
            load(sink_c, 60),
        ]
    );
}

// Criterion: a build from nothing arms every branch it loaded — the silent
// start seen on the #81 build is the case the confirmation exists for — and
// the first pass a gap later reloads each of them once.
#[test]
fn test_route_build_from_nothing_confirms_every_branch_it_loaded() {
    let fake = FakeGraph::with_sinks(&[SINK_A, SINK_B]);
    let (mut router, clock) = router_with_clock(&fake);

    let result = router.route_for_targets(&steady_selection());
    assert!(result.is_ok(), "build failed: {result:?}");
    assert!(
        fake.calls().contains(&create(COMBINED)),
        "{:?}",
        fake.calls()
    );
    let a = branch_into(&fake, SINK_A);
    let b = branch_into(&fake, SINK_B);

    advance(&clock, CONFIRM_GAP);
    fake.clear_calls();
    let result = router.route_for_targets(&steady_selection());
    assert!(result.is_ok(), "confirming pass failed: {result:?}");
    let changes: Vec<GraphCall> = fake.calls().into_iter().filter(changes_the_graph).collect();
    assert_eq!(changes.len(), 4, "both reloaded, each alone: {changes:?}");
    assert!(changes.contains(&unload(a)) && changes.contains(&unload(b)));

    advance(&clock, CONFIRM_GAP);
    fake.clear_calls();
    let result = router.route_for_targets(&steady_selection());
    assert!(
        result.is_ok(),
        "pass after the confirmation failed: {result:?}"
    );
    assert_eq!(fake.calls(), Vec::<GraphCall>::new(), "confirmed once only");
}

// Criterion: a speaker deselected and reselected while its sink never left
// is loaded again, and that load is confirmed too — deselect/reselect is
// the operator's workaround for the silent start, so its load must not be
// the one left unconfirmed. The other speaker is never touched.
#[test]
fn test_route_reselected_speaker_is_confirmed_alone_after_the_gap() {
    let (fake, a, b) = steady_graph(Some(true));
    let (mut router, clock) = router_with_clock(&fake);

    // Pass 1: B deselected, its branch goes. Pass 2: B reselected.
    let result = router.route_for_targets(&[target(MAC_A, 0)]);
    assert!(result.is_ok(), "deselect failed: {result:?}");
    assert_eq!(fake.calls(), vec![unload(b)]);
    fake.clear_calls();
    let result = router.route_for_targets(&steady_selection());
    assert!(result.is_ok(), "reselect failed: {result:?}");
    assert_eq!(fake.calls(), vec![load(SINK_B, 30)]);
    let reselected = branch_into(&fake, SINK_B);

    // A gap later: B's branch alone is reloaded.
    advance(&clock, CONFIRM_GAP);
    fake.clear_calls();
    let result = router.route_for_targets(&steady_selection());
    assert!(result.is_ok(), "confirming pass failed: {result:?}");
    let changes: Vec<GraphCall> = fake.calls().into_iter().filter(changes_the_graph).collect();
    assert_eq!(changes, vec![unload(reselected), load(SINK_B, 30)]);
    assert_eq!(branch_into(&fake, SINK_A), a);
}

// Criterion (guard, only after the gap): a branch loaded again in the very
// pass its confirmation falls due — ruled dead, then replaced — is not
// reloaded milliseconds after that load, which broke a start that worked
// (#75). It waits for a gap of its own; the other branch owed in that
// pass is confirmed alone.
#[test]
fn test_route_branch_replaced_when_its_confirmation_falls_due_waits_its_own_gap() {
    let fake = FakeGraph::with_sinks(&[SINK_A, SINK_B, COMBINED]);
    let (mut router, clock) = router_with_clock(&fake);
    let result = router.route_for_targets(&steady_selection());
    assert!(result.is_ok(), "loading pass failed: {result:?}");
    let a = branch_into(&fake, SINK_A);
    let b = branch_into(&fake, SINK_B);

    // A gap later both are owed, but B's branch has just died.
    fake.set_branch_liveness(b, Some(false));
    advance(&clock, CONFIRM_GAP);
    fake.clear_calls();
    let result = router.route_for_targets(&steady_selection());
    assert!(result.is_ok(), "confirming pass failed: {result:?}");
    let changes: Vec<GraphCall> = fake.calls().into_iter().filter(changes_the_graph).collect();
    assert_eq!(
        changes,
        vec![unload(b), load(SINK_B, 30), unload(a), load(SINK_A, 0)],
        "B replaced once, A confirmed alone"
    );
    let replaced = branch_into(&fake, SINK_B);

    // Its own gap later, the replacement is confirmed, and A is not again.
    advance(&clock, CONFIRM_GAP);
    fake.clear_calls();
    let result = router.route_for_targets(&steady_selection());
    assert!(
        result.is_ok(),
        "pass after the replacement failed: {result:?}"
    );
    let changes: Vec<GraphCall> = fake.calls().into_iter().filter(changes_the_graph).collect();
    assert_eq!(changes, vec![unload(replaced), load(SINK_B, 30)]);
}

// Criterion (guard, a build clears everything armed before it): a reload
// armed before a build is forgotten by it, even for a branch the build did
// not reload. The case: the combined sink vanished (a daemon restart), the
// build's load of B reported an error, and B's branch came up anyway — as a
// `PipeWireGraph` load can, when the module is kept and a later round trip
// fails. The next pass, a gap after the first arming but not after the
// build, owes nothing.
#[test]
fn test_route_build_forgets_the_reloads_armed_before_it() {
    let fake = FakeGraph::with_sinks(&[SINK_A, SINK_B]);
    let (mut router, clock) = router_with_clock(&fake);
    let result = router.route_for_targets(&steady_selection());
    assert!(result.is_ok(), "first build failed: {result:?}");

    // One second later: the combined sink is gone, and B's load errs.
    fake.remove_sink(COMBINED);
    fake.fail_for(GraphOp::LoadBranch, SINK_B);
    advance(&clock, Duration::from_secs(1));
    let result = router.route_for_targets(&steady_selection());
    assert!(result.is_err(), "B's load was made to fail: {result:?}");
    fake.clear_failures();
    let b = fake.seed_branch(COMBINED, SINK_B, 30, Some(true));

    // A gap after the first build, inside the gap after the second one.
    advance(&clock, CONFIRM_GAP - Duration::from_secs(1));
    fake.clear_calls();
    let result = router.route_for_targets(&steady_selection());
    assert!(result.is_ok(), "pass after the rebuild failed: {result:?}");
    assert_eq!(fake.calls(), Vec::<GraphCall>::new());
    assert_eq!(branch_into(&fake, SINK_B), b);
}

// Non-nominal: an empty target names no node, so it is answered without the
// graph being read at all — not even the sink list. A read is a round trip
// to the graph thread, and one that could only ever answer "nothing".
#[test]
fn test_empty_target_is_answered_without_reading_the_graph() {
    let (fake, _, _) = steady_graph(Some(true));
    let mut router = router_on(&fake);

    assert!(router.resolve_target_sink("").is_err());
    assert!(matches!(router.combined_sink_exists(""), Ok(false)));

    assert_eq!(fake.all_calls(), Vec::<GraphCall>::new());
}

// Criterion: the confirmation register is a field of `AudioRouter` — a
// router that armed a reload changes nothing for a second router in the
// same process. The second router's passes run a gap after the first
// router armed, so a shared register would hand them its reload: they
// would consume it, and the first router would then owe nothing.
#[test]
fn test_two_routers_do_not_share_the_confirmation_register() {
    // The first router goes through a speaker coming back, which arms it.
    let first_fake = FakeGraph::with_sinks(&[SINK_A, COMBINED]);
    first_fake.seed_branch(COMBINED, SINK_A, 0, Some(true));
    let (mut first, first_clock) = router_with_clock(&first_fake);
    let result = first.route_for_targets(&steady_selection());
    assert!(result.is_ok(), "first router, pass 1: {result:?}");
    first_fake.add_sink(SINK_B);
    let result = first.route_for_targets(&steady_selection());
    assert!(result.is_ok(), "first router, pass 2: {result:?}");
    assert!(
        first_fake.calls().contains(&load(SINK_B, 30)),
        "the first router never wired the returning speaker"
    );

    // The second router owes nothing: its steady graph stays untouched.
    let sink_c = "bluez_output.AA_BB_CC_DD_EE_03.1";
    let second_fake = FakeGraph::with_sinks(&[sink_c, COMBINED]);
    let c = second_fake.seed_branch(COMBINED, sink_c, 0, Some(true));
    let (mut second, second_clock) = router_with_clock(&second_fake);
    advance(&second_clock, CONFIRM_GAP);
    for pass in 0..2 {
        let result = second.route_for_targets(&[target("AA:BB:CC:DD:EE:03", 0)]);
        assert!(result.is_ok(), "second router, pass {pass}: {result:?}");
    }
    assert_eq!(second_fake.calls(), Vec::<GraphCall>::new());
    let ids: Vec<u32> = second_fake.loaded(COMBINED).iter().map(|l| l.id).collect();
    assert_eq!(ids, vec![c]);

    // And the first router still owes its own reload, a gap later.
    advance(&first_clock, CONFIRM_GAP);
    first_fake.clear_calls();
    let result = first.route_for_targets(&steady_selection());
    assert!(result.is_ok(), "first router, pass 3: {result:?}");
    assert!(
        first_fake.calls().iter().any(changes_the_graph),
        "the first router lost its armed reload: {:?}",
        first_fake.calls()
    );
}

// Criterion: `retune_branch` sets the new delay on that speaker's branch,
// in place — same id — and the combined sink and the other branch receive
// no call.
#[test]
fn test_retune_branch_touches_only_that_speakers_branch() {
    let (fake, a, b) = steady_graph(Some(true));
    let mut router = router_on(&fake);

    let result = router.retune_branch(
        COMBINED,
        &CombineBranch {
            sink: bluez_sink_prefix(MAC_A),
            latency_ms: 120,
        },
    );

    assert!(result.is_ok(), "retune failed: {result:?}");
    assert_eq!(fake.calls(), vec![set_delay(a, 120)]);
    assert_eq!(
        loaded_delays(&fake),
        vec![(a, SINK_A.to_string(), 120), (b, SINK_B.to_string(), 30)]
    );
}

// Criterion (guard, retune, never reload): `retune_branch` never calls
// `unload_branch` or `load_branch` — not even when the delay node rejects
// the parameter. The near miss is a fallback to the #79 reload on error:
// the retune comes back `Err`, and the branch is left as it was.
#[test]
fn test_retune_branch_never_unloads() {
    let (fake, a, b) = steady_graph(Some(true));
    fake.fail(GraphOp::SetBranchDelay);
    let mut router = router_on(&fake);

    let result = router.retune_branch(
        COMBINED,
        &CombineBranch {
            sink: bluez_sink_prefix(MAC_A),
            latency_ms: 120,
        },
    );

    assert!(
        matches!(result, Err(AudioError::PipeWire(_))),
        "a rejected delay is reported, got {result:?}"
    );
    assert_eq!(fake.calls(), vec![set_delay(a, 120)]);
    assert_eq!(
        loaded_delays(&fake),
        vec![(a, SINK_A.to_string(), 0), (b, SINK_B.to_string(), 30)]
    );
}

// Criterion: when the speaker has no branch, `retune_branch` does nothing
// and returns `Ok` — the new offset is stored, and the branch loads with it
// when the reconciliation next loads that speaker.
#[test]
fn test_retune_branch_without_a_branch_for_the_speaker_changes_nothing() {
    let fake = FakeGraph::with_sinks(&[SINK_A, SINK_B, COMBINED]);
    let b = fake.seed_branch(COMBINED, SINK_B, 30, Some(true));
    let mut router = router_on(&fake);

    let result = router.retune_branch(
        COMBINED,
        &CombineBranch {
            sink: bluez_sink_prefix(MAC_A),
            latency_ms: 120,
        },
    );

    assert!(result.is_ok(), "retune failed: {result:?}");
    assert_eq!(fake.calls(), Vec::<GraphCall>::new());
    assert_eq!(loaded_delays(&fake), vec![(b, SINK_B.to_string(), 30)]);
}

// Non-nominal: the delay node rejected a retune, so the branch still runs
// at its old delay. The next reconcile finds it at the wrong delay and
// retunes it — still in place, never by unloading it.
#[test]
fn test_route_after_a_rejected_retune_retunes_in_place_on_the_next_pass() {
    let (fake, a, b) = steady_graph(Some(true));
    fake.fail(GraphOp::SetBranchDelay);
    let mut router = router_on(&fake);
    let rejected = router.retune_branch(
        COMBINED,
        &CombineBranch {
            sink: bluez_sink_prefix(MAC_A),
            latency_ms: 120,
        },
    );
    assert!(rejected.is_err(), "the retune was rejected: {rejected:?}");

    fake.clear_failures();
    fake.clear_calls();
    let result = router.route_for_targets(&[target(MAC_A, 120), target(MAC_B, 30)]);

    assert!(result.is_ok(), "route failed: {result:?}");
    assert_eq!(fake.calls(), vec![set_delay(a, 120)]);
    assert_eq!(
        loaded_delays(&fake),
        vec![(a, SINK_A.to_string(), 120), (b, SINK_B.to_string(), 30)]
    );
}

// Non-nominal: retuning a speaker whose sink has vanished errs rather than
// touching anything.
#[test]
fn test_retune_branch_for_a_vanished_speaker_changes_nothing() {
    let (fake, _, _) = steady_graph(Some(true));
    fake.remove_sink(SINK_A);
    let mut router = router_on(&fake);

    let result = router.retune_branch(
        COMBINED,
        &CombineBranch {
            sink: bluez_sink_prefix(MAC_A),
            latency_ms: 120,
        },
    );

    let message = match result {
        Err(AudioError::PipeWire(message)) => message,
        other => format!("not a PipeWire error: {other:?}"),
    };
    assert!(
        message.contains("no PipeWire sink"),
        "unexpected error: {message}"
    );
    assert_eq!(fake.calls(), Vec::<GraphCall>::new());
}

// Criterion: `teardown` hands the whole job to the graph — one call, and the
// sink and its branches are gone.
#[test]
fn test_teardown_asks_the_graph_once_and_leaves_nothing() {
    let (fake, _, _) = steady_graph(Some(true));
    let mut router = router_on(&fake);

    let result = router.teardown(COMBINED);

    assert!(result.is_ok(), "teardown failed: {result:?}");
    assert_eq!(fake.calls(), vec![teardown(COMBINED)]);
    assert!(fake.loaded(COMBINED).is_empty());
    assert!(!fake.sink_names().iter().any(|s| s == COMBINED));
}

// Criterion: the volume is written per speaker sink — the MAC is resolved to
// the live node, and the level is clamped before it reaches the graph.
#[test]
fn test_set_sink_volume_resolves_the_speaker_sink_and_clamps_the_level() {
    let (fake, _, _) = steady_graph(Some(true));
    let mut router = router_on(&fake);

    let result = router.set_sink_volume(MAC_B, 1.5);

    assert!(result.is_ok(), "set volume failed: {result:?}");
    assert_eq!(
        fake.calls(),
        vec![GraphCall::SetSinkVolume {
            sink: SINK_B.to_string(),
            level: 1.0
        }]
    );
}

// Non-nominal: no sink for that speaker — an error, and no write.
#[test]
fn test_set_sink_volume_without_a_sink_writes_nothing() {
    let fake = FakeGraph::with_sinks(&[SINK_A]);
    let mut router = router_on(&fake);

    let result = router.set_sink_volume(MAC_B, 0.4);

    let message = match result {
        Err(AudioError::PipeWire(message)) => message,
        other => format!("not a PipeWire error: {other:?}"),
    };
    assert!(
        message.contains("no PipeWire sink"),
        "unexpected error: {message}"
    );
    assert_eq!(fake.calls(), Vec::<GraphCall>::new());
}

// ─── #145: one sink-list read, and "unreadable" told from "absent" ──────

// Criterion (#145, guard, exactly once): two speakers' volumes come from
// one `sinks()` read. The near miss is the second speaker: with one, a
// per-speaker read also makes a single call. The levels differ, so a
// result handed back in the wrong order fails too.
#[test]
fn test_sink_volumes_reads_the_sink_list_once_for_two_speakers() {
    let fake = FakeGraph::with_sinks(&[SINK_A, SINK_B]);
    fake.set_volume(SINK_A, 0.25);
    fake.set_volume(SINK_B, 0.75);
    let mut router = router_on(&fake);

    let levels = router.sink_volumes(&[MAC_A.to_string(), MAC_B.to_string()]);

    assert!(
        matches!(&levels, Ok(l) if *l == vec![Some(0.25), Some(0.75)]),
        "got {levels:?}"
    );
    let reads = fake
        .all_calls()
        .iter()
        .filter(|call| matches!(call, GraphCall::Sinks))
        .count();
    assert_eq!(reads, 1, "calls: {:?}", fake.all_calls());
}

// Criterion (#145, guard, stops at the first failure): an unreadable sink
// list is an `Err`, and nothing more is asked of the graph — no second
// `sinks()` for the second speaker, no `SinkVolume`. Two speakers, so a
// loop that carries on after the failure has somewhere to go.
#[test]
fn test_sink_volumes_errs_on_an_unreadable_sink_list_and_asks_nothing_more() {
    let fake = FakeGraph::with_sinks(&[SINK_A, SINK_B]);
    fake.set_volume(SINK_A, 0.25);
    fake.set_volume(SINK_B, 0.75);
    fake.fail(GraphOp::Sinks);
    let mut router = router_on(&fake);

    let levels = router.sink_volumes(&[MAC_A.to_string(), MAC_B.to_string()]);

    assert!(
        matches!(&levels, Err(AudioError::PipeWire(_))),
        "got {levels:?}"
    );
    assert_eq!(fake.all_calls(), vec![GraphCall::Sinks]);
}

// Criterion (#145, guard, absent is not unreadable): a speaker whose sink
// is missing from a list that read fine is `Ok(None)` in its own slot —
// not an `Err`, and not a reason to drop the other speaker's level.
#[test]
fn test_sink_volumes_answers_none_for_an_absent_sink_on_a_readable_list() {
    let fake = FakeGraph::with_sinks(&[SINK_A]);
    fake.set_volume(SINK_A, 0.25);
    let mut router = router_on(&fake);

    let levels = router.sink_volumes(&[MAC_A.to_string(), MAC_B.to_string()]);

    assert!(
        matches!(&levels, Ok(l) if *l == vec![Some(0.25), None]),
        "got {levels:?}"
    );
}

// Criterion (#145): `set_sink_volume` over an unreadable sink list returns
// the graph's failure — the fake's own message — and never claims the
// speaker has no sink, which would send the user looking for a speaker
// problem. Nothing is written.
#[test]
fn test_set_sink_volume_on_an_unreadable_sink_list_carries_the_graph_failure() {
    let fake = FakeGraph::with_sinks(&[SINK_A]);
    fake.fail(GraphOp::Sinks);
    let mut router = router_on(&fake);

    let result = router.set_sink_volume(MAC_A, 0.4);

    let message = match result {
        Err(AudioError::PipeWire(message)) => message,
        other => format!("not a PipeWire error: {other:?}"),
    };
    assert!(
        message.contains("Sinks told to fail"),
        "the graph failure is lost: {message}"
    );
    assert!(
        !message.contains("no PipeWire sink"),
        "an unreadable list is not an absent sink: {message}"
    );
    assert_eq!(fake.calls(), Vec::<GraphCall>::new());
}

// Criterion: the volume is read per speaker sink, over-amplification
// included — the 153% is passed on, not clamped, so `reported_volume` can
// refuse it — and an absent speaker reads as `None`. Nothing is written.
#[test]
fn test_sink_volumes_reads_each_speaker_sink_unclamped() {
    let (fake, _, _) = steady_graph(Some(true));
    fake.set_volume(SINK_A, 0.59);
    fake.set_volume(SINK_B, 1.53);
    let mut router = router_on(&fake);

    let levels = router.sink_volumes(&[
        MAC_A.to_string(),
        MAC_B.to_string(),
        "AA:BB:CC:DD:EE:03".to_string(),
    ]);

    assert!(
        matches!(&levels, Ok(l) if *l == vec![Some(0.59), Some(1.53), None]),
        "got {levels:?}"
    );
    assert!(fake.all_calls().contains(&GraphCall::SinkVolume {
        sink: SINK_A.to_string()
    }));
    assert_eq!(fake.calls(), Vec::<GraphCall>::new());
}

// Criterion (#145, guard, the empty value asks nothing): no speaker, no
// read — not even of a sink list that would fail. The near miss is the
// failing list: without the guard the empty read errs, and `/playback`
// would report a stall for a selection that asked the graph nothing.
#[test]
fn test_sink_volumes_of_no_speaker_asks_the_graph_nothing() {
    let fake = FakeGraph::with_sinks(&[SINK_A]);
    fake.fail(GraphOp::Sinks);
    let mut router = router_on(&fake);

    let levels = router.sink_volumes(&[]);

    assert!(matches!(&levels, Ok(l) if l.is_empty()), "got {levels:?}");
    assert_eq!(fake.all_calls(), Vec::<GraphCall>::new());
}

// Criterion (#145, #148, guard, no level is not a failure): a sink that
// is listed but has no level is `None` in its own slot, like an absent
// one, and the other speaker is still read — `Graph::sink_volume`
// answered `Ok(None)`, which is not a reason to stop. The near miss is
// the listed `SINK_A` with no level set: an implementation reading every
// `None` as a failure errs here only.
#[test]
fn test_sink_volumes_answers_none_for_a_listed_sink_with_no_readable_level() {
    let fake = FakeGraph::with_sinks(&[SINK_A, SINK_B]);
    fake.set_volume(SINK_B, 0.75);
    let mut router = router_on(&fake);

    let levels = router.sink_volumes(&[MAC_A.to_string(), MAC_B.to_string()]);

    assert!(
        matches!(&levels, Ok(l) if *l == vec![None, Some(0.75)]),
        "got {levels:?}"
    );
}

// Criterion (#148, guard, stops at the first failure): the first
// speaker's level read failing is an `Err`, and the second speaker's
// level is never asked — while the graph does not answer, that read would
// only wait out its own timeout. The near miss is `SINK_B`, listed and
// readable: a read that collects every result and then looks for an
// `Err` still errs, but asks for it.
#[test]
fn test_sink_volumes_errs_on_the_first_failed_level_read_and_asks_nothing_more() {
    let fake = FakeGraph::with_sinks(&[SINK_A, SINK_B]);
    fake.set_volume(SINK_A, 0.25);
    fake.set_volume(SINK_B, 0.75);
    fake.fail_for(GraphOp::SinkVolume, SINK_A);
    let mut router = router_on(&fake);

    let levels = router.sink_volumes(&[MAC_A.to_string(), MAC_B.to_string()]);

    assert!(
        matches!(&levels, Err(AudioError::PipeWire(m)) if m.contains("SinkVolume told to fail")),
        "the graph's failure is handed back, got {levels:?}"
    );
    assert_eq!(
        fake.all_calls(),
        vec![
            GraphCall::Sinks,
            GraphCall::SinkVolume {
                sink: SINK_A.to_string()
            },
        ],
        "nothing is asked after the failed read"
    );
}

// Criterion (#148, guard, a failure is never "no level"): the *last*
// speaker's level read failing is an `Err` too, not `None` in its slot.
// The near miss is that position: with nothing left to ask, only the
// swallowing of the failure tells the two apart.
#[test]
fn test_sink_volumes_errs_when_the_last_level_read_fails() {
    let fake = FakeGraph::with_sinks(&[SINK_A, SINK_B]);
    fake.set_volume(SINK_A, 0.25);
    fake.set_volume(SINK_B, 0.25);
    fake.fail_for(GraphOp::SinkVolume, SINK_B);
    let mut router = router_on(&fake);

    let levels = router.sink_volumes(&[MAC_A.to_string(), MAC_B.to_string()]);

    assert!(
        matches!(&levels, Err(AudioError::PipeWire(_))),
        "a failed read is not a sink without a level, got {levels:?}"
    );
}

// Criterion (#80): the router exposes when its earliest confirming reload
// falls due — nothing before a load, the load's time plus `CONFIRM_GAP`
// after it, and nothing again once the reload ran, since a confirming
// reload does not arm itself.
#[test]
fn test_router_next_confirmation_due_follows_the_branch_it_loaded() {
    let fake = FakeGraph::with_sinks(&[SINK_A, COMBINED]);
    let (mut router, clock) = router_with_clock(&fake);
    let loaded_at = *clock.lock().unwrap();
    assert_eq!(router.next_confirmation_due(), None, "nothing loaded yet");

    let result = router.route_for_targets(&[target(MAC_A, 0)]);
    assert!(result.is_ok(), "the load failed: {result:?}");
    assert_eq!(fake.calls(), vec![load(SINK_A, 0)]);
    assert_eq!(
        router.next_confirmation_due(),
        Some(loaded_at + CONFIRM_GAP)
    );

    advance(&clock, CONFIRM_GAP);
    fake.clear_calls();
    let result = router.route_for_targets(&[target(MAC_A, 0)]);
    assert!(result.is_ok(), "the reload failed: {result:?}");
    assert!(
        fake.calls().contains(&load(SINK_A, 0)),
        "control: the reload ran, got {:?}",
        fake.calls()
    );
    assert_eq!(router.next_confirmation_due(), None);
}

// Criterion (#80): a pass logs what woke it only when it changed the graph,
// and `graph_changes` is how it tells. Every change the graph accepts
// counts once: here a dead unload, an unwanted unload, a retune and two
// loads — five calls, five changes. A steady pass after it counts none.
#[test]
fn test_router_graph_changes_counts_each_change_the_graph_accepted() {
    let sink_c = "bluez_output.AA_BB_CC_DD_EE_03.1";
    let sink_d = "bluez_output.AA_BB_CC_DD_EE_04.1";
    let fake = FakeGraph::with_sinks(&[SINK_A, SINK_B, sink_c, sink_d, COMBINED]);
    fake.seed_branch(COMBINED, SINK_A, 0, Some(true));
    fake.seed_branch(COMBINED, sink_c, 60, Some(false));
    fake.seed_branch(COMBINED, sink_d, 30, Some(true));
    let mut router = router_on(&fake);
    let selection = [
        target(MAC_A, 100),
        target(MAC_B, 30),
        target("AA:BB:CC:DD:EE:03", 60),
    ];
    assert_eq!(router.graph_changes(), 0, "a fresh router changed nothing");

    let result = router.route_for_targets(&selection);
    assert!(result.is_ok(), "route failed: {result:?}");
    assert_eq!(fake.calls().len(), 5, "calls: {:?}", fake.calls());
    assert_eq!(router.graph_changes(), 5);

    let result = router.route_for_targets(&selection);
    assert!(result.is_ok(), "steady pass failed: {result:?}");
    assert_eq!(router.graph_changes(), 5, "a steady pass changes nothing");
}

// Criterion (#80): a build from nothing is a change — the combined sink
// counts once, each branch loaded once more: one sink and two branches are
// three. The confirming reload a gap later is two more, one unload and one
// load per branch reloaded: here both, so four.
#[test]
fn test_router_graph_changes_counts_a_build_and_its_confirming_reload() {
    let fake = FakeGraph::with_sinks(&[SINK_A, SINK_B]);
    let (mut router, clock) = router_with_clock(&fake);

    let result = router.route_for_targets(&steady_selection());
    assert!(result.is_ok(), "build failed: {result:?}");
    assert_eq!(router.graph_changes(), 3, "calls: {:?}", fake.calls());

    advance(&clock, CONFIRM_GAP);
    let result = router.route_for_targets(&steady_selection());
    assert!(result.is_ok(), "confirming pass failed: {result:?}");
    assert_eq!(router.graph_changes(), 3 + 4, "calls: {:?}", fake.calls());
}

// Criterion (#80): a call the graph refused changed nothing, so it does
// not count — or a pass that only failed would log that it changed the
// graph. The near misses, each attempted and recorded by the fake: a
// rejected retune, a refused unload of an unwanted branch, and a refused
// unload of a dead one, whose replacement load still counts.
#[test]
fn test_router_graph_changes_counts_no_call_the_graph_refused() {
    let (fake, a, _) = steady_graph(Some(true));
    fake.fail(GraphOp::SetBranchDelay);
    let mut router = router_on(&fake);
    let result = router.route_for_targets(&[target(MAC_A, 120), target(MAC_B, 30)]);
    assert!(result.is_err(), "the retune was rejected: {result:?}");
    assert_eq!(fake.calls(), vec![set_delay(a, 120)], "it was attempted");
    assert_eq!(router.graph_changes(), 0);

    let (fake, a, _) = steady_graph(Some(true));
    fake.fail(GraphOp::UnloadBranch);
    let mut router = router_on(&fake);
    let result = router.route_for_targets(&[target(MAC_B, 30)]);
    assert!(result.is_err(), "the unload was refused: {result:?}");
    assert_eq!(fake.calls(), vec![unload(a)], "it was attempted");
    assert_eq!(router.graph_changes(), 0);

    let (fake, dead_a, dead_b) = steady_graph(Some(false));
    fake.fail(GraphOp::UnloadBranch);
    let mut router = router_on(&fake);
    let result = router.route_for_targets(&steady_selection());
    assert!(result.is_ok(), "route failed: {result:?}");
    assert_eq!(
        fake.calls(),
        vec![
            unload(dead_a),
            unload(dead_b),
            load(SINK_A, 0),
            load(SINK_B, 30)
        ]
    );
    assert_eq!(router.graph_changes(), 2, "the two loads, not the unloads");
}

// Criterion (#146): `AudioError::Expired` displays as
// `the audio graph did not start the command in time`. The whole line is
// compared: it carries no `PipeWire error:` prefix, which a variant folded
// into the `PipeWire` arm would add.
#[test]
fn test_audio_error_expired_displays_as_the_audio_graph_did_not_start_the_command_in_time() {
    assert_eq!(
        AudioError::Expired.to_string(),
        "the audio graph did not start the command in time"
    );
}
