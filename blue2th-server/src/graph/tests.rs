// SPDX-License-Identifier: MIT OR Apache-2.0

use super::fake::{FakeGraph, GraphCall, GraphOp};
use super::{Graph, LoadedBranch};
use crate::audio::{AudioError, CombineBranch};

const COMBINED: &str = "blue2th_combined";
const SPEAKER: &str = "bluez_output.AA_BB_CC_DD_EE_01.1";

// Criterion: `FakeGraph` rejects an empty node name with an error, so a test
// catches a router that lets one reach the trait.
#[test]
fn test_fake_graph_refuses_an_empty_node_name() {
    let mut fake = FakeGraph::with_sinks(&[COMBINED, SPEAKER]);

    assert!(matches!(fake.branches(""), Err(AudioError::PipeWire(_))));
    assert!(matches!(
        fake.create_combined_sink(""),
        Err(AudioError::PipeWire(_))
    ));
    assert!(matches!(
        fake.load_branch(COMBINED, "", 50),
        Err(AudioError::PipeWire(_))
    ));
    assert!(matches!(
        fake.load_branch("", SPEAKER, 50),
        Err(AudioError::PipeWire(_))
    ));
    assert!(matches!(fake.teardown(""), Err(AudioError::PipeWire(_))));
    assert!(matches!(
        fake.clear_stale_default_sink(""),
        Err(AudioError::PipeWire(_))
    ));
    assert!(matches!(
        fake.set_sink_volume("", 0.5),
        Err(AudioError::PipeWire(_))
    ));
    assert!(matches!(fake.sink_volume(""), Err(AudioError::PipeWire(_))));

    assert_eq!(fake.empty_names_refused(), 8);
    // Nothing was changed by the refused calls.
    assert_eq!(fake.sink_names(), vec![COMBINED, SPEAKER]);
    assert!(fake.loaded(COMBINED).is_empty());
}

// Criterion (#148): the fake's `sink_volume` honours `fail_for` — an `Err`
// for the sink named, and only for it — and `fail` — an `Err` for every
// sink — while a listed sink with no level set is `Ok(None)`, not an
// `Err`. The near miss of `fail_for` is `OTHER`, readable beside the
// failing sink; the near miss of "no level is not a failure" is `SILENT`,
// listed but never given a level.
#[test]
fn test_fake_graph_sink_volume_fails_when_told_and_reads_no_level_as_none() {
    const OTHER: &str = "bluez_output.AA_BB_CC_DD_EE_02.1";
    const SILENT: &str = "bluez_output.AA_BB_CC_DD_EE_03.1";
    let mut fake = FakeGraph::with_sinks(&[SPEAKER, OTHER, SILENT]);
    fake.set_volume(SPEAKER, 0.5);
    fake.set_volume(OTHER, 0.25);

    fake.fail_for(GraphOp::SinkVolume, SPEAKER);
    let failed = fake.sink_volume(SPEAKER);
    assert!(
        matches!(&failed, Err(AudioError::PipeWire(m)) if m.contains("SinkVolume told to fail")),
        "fail_for makes the read an Err, got {failed:?}"
    );
    assert_eq!(
        fake.sink_volume(OTHER).ok(),
        Some(Some(0.25)),
        "fail_for fails only the sink it names"
    );
    assert_eq!(
        fake.sink_volume(SILENT).ok(),
        Some(None),
        "a listed sink with no level is Ok(None)"
    );

    fake.clear_failures();
    fake.fail(GraphOp::SinkVolume);
    let failed = fake.sink_volume(OTHER);
    assert!(
        matches!(&failed, Err(AudioError::PipeWire(_))),
        "fail makes every read an Err, got {failed:?}"
    );

    fake.clear_failures();
    assert_eq!(fake.sink_volume(SPEAKER).ok(), Some(Some(0.5)));
    assert_eq!(fake.empty_names_refused(), 0);
    assert_eq!(
        fake.all_calls().len(),
        5,
        "every read is recorded, the failed ones included"
    );
}

// Criterion (#147, 2026-10-03): `FakeGraph` can be told to fail a call
// with `AudioError::Unanswered` rather than its usual
// `PipeWire("… told to fail")`, and the attempt is recorded like any
// other. Every call to the op answers so, whatever sink it names, and a
// failed set changes nothing in the fake, as every failure rule does.
// Near misses: `Teardown` told to `fail` beside it, which must still
// answer `PipeWire` (the two rules do not merge), and `SinkVolume`, an op
// told nothing, which must still answer.
#[test]
fn test_fake_graph_fails_a_call_unanswered_when_told_and_records_the_attempt() {
    const OTHER: &str = "bluez_output.AA_BB_CC_DD_EE_02.1";
    let mut fake = FakeGraph::with_sinks(&[COMBINED, SPEAKER, OTHER]);
    fake.set_volume(SPEAKER, 0.25);
    fake.fail_unanswered(GraphOp::SetSinkVolume);
    fake.fail(GraphOp::Teardown);

    let first = fake.set_sink_volume(SPEAKER, 0.5);
    let second = fake.set_sink_volume(OTHER, 0.75);
    let refused = fake.teardown(COMBINED);

    assert!(
        matches!(first, Err(AudioError::Unanswered)),
        "got {first:?}"
    );
    assert!(
        matches!(second, Err(AudioError::Unanswered)),
        "every call to the op, whatever it names: got {second:?}"
    );
    assert!(
        matches!(&refused, Err(AudioError::PipeWire(m)) if m.contains("Teardown told to fail")),
        "`fail` keeps its own answer beside it: got {refused:?}"
    );
    assert_eq!(
        fake.sink_volume(SPEAKER).ok(),
        Some(Some(0.25)),
        "an op told nothing answers, and the unanswered set wrote nothing"
    );
    assert_eq!(
        fake.calls(),
        vec![
            GraphCall::SetSinkVolume {
                sink: SPEAKER.to_string(),
                level: 0.5
            },
            GraphCall::SetSinkVolume {
                sink: OTHER.to_string(),
                level: 0.75
            },
            GraphCall::Teardown {
                sink_name: COMBINED.to_string()
            },
        ],
        "the unanswered attempts are recorded like any other"
    );
}

// Criterion: `FakeGraph` state stays coherent across calls — a load shows up
// in the next `branches()`, an unload removes it.
#[test]
fn test_fake_graph_lists_a_loaded_branch_until_it_is_unloaded() {
    let mut fake = FakeGraph::with_sinks(&[SPEAKER]);
    fake.create_combined_sink(COMBINED).unwrap();
    fake.load_branch(COMBINED, SPEAKER, 70).unwrap();

    let loaded = fake.branches(COMBINED).unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].branch.sink, SPEAKER);
    assert_eq!(loaded[0].branch.latency_ms, 70);
    assert_eq!(loaded[0].live, Some(true));

    fake.unload_branch(loaded[0].id).unwrap();
    assert!(fake.branches(COMBINED).unwrap().is_empty());
}

// Criterion: `create_combined_sink` makes the sink appear in `sinks()`, and
// `teardown` removes the sink and its branches.
#[test]
fn test_fake_graph_teardown_removes_the_sink_and_its_branches() {
    let mut fake = FakeGraph::with_sinks(&[SPEAKER]);
    fake.create_combined_sink(COMBINED).unwrap();
    assert_eq!(fake.sinks().unwrap(), vec![SPEAKER, COMBINED]);
    fake.load_branch(COMBINED, SPEAKER, 50).unwrap();

    fake.teardown(COMBINED).unwrap();

    assert_eq!(fake.sinks().unwrap(), vec![SPEAKER]);
    assert!(fake.branches(COMBINED).unwrap().is_empty());
}

// Criterion: `FakeGraph` records every mutating call in order, and keeps the
// reads out of that list.
#[test]
fn test_fake_graph_records_mutating_calls_in_order() {
    let mut fake = FakeGraph::with_sinks(&[SPEAKER]);
    fake.create_combined_sink(COMBINED).unwrap();
    fake.sinks().unwrap();
    fake.load_branch(COMBINED, SPEAKER, 50).unwrap();
    fake.branches(COMBINED).unwrap();
    fake.set_sink_volume(SPEAKER, 0.5).unwrap();

    assert_eq!(
        fake.calls(),
        vec![
            GraphCall::CreateCombinedSink {
                sink_name: COMBINED.to_string()
            },
            GraphCall::LoadBranch {
                sink_name: COMBINED.to_string(),
                real_sink: SPEAKER.to_string(),
                latency_ms: 50
            },
            GraphCall::SetSinkVolume {
                sink: SPEAKER.to_string(),
                level: 0.5
            },
        ]
    );
    assert_eq!(fake.all_calls().len(), 5);
}

// Criterion: `FakeGraph` can be told to fail a given call; the attempt is
// recorded and the state is left as it was.
#[test]
fn test_fake_graph_fails_the_call_it_was_told_to_fail() {
    let other = "bluez_output.AA_BB_CC_DD_EE_02.1";
    let mut fake = FakeGraph::with_sinks(&[COMBINED, SPEAKER, other]);
    fake.fail_for(GraphOp::LoadBranch, SPEAKER);

    assert!(fake.load_branch(COMBINED, SPEAKER, 50).is_err());
    assert!(fake.load_branch(COMBINED, other, 50).is_ok());

    assert_eq!(fake.calls().len(), 2);
    let loaded = fake.loaded(COMBINED);
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].branch.sink, other);
}

// Criterion (#147, 2026-10-03): `FakeGraph` can be told to fail with
// `AudioError::Unanswered` the calls to an op that name one node, and
// the attempt is recorded like any other. The near miss is the same op
// naming another node, which must still load: a rule ignoring the node
// would stall it too, and a route could not stall after a load.
#[test]
fn test_fake_graph_fails_unanswered_only_the_call_naming_its_node() {
    let other = "bluez_output.AA_BB_CC_DD_EE_02.1";
    let mut fake = FakeGraph::with_sinks(&[COMBINED, SPEAKER, other]);
    fake.fail_unanswered_for(GraphOp::LoadBranch, SPEAKER);

    let stalled = fake.load_branch(COMBINED, SPEAKER, 50);
    let answered = fake.load_branch(COMBINED, other, 70);

    assert_eq!(stalled, Err(AudioError::Unanswered));
    assert_eq!(answered, Ok(()));
    assert_eq!(
        fake.calls(),
        vec![
            GraphCall::LoadBranch {
                sink_name: COMBINED.to_string(),
                real_sink: SPEAKER.to_string(),
                latency_ms: 50
            },
            GraphCall::LoadBranch {
                sink_name: COMBINED.to_string(),
                real_sink: other.to_string(),
                latency_ms: 70
            },
        ]
    );
    let sinks: Vec<String> = fake
        .loaded(COMBINED)
        .into_iter()
        .map(|l| l.branch.sink)
        .collect();
    assert_eq!(sinks, vec![other.to_string()]);
}

// Criterion: `FakeGraph` can report "cannot tell" — `sinks()` errs — from a
// chosen read onwards.
#[test]
fn test_fake_graph_reports_cannot_tell_once_told_to() {
    let mut fake = FakeGraph::with_sinks(&[SPEAKER]);
    fake.fail_after(GraphOp::Sinks, 1);

    assert_eq!(fake.sinks().unwrap(), vec![SPEAKER]);
    assert!(matches!(fake.sinks(), Err(AudioError::PipeWire(_))));
    assert!(matches!(fake.sinks(), Err(AudioError::PipeWire(_))));
}

// Criterion (#152, test double): `FakeGraph` can report a stall —
// `sinks()` answers `Unanswered` — from a chosen read onwards, the reads
// before it answering the list as it is.
#[test]
fn test_fake_graph_reports_unanswered_from_a_chosen_read_onwards() {
    let mut fake = FakeGraph::with_sinks(&[SPEAKER]);
    fake.fail_unanswered_after(GraphOp::Sinks, 1);

    assert_eq!(fake.sinks(), Ok(vec![SPEAKER.to_string()]));
    assert_eq!(fake.sinks(), Err(AudioError::Unanswered));
    assert_eq!(fake.sinks(), Err(AudioError::Unanswered));
}

// Criterion: `FakeGraph` records `set_branch_delay` and updates the stored
// latency of that branch only, in place: same id, the other branch
// untouched. `LoadedBranch.branch.latency_ms` is the delay last applied.
#[test]
fn test_fake_graph_set_branch_delay_retunes_that_branch_in_place() {
    let other = "bluez_output.AA_BB_CC_DD_EE_02.1";
    let mut fake = FakeGraph::with_sinks(&[COMBINED, SPEAKER, other]);
    let a = fake.seed_branch(COMBINED, SPEAKER, 0, Some(true));
    let b = fake.seed_branch(COMBINED, other, 30, Some(true));

    assert!(fake.set_branch_delay(a, 120).is_ok());

    assert_eq!(
        fake.calls(),
        vec![GraphCall::SetBranchDelay {
            id: a,
            delay_ms: 120
        }]
    );
    let loaded = fake.loaded(COMBINED);
    let delays: Vec<(u32, &str, u32)> = loaded
        .iter()
        .map(|l| (l.id, l.branch.sink.as_str(), l.branch.latency_ms))
        .collect();
    assert_eq!(delays, vec![(a, SPEAKER, 120), (b, other, 30)]);
}

// Criterion (non-nominal): `set_branch_delay` on an id no branch carries is
// an `Err`, recorded like any other attempt, and changes nothing.
#[test]
fn test_fake_graph_set_branch_delay_on_an_unknown_id_errs() {
    let mut fake = FakeGraph::with_sinks(&[COMBINED, SPEAKER]);
    let a = fake.seed_branch(COMBINED, SPEAKER, 0, Some(true));

    assert!(matches!(
        fake.set_branch_delay(a + 100, 120),
        Err(AudioError::PipeWire(_))
    ));

    assert_eq!(
        fake.calls(),
        vec![GraphCall::SetBranchDelay {
            id: a + 100,
            delay_ms: 120
        }]
    );
    let loaded = fake.loaded(COMBINED);
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].branch.latency_ms, 0);
}

// Criterion (non-nominal): a delay the node rejected is not the delay the
// branch runs at, so a failed `set_branch_delay` leaves the stored latency
// as it was — that is what lets the next reconcile see the mismatch.
#[test]
fn test_fake_graph_a_failed_set_branch_delay_leaves_the_latency() {
    let mut fake = FakeGraph::with_sinks(&[COMBINED, SPEAKER]);
    let a = fake.seed_branch(COMBINED, SPEAKER, 0, Some(true));
    fake.fail(GraphOp::SetBranchDelay);

    assert!(matches!(
        fake.set_branch_delay(a, 120),
        Err(AudioError::PipeWire(_))
    ));
    assert_eq!(fake.loaded(COMBINED)[0].branch.latency_ms, 0);

    fake.clear_failures();
    assert!(fake.set_branch_delay(a, 120).is_ok());
    assert_eq!(fake.loaded(COMBINED)[0].branch.latency_ms, 120);
}

/// `default.configured.audio.sink` as `pw-metadata -n default 0` showed it
/// on the dev PC on 2026-09-24 and 2026-09-26, after earlier versions of
/// blue2th had written it (`type:'Spa:String:JSON'`).
const STALE_DEFAULT: &str = r#"{"name":"blue2th_combined"}"#;

// Criterion: `FakeGraph` models a configured default — seeded, read back,
// and cleared by `clear_stale_default_sink` when it names the sink exactly,
// which answers `true` and records the call. A second clear finds nothing.
#[test]
fn test_fake_graph_clears_a_configured_default_naming_the_sink() {
    let mut fake = FakeGraph::with_sinks(&[SPEAKER]);
    fake.set_configured_default(Some(STALE_DEFAULT));
    assert_eq!(fake.configured_default().as_deref(), Some(STALE_DEFAULT));

    assert_eq!(fake.clear_stale_default_sink(COMBINED).ok(), Some(true));
    assert_eq!(fake.configured_default(), None);

    assert_eq!(fake.clear_stale_default_sink(COMBINED).ok(), Some(false));
    assert_eq!(
        fake.calls(),
        vec![
            GraphCall::ClearStaleDefaultSink {
                sink_name: COMBINED.to_string()
            },
            GraphCall::ClearStaleDefaultSink {
                sink_name: COMBINED.to_string()
            },
        ]
    );
}

// Criterion (guard, exact name): a configured default naming another sink —
// including one whose name merely starts with the combined sink's — is left
// exactly as it is, and the clear answers `false`.
#[test]
fn test_fake_graph_keeps_a_configured_default_naming_another_sink() {
    for other in [
        r#"{"name":"blue2th_combined_old"}"#,
        // `default.audio.sink` on the dev PC, 2026-09-26: a real sink value.
        r#"{"name":"bluez_output.80_99_E7_63_50_29.1"}"#,
    ] {
        let mut fake = FakeGraph::with_sinks(&[SPEAKER]);
        fake.set_configured_default(Some(other));

        assert_eq!(fake.clear_stale_default_sink(COMBINED).ok(), Some(false));
        assert_eq!(fake.configured_default().as_deref(), Some(other));
    }
    // The near-miss's twin, which the same fake does clear.
    let mut fake = FakeGraph::with_sinks(&[SPEAKER]);
    fake.set_configured_default(Some(STALE_DEFAULT));
    assert_eq!(fake.clear_stale_default_sink(COMBINED).ok(), Some(true));
}

// Criterion (non-nominal): a clear the graph refuses is an `Err`, and the
// configured default is left as it was.
#[test]
fn test_fake_graph_a_failed_clear_leaves_the_configured_default() {
    let mut fake = FakeGraph::with_sinks(&[SPEAKER]);
    fake.set_configured_default(Some(STALE_DEFAULT));
    fake.fail(GraphOp::ClearStaleDefaultSink);

    assert!(matches!(
        fake.clear_stale_default_sink(COMBINED),
        Err(AudioError::PipeWire(_))
    ));
    assert_eq!(fake.configured_default().as_deref(), Some(STALE_DEFAULT));
}

// Criterion (#139): the fake records `retarget_streams` as a mutating
// call naming the sink, answers `Ok(0)` — no stream is modelled, and
// re-targeting nothing is a success — fails it when told to, and refuses
// an empty sink name. `routing_calls` leaves only that call out.
#[test]
fn test_fake_graph_records_retarget_streams_and_fails_it_when_told() {
    let mut fake = FakeGraph::with_sinks(&[SPEAKER]);
    fake.create_combined_sink(COMBINED).unwrap();

    assert_eq!(fake.retarget_streams(COMBINED).ok(), Some(0));
    assert_eq!(
        fake.calls(),
        vec![
            GraphCall::CreateCombinedSink {
                sink_name: COMBINED.to_string()
            },
            GraphCall::RetargetStreams {
                sink_name: COMBINED.to_string()
            },
        ]
    );
    assert_eq!(
        fake.routing_calls(),
        vec![GraphCall::CreateCombinedSink {
            sink_name: COMBINED.to_string()
        }]
    );

    fake.fail(GraphOp::RetargetStreams);
    assert!(matches!(
        fake.retarget_streams(COMBINED),
        Err(AudioError::PipeWire(_))
    ));
    fake.clear_failures();
    assert!(matches!(
        fake.retarget_streams(""),
        Err(AudioError::PipeWire(_))
    ));
    assert_eq!(fake.empty_names_refused(), 1);
}

// Criterion (#147): the fake records every deadline it is handed, in
// order, apart from the call log — a deadline is not a call the daemon
// sees, so `all_calls` does not show it and `clear_calls` keeps it.
#[test]
fn test_fake_graph_records_the_deadlines_it_is_handed_outside_the_call_log() {
    let mut fake = FakeGraph::with_sinks(&[SPEAKER]);
    let base = std::time::Instant::now();
    let first = base + std::time::Duration::from_millis(1600);
    let second = base + std::time::Duration::from_millis(4100);

    fake.set_deadline(first);
    fake.sinks().unwrap();
    fake.set_deadline(second);

    assert_eq!(fake.deadlines(), vec![first, second]);
    assert_eq!(fake.all_calls(), vec![GraphCall::Sinks]);
    fake.clear_calls();
    assert_eq!(fake.deadlines(), vec![first, second]);
}

// Criterion (#147): the fake runs a hook from inside the next read of the
// sink list, once — the read after it runs none — and before that read
// is recorded or answered.
#[test]
fn test_fake_graph_runs_a_hook_once_from_inside_the_next_sinks_read() {
    let mut fake = FakeGraph::with_sinks(&[SPEAKER]);
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let record = std::sync::Arc::clone(&seen);
    // A clone of the fake is a handle onto the same state.
    let inspected = fake.clone();
    fake.run_on_next_sinks_read(move || {
        record.lock().unwrap().push(inspected.all_calls().len());
    });
    fake.branches(COMBINED).unwrap();
    assert!(seen.lock().unwrap().is_empty(), "only a sinks read runs it");

    fake.sinks().unwrap();
    fake.sinks().unwrap();

    assert_eq!(
        *seen.lock().unwrap(),
        vec![1],
        "run once, before its own read was recorded"
    );
    assert_eq!(fake.all_calls().len(), 3);
}

// Criterion: liveness is reported as `Some(true)`, `Some(false)` or `None`.
#[test]
fn test_fake_graph_reports_the_liveness_it_was_given() {
    let mut fake = FakeGraph::with_sinks(&[COMBINED, SPEAKER]);
    let dead = fake.seed_branch(COMBINED, SPEAKER, 50, Some(false));
    fake.set_new_branch_liveness(None);
    fake.load_branch(COMBINED, SPEAKER, 60).unwrap();

    let loaded = fake.branches(COMBINED).unwrap();
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0].id, dead);
    assert_eq!(loaded[0].live, Some(false));
    assert_eq!(loaded[1].live, None);
}

// --- NamedGuard (#154) ---
//
// The trap every refusal test below avoids: the fake refuses an empty name
// too, with an `Err` of the same variant. So each one asserts the guard's
// own message, an empty call log (the fake logs a call before refusing
// it) and `empty_names_refused() == 0` — any of the three alone would
// fail with the guard's check deleted.

/// The smallest non-empty names: the near miss of "empty". Distinct, so a
/// swapped sink and target fail.
const ONE_CHAR_SINK: &str = "x";
const ONE_CHAR_TARGET: &str = "y";
const SINK_REFUSED: &str = "empty sink name refused";
const TARGET_REFUSED: &str = "empty target sink name refused";

/// A guard over `fake`. The clone is a handle onto the same state, so the
/// test keeps reading the fake after the guard owns its copy.
fn guarded(fake: &FakeGraph) -> super::NamedGuard<FakeGraph> {
    super::NamedGuard::new(fake.clone())
}

/// `answer` is the guard's refusal `expected`, and the wrapped fake was
/// never reached.
fn assert_refused_by_the_guard<T: std::fmt::Debug>(
    answer: Result<T, AudioError>,
    fake: &FakeGraph,
    expected: &str,
) {
    assert!(
        matches!(&answer, Err(AudioError::PipeWire(m)) if m == expected),
        "expected the guard's {expected:?}, got {answer:?}"
    );
    assert_eq!(
        fake.all_calls(),
        Vec::<GraphCall>::new(),
        "the wrapped graph was called"
    );
    assert_eq!(
        fake.empty_names_refused(),
        0,
        "the fake refused the empty name, not the guard"
    );
}

// Criterion: `branches("")` through `NamedGuard<FakeGraph>` is the guard's
// "empty sink name refused", the fake never called.
#[test]
fn test_named_guard_refuses_an_empty_sink_in_branches() {
    let fake = FakeGraph::with_sinks(&[COMBINED, SPEAKER]);
    let mut guard = guarded(&fake);
    assert_refused_by_the_guard(guard.branches(""), &fake, SINK_REFUSED);
}

// Criterion: `create_combined_sink("")` is refused by the guard.
#[test]
fn test_named_guard_refuses_an_empty_sink_in_create_combined_sink() {
    let fake = FakeGraph::with_sinks(&[COMBINED, SPEAKER]);
    let mut guard = guarded(&fake);
    assert_refused_by_the_guard(guard.create_combined_sink(""), &fake, SINK_REFUSED);
}

// Criterion: `load_branch` with an empty sink and a **valid** target is
// refused by the guard's sink check — only that check can refuse it.
#[test]
fn test_named_guard_refuses_an_empty_sink_in_load_branch() {
    let fake = FakeGraph::with_sinks(&[COMBINED, SPEAKER]);
    let mut guard = guarded(&fake);
    assert_refused_by_the_guard(guard.load_branch("", SPEAKER, 37), &fake, SINK_REFUSED);
}

// Criterion: `teardown("")` is refused by the guard.
#[test]
fn test_named_guard_refuses_an_empty_sink_in_teardown() {
    let fake = FakeGraph::with_sinks(&[COMBINED, SPEAKER]);
    let mut guard = guarded(&fake);
    assert_refused_by_the_guard(guard.teardown(""), &fake, SINK_REFUSED);
}

// Criterion: `clear_stale_default_sink("")` is refused by the guard.
#[test]
fn test_named_guard_refuses_an_empty_sink_in_clear_stale_default_sink() {
    let fake = FakeGraph::with_sinks(&[COMBINED, SPEAKER]);
    let mut guard = guarded(&fake);
    assert_refused_by_the_guard(guard.clear_stale_default_sink(""), &fake, SINK_REFUSED);
}

// Criterion: `retarget_streams("")` is refused by the guard.
#[test]
fn test_named_guard_refuses_an_empty_sink_in_retarget_streams() {
    let fake = FakeGraph::with_sinks(&[COMBINED, SPEAKER]);
    let mut guard = guarded(&fake);
    assert_refused_by_the_guard(guard.retarget_streams(""), &fake, SINK_REFUSED);
}

// Criterion: `sink_volume("")` is refused by the guard.
#[test]
fn test_named_guard_refuses_an_empty_sink_in_sink_volume() {
    let fake = FakeGraph::with_sinks(&[COMBINED, SPEAKER]);
    let mut guard = guarded(&fake);
    assert_refused_by_the_guard(guard.sink_volume(""), &fake, SINK_REFUSED);
}

// Criterion: `set_sink_volume("", level)` is refused by the guard.
#[test]
fn test_named_guard_refuses_an_empty_sink_in_set_sink_volume() {
    let fake = FakeGraph::with_sinks(&[COMBINED, SPEAKER]);
    let mut guard = guarded(&fake);
    assert_refused_by_the_guard(guard.set_sink_volume("", 0.375), &fake, SINK_REFUSED);
}

// Criterion: `load_branch` with a valid sink and an empty target is the
// guard's "empty target sink name refused". The sink is valid, so only
// the target check can refuse it.
#[test]
fn test_named_guard_refuses_an_empty_target_sink_in_load_branch() {
    let fake = FakeGraph::with_sinks(&[COMBINED, SPEAKER]);
    let mut guard = guarded(&fake);
    assert_refused_by_the_guard(guard.load_branch(COMBINED, "", 37), &fake, TARGET_REFUSED);
}

// Criterion: with both `load_branch` names empty, the sink is checked
// first, so the refusal names the sink. A guard checking the target first
// answers "empty target sink name refused" and fails this.
#[test]
fn test_named_guard_names_the_sink_first_when_both_load_branch_names_are_empty() {
    let fake = FakeGraph::with_sinks(&[COMBINED, SPEAKER]);
    let mut guard = guarded(&fake);
    assert_refused_by_the_guard(guard.load_branch("", "", 37), &fake, SINK_REFUSED);
}

// Criterion (near miss): `branches` with a one-character name reaches the
// fake once, with the name unchanged, and the fake's answer comes back.
#[test]
fn test_named_guard_forwards_branches_with_a_one_character_name() {
    let fake = FakeGraph::with_sinks(&[ONE_CHAR_SINK, ONE_CHAR_TARGET]);
    let id = fake.seed_branch(ONE_CHAR_SINK, ONE_CHAR_TARGET, 37, Some(true));
    let mut guard = guarded(&fake);

    let answer = guard.branches(ONE_CHAR_SINK);

    assert_eq!(
        answer,
        Ok(vec![LoadedBranch {
            id,
            branch: CombineBranch {
                sink: ONE_CHAR_TARGET.to_string(),
                latency_ms: 37,
            },
            live: Some(true),
        }])
    );
    assert_eq!(
        fake.all_calls(),
        vec![GraphCall::Branches {
            sink_name: ONE_CHAR_SINK.to_string()
        }]
    );
}

// Criterion (near miss): `create_combined_sink` with a one-character name
// reaches the fake once, unchanged, and creates the sink there.
#[test]
fn test_named_guard_forwards_create_combined_sink_with_a_one_character_name() {
    let fake = FakeGraph::new();
    let mut guard = guarded(&fake);

    assert_eq!(guard.create_combined_sink(ONE_CHAR_SINK), Ok(()));
    assert_eq!(
        fake.all_calls(),
        vec![GraphCall::CreateCombinedSink {
            sink_name: ONE_CHAR_SINK.to_string()
        }]
    );
    assert_eq!(fake.sink_names(), vec![ONE_CHAR_SINK.to_string()]);
}

// Criterion (near miss): `load_branch` with one-character names reaches
// the fake once with sink, target and `latency_ms` unchanged — distinct
// values, so a swap fails.
#[test]
fn test_named_guard_forwards_load_branch_with_one_character_names() {
    let fake = FakeGraph::with_sinks(&[ONE_CHAR_SINK, ONE_CHAR_TARGET]);
    let mut guard = guarded(&fake);

    assert_eq!(
        guard.load_branch(ONE_CHAR_SINK, ONE_CHAR_TARGET, 37),
        Ok(())
    );
    assert_eq!(
        fake.all_calls(),
        vec![GraphCall::LoadBranch {
            sink_name: ONE_CHAR_SINK.to_string(),
            real_sink: ONE_CHAR_TARGET.to_string(),
            latency_ms: 37,
        }]
    );
    let loaded = fake.loaded(ONE_CHAR_SINK);
    assert_eq!(loaded.len(), 1, "one branch loaded, got {loaded:?}");
    assert_eq!(
        loaded.first().map(|b| b.branch.clone()),
        Some(CombineBranch {
            sink: ONE_CHAR_TARGET.to_string(),
            latency_ms: 37,
        })
    );
}

// Criterion (near miss): `teardown` with a one-character name reaches the
// fake once, unchanged, and removes that sink only.
#[test]
fn test_named_guard_forwards_teardown_with_a_one_character_name() {
    let fake = FakeGraph::with_sinks(&[ONE_CHAR_SINK, ONE_CHAR_TARGET]);
    let mut guard = guarded(&fake);

    assert_eq!(guard.teardown(ONE_CHAR_SINK), Ok(()));
    assert_eq!(
        fake.all_calls(),
        vec![GraphCall::Teardown {
            sink_name: ONE_CHAR_SINK.to_string()
        }]
    );
    assert_eq!(fake.sink_names(), vec![ONE_CHAR_TARGET.to_string()]);
}

// Criterion (near miss): `clear_stale_default_sink` with a one-character
// name reaches the fake once, unchanged, and its `true` comes back — a
// guard answering a constant `false` fails this.
#[test]
fn test_named_guard_forwards_clear_stale_default_sink_with_a_one_character_name() {
    let fake = FakeGraph::with_sinks(&[ONE_CHAR_SINK]);
    fake.set_configured_default(Some(r#"{"name":"x"}"#));
    let mut guard = guarded(&fake);

    assert_eq!(guard.clear_stale_default_sink(ONE_CHAR_SINK), Ok(true));
    assert_eq!(
        fake.all_calls(),
        vec![GraphCall::ClearStaleDefaultSink {
            sink_name: ONE_CHAR_SINK.to_string()
        }]
    );
    assert_eq!(fake.configured_default(), None);
}

// Criterion (near miss): `retarget_streams` with a one-character name
// reaches the fake once, unchanged, and its count comes back. The count is
// not `0`, the fake's default: a guard answering a constant `Ok(0)` fails.
#[test]
fn test_named_guard_forwards_retarget_streams_with_a_one_character_name() {
    let fake = FakeGraph::with_sinks(&[ONE_CHAR_SINK]);
    fake.set_streams_to_retarget(3);
    let mut guard = guarded(&fake);

    assert_eq!(guard.retarget_streams(ONE_CHAR_SINK), Ok(3));
    assert_eq!(
        fake.all_calls(),
        vec![GraphCall::RetargetStreams {
            sink_name: ONE_CHAR_SINK.to_string()
        }]
    );
}

// Criterion (near miss): `sink_volume` with a one-character name reaches
// the fake once, unchanged, and the fake's level comes back.
#[test]
fn test_named_guard_forwards_sink_volume_with_a_one_character_name() {
    let fake = FakeGraph::with_sinks(&[ONE_CHAR_SINK]);
    fake.set_volume(ONE_CHAR_SINK, 0.625);
    let mut guard = guarded(&fake);

    assert_eq!(guard.sink_volume(ONE_CHAR_SINK), Ok(Some(0.625)));
    assert_eq!(
        fake.all_calls(),
        vec![GraphCall::SinkVolume {
            sink: ONE_CHAR_SINK.to_string()
        }]
    );
}

// Criterion (near miss): `set_sink_volume` with a one-character name
// reaches the fake once with the sink and a non-default `level`
// unchanged.
#[test]
fn test_named_guard_forwards_set_sink_volume_with_a_one_character_name() {
    let fake = FakeGraph::with_sinks(&[ONE_CHAR_SINK]);
    let mut guard = guarded(&fake);

    assert_eq!(guard.set_sink_volume(ONE_CHAR_SINK, 0.375), Ok(()));
    assert_eq!(
        fake.all_calls(),
        vec![GraphCall::SetSinkVolume {
            sink: ONE_CHAR_SINK.to_string(),
            level: 0.375,
        }]
    );
}

// Criterion: `set_deadline` takes no name and reaches the fake with its
// instant unchanged.
#[test]
fn test_named_guard_passes_set_deadline_through() {
    let fake = FakeGraph::new();
    let mut guard = guarded(&fake);
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(2300);

    guard.set_deadline(deadline);

    assert_eq!(fake.deadlines(), vec![deadline]);
}

// Criterion: `sinks` takes no name and reaches the fake once; its list
// comes back unchanged.
#[test]
fn test_named_guard_passes_sinks_through() {
    let fake = FakeGraph::with_sinks(&[COMBINED, SPEAKER]);
    let mut guard = guarded(&fake);

    assert_eq!(
        guard.sinks(),
        Ok(vec![COMBINED.to_string(), SPEAKER.to_string()])
    );
    assert_eq!(fake.all_calls(), vec![GraphCall::Sinks]);
}

// Criterion (guard, nothing checked): `unload_branch` takes no name and
// reaches the fake with its id unchanged — id `0` included, so a guard
// that wrongly treats `0` as "empty" fails this.
#[test]
fn test_named_guard_passes_unload_branch_through_unchecked() {
    let fake = FakeGraph::with_sinks(&[COMBINED, SPEAKER]);
    let mut guard = guarded(&fake);

    assert_eq!(guard.unload_branch(0), Ok(()));
    assert_eq!(fake.all_calls(), vec![GraphCall::UnloadBranch { id: 0 }]);
}

// Criterion: `set_branch_delay` takes no name and reaches the fake with
// id and delay unchanged — distinct values, so a swap fails — and
// retunes the branch there.
#[test]
fn test_named_guard_passes_set_branch_delay_through() {
    let fake = FakeGraph::with_sinks(&[COMBINED, SPEAKER]);
    let id = fake.seed_branch(COMBINED, SPEAKER, 50, Some(true));
    let mut guard = guarded(&fake);

    assert_eq!(guard.set_branch_delay(id, 73), Ok(()));
    assert_eq!(
        fake.all_calls(),
        vec![GraphCall::SetBranchDelay { id, delay_ms: 73 }]
    );
    assert_eq!(
        fake.loaded(COMBINED).first().map(|b| b.branch.latency_ms),
        Some(73)
    );
}

// Criterion (guard, nothing checked): `set_branch_delay(0, 0)` is not
// refused by the guard — it reaches the fake, whose own answer (no branch
// carries id 0) comes back unchanged.
#[test]
fn test_named_guard_passes_set_branch_delay_with_zeroes_through_unchecked() {
    let fake = FakeGraph::with_sinks(&[COMBINED, SPEAKER]);
    let mut guard = guarded(&fake);

    assert_eq!(
        guard.set_branch_delay(0, 0),
        Err(AudioError::PipeWire("fake graph: no branch 0".to_string()))
    );
    assert_eq!(
        fake.all_calls(),
        vec![GraphCall::SetBranchDelay { id: 0, delay_ms: 0 }]
    );
}

// Criterion: an `Err` from the wrapped graph on a call that passed the
// guard comes back unchanged — variant and text — on every method that
// can fail, the name-taking ones and the others alike.
#[test]
fn test_named_guard_returns_the_wrapped_graphs_error_unchanged() {
    let fake = FakeGraph::with_sinks(&[ONE_CHAR_SINK, ONE_CHAR_TARGET]);
    for op in [
        GraphOp::Branches,
        GraphOp::CreateCombinedSink,
        GraphOp::Teardown,
        GraphOp::ClearStaleDefaultSink,
        GraphOp::RetargetStreams,
        GraphOp::SinkVolume,
        GraphOp::SetSinkVolume,
    ] {
        fake.fail_for(op, ONE_CHAR_SINK);
    }
    // `load_branch` failure rules match on the target sink.
    fake.fail_for(GraphOp::LoadBranch, ONE_CHAR_TARGET);
    fake.fail(GraphOp::Sinks);
    fake.fail(GraphOp::UnloadBranch);
    fake.fail(GraphOp::SetBranchDelay);
    let mut guard = guarded(&fake);
    let told = |what: &str| AudioError::PipeWire(format!("fake graph: {what}"));

    assert_eq!(guard.sinks(), Err(told("Sinks told to fail for ")));
    assert_eq!(
        guard.branches(ONE_CHAR_SINK),
        Err(told("Branches told to fail for x"))
    );
    assert_eq!(
        guard.create_combined_sink(ONE_CHAR_SINK),
        Err(told("CreateCombinedSink told to fail for x"))
    );
    assert_eq!(
        guard.load_branch(ONE_CHAR_SINK, ONE_CHAR_TARGET, 37),
        Err(told("LoadBranch told to fail for y"))
    );
    assert_eq!(
        guard.unload_branch(4),
        Err(told("UnloadBranch told to fail for 4"))
    );
    assert_eq!(
        guard.set_branch_delay(4, 73),
        Err(told("SetBranchDelay told to fail for 4"))
    );
    assert_eq!(
        guard.teardown(ONE_CHAR_SINK),
        Err(told("Teardown told to fail for x"))
    );
    assert_eq!(
        guard.clear_stale_default_sink(ONE_CHAR_SINK),
        Err(told("ClearStaleDefaultSink told to fail for x"))
    );
    assert_eq!(
        guard.retarget_streams(ONE_CHAR_SINK),
        Err(told("RetargetStreams told to fail for x"))
    );
    assert_eq!(
        guard.sink_volume(ONE_CHAR_SINK),
        Err(told("SinkVolume told to fail for x"))
    );
    assert_eq!(
        guard.set_sink_volume(ONE_CHAR_SINK, 0.375),
        Err(told("SetSinkVolume told to fail for x"))
    );
    assert_eq!(fake.all_calls().len(), 11, "every call reached the fake");
}

// Criterion: an `Unanswered` from the wrapped graph comes back as
// `Unanswered`, not rewritten into a `PipeWire` error.
#[test]
fn test_named_guard_returns_an_unanswered_error_unchanged() {
    let fake = FakeGraph::with_sinks(&[ONE_CHAR_SINK, ONE_CHAR_TARGET]);
    fake.fail_unanswered(GraphOp::LoadBranch);
    fake.fail_unanswered(GraphOp::Sinks);
    let mut guard = guarded(&fake);

    assert_eq!(
        guard.load_branch(ONE_CHAR_SINK, ONE_CHAR_TARGET, 37),
        Err(AudioError::Unanswered)
    );
    assert_eq!(guard.sinks(), Err(AudioError::Unanswered));
}
