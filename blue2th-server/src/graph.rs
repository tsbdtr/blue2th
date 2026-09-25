// SPDX-License-Identifier: MIT OR Apache-2.0
//! The typed seam between the routing logic and whatever drives PipeWire (#79).
//!
//! Nothing textual crosses [`Graph`]: a caller asks for node names, loaded
//! branches and volumes, and never sees the listing an implementation read them
//! from. That is what lets [`crate::audio::AudioRouter`] be driven by an
//! in-memory graph in the tests.

use crate::audio::{AudioError, CombineBranch};

/// One delay branch as the graph reports it loaded for a combined sink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedBranch {
    /// The handle [`Graph::unload_branch`] takes.
    pub id: u32,
    /// The **resolved** sink node the branch feeds, and its latency.
    pub branch: CombineBranch,
    /// Whether the branch is carrying audio: `Some(false)` is a branch that is
    /// loaded but dead, `None` is "cannot tell" and never reads as dead.
    pub live: Option<bool>,
}

/// The operations the routing logic needs from the audio graph. Every method
/// takes `&mut self` and every fallible one returns [`AudioError`], so an
/// implementation is free to talk to another thread and to lose its connection.
pub trait Graph: Send {
    /// The node names of the sinks currently present. An `Err` is "cannot
    /// tell", which a caller must not read as "no sink exists".
    fn sinks(&mut self) -> Result<Vec<String>, AudioError>;
    /// The delay branches loaded for the combined sink `sink_name`.
    fn branches(&mut self, sink_name: &str) -> Result<Vec<LoadedBranch>, AudioError>;
    /// Create the shared null sink the player streams into.
    fn create_combined_sink(&mut self, sink_name: &str) -> Result<(), AudioError>;
    /// Load one delay branch from `sink_name`'s monitor into `real_sink`.
    fn load_branch(
        &mut self,
        sink_name: &str,
        real_sink: &str,
        latency_ms: u32,
    ) -> Result<(), AudioError>;
    /// Unload one branch by the id [`Graph::branches`] reported for it.
    fn unload_branch(&mut self, id: u32) -> Result<(), AudioError>;
    /// Change the delay of the loaded branch `id` in place, without unloading
    /// it. An id no branch carries is an `Err`.
    fn set_branch_delay(&mut self, id: u32, delay_ms: u32) -> Result<(), AudioError>;
    /// Remove the combined sink `sink_name` and every branch belonging to it.
    fn teardown(&mut self, sink_name: &str) -> Result<(), AudioError>;
    /// Make `sink` the default sink.
    fn set_default_sink(&mut self, sink: &str) -> Result<(), AudioError>;
    /// The volume of `sink` as a fraction, `None` when it cannot be read.
    fn sink_volume(&mut self, sink: &str) -> Option<f32>;
    /// Set the volume of `sink` to `level`, a fraction.
    fn set_sink_volume(&mut self, sink: &str, level: f32) -> Result<(), AudioError>;
}

#[cfg(test)]
pub mod fake {
    //! An in-memory [`Graph`] for the tests: it keeps sinks and branches
    //! coherent across calls, records every call in order, and can be told to
    //! fail.

    use super::{Graph, LoadedBranch};
    use crate::audio::{AudioError, CombineBranch};
    use std::sync::{Arc, Mutex, MutexGuard};

    /// One call the fake received. Reads are recorded too, so a test can assert
    /// that the graph was not even looked at.
    #[derive(Debug, Clone, PartialEq)]
    pub enum GraphCall {
        Sinks,
        Branches {
            sink_name: String,
        },
        SinkVolume {
            sink: String,
        },
        CreateCombinedSink {
            sink_name: String,
        },
        LoadBranch {
            sink_name: String,
            real_sink: String,
            latency_ms: u32,
        },
        UnloadBranch {
            id: u32,
        },
        SetBranchDelay {
            id: u32,
            delay_ms: u32,
        },
        Teardown {
            sink_name: String,
        },
        SetDefaultSink {
            sink: String,
        },
        SetSinkVolume {
            sink: String,
            level: f32,
        },
    }

    impl GraphCall {
        /// Whether the call changes the graph, as opposed to reading it.
        pub fn is_mutating(&self) -> bool {
            !matches!(
                self,
                GraphCall::Sinks | GraphCall::Branches { .. } | GraphCall::SinkVolume { .. }
            )
        }
    }

    /// The trait method a failure rule applies to.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum GraphOp {
        Sinks,
        Branches,
        CreateCombinedSink,
        LoadBranch,
        UnloadBranch,
        SetBranchDelay,
        Teardown,
        SetDefaultSink,
        SinkVolume,
        SetSinkVolume,
    }

    /// Fail calls to `op`: every one, only those naming `node`, and only once
    /// `skip` matching calls have been let through.
    #[derive(Debug)]
    struct FailRule {
        op: GraphOp,
        node: Option<String>,
        skip: usize,
    }

    #[derive(Debug)]
    struct SeededBranch {
        combined: String,
        loaded: LoadedBranch,
    }

    #[derive(Debug)]
    struct State {
        sinks: Vec<String>,
        branches: Vec<SeededBranch>,
        volumes: Vec<(String, f32)>,
        default_sink: Option<String>,
        next_id: u32,
        new_branch_liveness: Option<bool>,
        log: Vec<GraphCall>,
        rules: Vec<FailRule>,
        empty_names_refused: usize,
    }

    /// The in-memory graph. Cloning it clones a *handle*: every clone reads and
    /// writes the same state, which is how a test keeps looking at the graph
    /// after the router has taken ownership of its `Box<dyn Graph>`.
    #[derive(Debug, Clone)]
    pub struct FakeGraph {
        state: Arc<Mutex<State>>,
    }

    impl Default for FakeGraph {
        fn default() -> Self {
            Self::new()
        }
    }

    impl FakeGraph {
        /// An empty graph: no sink, no branch, nothing recorded.
        pub fn new() -> Self {
            Self {
                state: Arc::new(Mutex::new(State {
                    sinks: Vec::new(),
                    branches: Vec::new(),
                    volumes: Vec::new(),
                    default_sink: None,
                    next_id: 1,
                    new_branch_liveness: Some(true),
                    log: Vec::new(),
                    rules: Vec::new(),
                    empty_names_refused: 0,
                })),
            }
        }

        /// A graph already carrying `sinks`.
        pub fn with_sinks(sinks: &[&str]) -> Self {
            let fake = Self::new();
            for sink in sinks {
                fake.add_sink(sink);
            }
            fake
        }

        fn state(&self) -> MutexGuard<'_, State> {
            self.state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        }

        // --- Seeding: none of this is recorded in the call log. ---

        /// Make a sink appear, as a speaker connecting does.
        pub fn add_sink(&self, name: &str) {
            let mut state = self.state();
            if !state.sinks.iter().any(|s| s == name) {
                state.sinks.push(name.to_string());
            }
        }

        /// Make a sink disappear, as a speaker switched off does. Its branches
        /// stay loaded: a module outlives its sink's node.
        pub fn remove_sink(&self, name: &str) {
            self.state().sinks.retain(|s| s != name);
        }

        /// Put a branch in the graph as if an earlier pass had loaded it, and
        /// return its id.
        pub fn seed_branch(
            &self,
            combined: &str,
            real_sink: &str,
            latency_ms: u32,
            live: Option<bool>,
        ) -> u32 {
            let mut state = self.state();
            let id = state.next_id;
            state.next_id += 1;
            state.branches.push(SeededBranch {
                combined: combined.to_string(),
                loaded: LoadedBranch {
                    id,
                    branch: CombineBranch {
                        sink: real_sink.to_string(),
                        latency_ms,
                    },
                    live,
                },
            });
            id
        }

        /// The liveness every branch loaded from now on reports.
        pub fn set_new_branch_liveness(&self, live: Option<bool>) {
            self.state().new_branch_liveness = live;
        }

        /// Give `sink` a volume to read back.
        pub fn set_volume(&self, sink: &str, level: f32) {
            let mut state = self.state();
            state.volumes.retain(|(name, _)| name != sink);
            state.volumes.push((sink.to_string(), level));
        }

        /// Fail every call to `op`.
        pub fn fail(&self, op: GraphOp) {
            self.state().rules.push(FailRule {
                op,
                node: None,
                skip: 0,
            });
        }

        /// Fail the calls to `op` that name `node` (for `load_branch`, the real
        /// sink).
        pub fn fail_for(&self, op: GraphOp, node: &str) {
            self.state().rules.push(FailRule {
                op,
                node: Some(node.to_string()),
                skip: 0,
            });
        }

        /// Drop every failure rule: the graph answers normally from now on.
        pub fn clear_failures(&self) {
            self.state().rules.clear();
        }

        /// Let `successes` calls to `op` through, then fail every later one.
        pub fn fail_after(&self, op: GraphOp, successes: usize) {
            self.state().rules.push(FailRule {
                op,
                node: None,
                skip: successes,
            });
        }

        // --- Inspection. ---

        /// The mutating calls received, in order. A failed attempt is recorded
        /// like any other: what the router *asked for* is the subject.
        pub fn calls(&self) -> Vec<GraphCall> {
            self.state()
                .log
                .iter()
                .filter(|call| call.is_mutating())
                // Cloned out of the lock: the log keeps growing behind it.
                .cloned()
                .collect()
        }

        /// Every call received, reads included, in order.
        pub fn all_calls(&self) -> Vec<GraphCall> {
            // Cloned out of the lock: the log keeps growing behind it.
            self.state().log.clone()
        }

        /// Forget the calls recorded so far, so the next pass is read alone.
        pub fn clear_calls(&self) {
            self.state().log.clear();
        }

        /// The branches currently loaded for `combined`, without recording a call.
        pub fn loaded(&self, combined: &str) -> Vec<LoadedBranch> {
            self.state()
                .branches
                .iter()
                .filter(|b| b.combined == combined)
                // Cloned out of the lock, as a snapshot.
                .map(|b| b.loaded.clone())
                .collect()
        }

        /// The sinks currently present, without recording a call.
        pub fn sink_names(&self) -> Vec<String> {
            // Cloned out of the lock, as a snapshot.
            self.state().sinks.clone()
        }

        /// The default sink, if one was ever set.
        pub fn default_sink(&self) -> Option<String> {
            // Cloned out of the lock, as a snapshot.
            self.state().default_sink.clone()
        }

        /// How many calls were refused for carrying an empty node name.
        pub fn empty_names_refused(&self) -> usize {
            self.state().empty_names_refused
        }
    }

    impl State {
        /// Refuse an empty node name: it names no node, and every prefix or
        /// substring predicate downstream would say yes to it.
        fn refuse_empty(&mut self, op: GraphOp, names: &[&str]) -> Result<(), AudioError> {
            if names.iter().any(|name| name.is_empty()) {
                self.empty_names_refused += 1;
                return Err(AudioError::PipeWire(format!(
                    "fake graph: {op:?} received an empty node name"
                )));
            }
            Ok(())
        }

        /// Apply the failure rules to one call to `op` naming `node`.
        fn check(&mut self, op: GraphOp, node: &str) -> Result<(), AudioError> {
            for rule in &mut self.rules {
                if rule.op != op || rule.node.as_deref().is_some_and(|n| n != node) {
                    continue;
                }
                if rule.skip > 0 {
                    rule.skip -= 1;
                    continue;
                }
                return Err(AudioError::PipeWire(format!(
                    "fake graph: {op:?} told to fail for {node}"
                )));
            }
            Ok(())
        }
    }

    impl Graph for FakeGraph {
        fn sinks(&mut self) -> Result<Vec<String>, AudioError> {
            let mut state = self.state();
            state.log.push(GraphCall::Sinks);
            state.check(GraphOp::Sinks, "")?;
            // Cloned out of the lock: the caller owns its reading.
            Ok(state.sinks.clone())
        }

        fn branches(&mut self, sink_name: &str) -> Result<Vec<LoadedBranch>, AudioError> {
            let mut state = self.state();
            state.log.push(GraphCall::Branches {
                sink_name: sink_name.to_string(),
            });
            state.refuse_empty(GraphOp::Branches, &[sink_name])?;
            state.check(GraphOp::Branches, sink_name)?;
            Ok(state
                .branches
                .iter()
                .filter(|b| b.combined == sink_name)
                // Cloned out of the lock: the caller owns its reading.
                .map(|b| b.loaded.clone())
                .collect())
        }

        fn create_combined_sink(&mut self, sink_name: &str) -> Result<(), AudioError> {
            let mut state = self.state();
            state.log.push(GraphCall::CreateCombinedSink {
                sink_name: sink_name.to_string(),
            });
            state.refuse_empty(GraphOp::CreateCombinedSink, &[sink_name])?;
            state.check(GraphOp::CreateCombinedSink, sink_name)?;
            if !state.sinks.iter().any(|s| s == sink_name) {
                state.sinks.push(sink_name.to_string());
            }
            Ok(())
        }

        fn load_branch(
            &mut self,
            sink_name: &str,
            real_sink: &str,
            latency_ms: u32,
        ) -> Result<(), AudioError> {
            let mut state = self.state();
            state.log.push(GraphCall::LoadBranch {
                sink_name: sink_name.to_string(),
                real_sink: real_sink.to_string(),
                latency_ms,
            });
            state.refuse_empty(GraphOp::LoadBranch, &[sink_name, real_sink])?;
            state.check(GraphOp::LoadBranch, real_sink)?;
            for end in [sink_name, real_sink] {
                if !state.sinks.iter().any(|s| s == end) {
                    return Err(AudioError::PipeWire(format!(
                        "fake graph: no such sink {end}"
                    )));
                }
            }
            let id = state.next_id;
            state.next_id += 1;
            let live = state.new_branch_liveness;
            state.branches.push(SeededBranch {
                combined: sink_name.to_string(),
                loaded: LoadedBranch {
                    id,
                    branch: CombineBranch {
                        sink: real_sink.to_string(),
                        latency_ms,
                    },
                    live,
                },
            });
            Ok(())
        }

        fn unload_branch(&mut self, id: u32) -> Result<(), AudioError> {
            let mut state = self.state();
            state.log.push(GraphCall::UnloadBranch { id });
            state.check(GraphOp::UnloadBranch, &id.to_string())?;
            // A branch that is already gone is not an error.
            state.branches.retain(|b| b.loaded.id != id);
            Ok(())
        }

        fn set_branch_delay(&mut self, id: u32, delay_ms: u32) -> Result<(), AudioError> {
            let mut state = self.state();
            state.log.push(GraphCall::SetBranchDelay { id, delay_ms });
            state.check(GraphOp::SetBranchDelay, &id.to_string())?;
            let branch = state
                .branches
                .iter_mut()
                .find(|b| b.loaded.id == id)
                .ok_or_else(|| AudioError::PipeWire(format!("fake graph: no branch {id}")))?;
            branch.loaded.branch.latency_ms = delay_ms;
            Ok(())
        }

        fn teardown(&mut self, sink_name: &str) -> Result<(), AudioError> {
            let mut state = self.state();
            state.log.push(GraphCall::Teardown {
                sink_name: sink_name.to_string(),
            });
            state.refuse_empty(GraphOp::Teardown, &[sink_name])?;
            state.check(GraphOp::Teardown, sink_name)?;
            state.sinks.retain(|s| s != sink_name);
            state.branches.retain(|b| b.combined != sink_name);
            Ok(())
        }

        fn set_default_sink(&mut self, sink: &str) -> Result<(), AudioError> {
            let mut state = self.state();
            state.log.push(GraphCall::SetDefaultSink {
                sink: sink.to_string(),
            });
            state.refuse_empty(GraphOp::SetDefaultSink, &[sink])?;
            state.check(GraphOp::SetDefaultSink, sink)?;
            if !state.sinks.iter().any(|s| s == sink) {
                return Err(AudioError::PipeWire(format!(
                    "fake graph: no such sink {sink}"
                )));
            }
            state.default_sink = Some(sink.to_string());
            Ok(())
        }

        fn sink_volume(&mut self, sink: &str) -> Option<f32> {
            let mut state = self.state();
            state.log.push(GraphCall::SinkVolume {
                sink: sink.to_string(),
            });
            state.refuse_empty(GraphOp::SinkVolume, &[sink]).ok()?;
            state
                .volumes
                .iter()
                .find(|(name, _)| name == sink)
                .map(|(_, level)| *level)
        }

        fn set_sink_volume(&mut self, sink: &str, level: f32) -> Result<(), AudioError> {
            let mut state = self.state();
            state.log.push(GraphCall::SetSinkVolume {
                sink: sink.to_string(),
                level,
            });
            state.refuse_empty(GraphOp::SetSinkVolume, &[sink])?;
            state.check(GraphOp::SetSinkVolume, sink)?;
            if !state.sinks.iter().any(|s| s == sink) {
                return Err(AudioError::PipeWire(format!(
                    "fake graph: no such sink {sink}"
                )));
            }
            state.volumes.retain(|(name, _)| name != sink);
            state.volumes.push((sink.to_string(), level));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{FakeGraph, GraphCall, GraphOp};
    use super::Graph;
    use crate::audio::AudioError;

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
            fake.set_default_sink(""),
            Err(AudioError::PipeWire(_))
        ));
        assert!(matches!(
            fake.set_sink_volume("", 0.5),
            Err(AudioError::PipeWire(_))
        ));
        assert!(fake.sink_volume("").is_none());

        assert_eq!(fake.empty_names_refused(), 8);
        // Nothing was changed by the refused calls.
        assert_eq!(fake.sink_names(), vec![COMBINED, SPEAKER]);
        assert!(fake.loaded(COMBINED).is_empty());
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
        fake.set_default_sink(COMBINED).unwrap();

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
                GraphCall::SetDefaultSink {
                    sink: COMBINED.to_string()
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
}
