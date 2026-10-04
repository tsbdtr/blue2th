// SPDX-License-Identifier: MIT OR Apache-2.0
//! The typed seam between the routing logic and whatever drives PipeWire (#79).
//!
//! Nothing textual crosses [`Graph`]: a caller asks for node names, loaded
//! branches and volumes, and never sees the listing an implementation read them
//! from. That is what lets [`crate::audio::AudioRouter`] be driven by an
//! in-memory graph in the tests.

use std::time::Instant;

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
/// implementation is free to lose its connection.
///
/// Not `Send` (#147): the PipeWire implementation is the loop thread's own
/// state, whose objects are `Rc`-based and never leave that thread. A router
/// that has to cross threads asks for `dyn Graph + Send` itself.
pub trait Graph {
    /// The instant past which the calls of the message being run stop waiting
    /// for the daemon (#147). Handed once per message, before its first call:
    /// every call of that message shares it, and no other method of this
    /// trait moves it.
    fn set_deadline(&mut self, deadline: Instant);
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
    /// Delete `default.configured.audio.sink` when, and only when, its value
    /// names `sink_name` exactly (#66), and answer whether it did. Any other
    /// value is left as it is.
    fn clear_stale_default_sink(&mut self, sink_name: &str) -> Result<bool, AudioError>;
    /// Ask the session manager to move every output stream that asked for
    /// `sink_name` back onto it (#139), and answer how many were asked. No
    /// such stream is `Ok(0)`, not an error.
    fn retarget_streams(&mut self, sink_name: &str) -> Result<usize, AudioError>;
    /// The volume of `sink` as a fraction. An `Err` is "cannot tell" — the
    /// graph did not answer, or lost its connection — which a caller must not
    /// read as a sink without a level; `Ok(None)` is a sink that has no
    /// readable level.
    fn sink_volume(&mut self, sink: &str) -> Result<Option<f32>, AudioError>;
    /// Set the volume of `sink` to `level`, a fraction.
    fn set_sink_volume(&mut self, sink: &str, level: f32) -> Result<(), AudioError>;
}

/// A [`Graph`] that refuses an empty node name before the graph it wraps sees
/// it (#154): an empty name is a wildcard to every prefix or substring match
/// below it, never "no node". Every other call is delegated as it is.
///
/// No `Deref` to the wrapped graph, on purpose: `LoopState` carries inherent
/// methods named like the trait's, and a call resolved through `Deref` where
/// [`Graph`] is not in scope would skip the guard without a sound. The wrapped
/// graph is reached only through [`NamedGuard::inner_mut`].
pub(crate) struct NamedGuard<G> {
    inner: G,
}

impl<G> NamedGuard<G> {
    /// Put `inner` behind the guard.
    pub(crate) fn new(inner: G) -> Self {
        Self { inner }
    }

    /// The wrapped graph, for what is not a [`Graph`] call (connection
    /// management on the loop thread).
    pub(crate) fn inner_mut(&mut self) -> &mut G {
        &mut self.inner
    }
}

impl<G: Graph> Graph for NamedGuard<G> {
    fn set_deadline(&mut self, deadline: Instant) {
        self.inner.set_deadline(deadline);
    }

    fn sinks(&mut self) -> Result<Vec<String>, AudioError> {
        self.inner.sinks()
    }

    fn branches(&mut self, sink_name: &str) -> Result<Vec<LoadedBranch>, AudioError> {
        named("sink", sink_name)?;
        self.inner.branches(sink_name)
    }

    fn create_combined_sink(&mut self, sink_name: &str) -> Result<(), AudioError> {
        named("sink", sink_name)?;
        self.inner.create_combined_sink(sink_name)
    }

    fn load_branch(
        &mut self,
        sink_name: &str,
        real_sink: &str,
        latency_ms: u32,
    ) -> Result<(), AudioError> {
        named("sink", sink_name)?;
        named("target sink", real_sink)?;
        self.inner.load_branch(sink_name, real_sink, latency_ms)
    }

    fn unload_branch(&mut self, id: u32) -> Result<(), AudioError> {
        self.inner.unload_branch(id)
    }

    fn set_branch_delay(&mut self, id: u32, delay_ms: u32) -> Result<(), AudioError> {
        self.inner.set_branch_delay(id, delay_ms)
    }

    fn teardown(&mut self, sink_name: &str) -> Result<(), AudioError> {
        named("sink", sink_name)?;
        self.inner.teardown(sink_name)
    }

    fn clear_stale_default_sink(&mut self, sink_name: &str) -> Result<bool, AudioError> {
        named("sink", sink_name)?;
        self.inner.clear_stale_default_sink(sink_name)
    }

    fn retarget_streams(&mut self, sink_name: &str) -> Result<usize, AudioError> {
        named("sink", sink_name)?;
        self.inner.retarget_streams(sink_name)
    }

    fn sink_volume(&mut self, sink: &str) -> Result<Option<f32>, AudioError> {
        named("sink", sink)?;
        self.inner.sink_volume(sink)
    }

    fn set_sink_volume(&mut self, sink: &str, level: f32) -> Result<(), AudioError> {
        named("sink", sink)?;
        self.inner.set_sink_volume(sink, level)
    }
}

/// Refuse an empty name before a graph acts on it: an empty name is a
/// wildcard to every match below it, never "no node".
pub(crate) fn named(what: &str, name: &str) -> Result<(), AudioError> {
    if name.is_empty() {
        return Err(AudioError::PipeWire(format!("empty {what} name refused")));
    }
    Ok(())
}

#[cfg(test)]
pub mod fake {
    //! An in-memory [`Graph`] for the tests: it keeps sinks and branches
    //! coherent across calls, records every call in order, and can be told to
    //! fail.

    use super::{Graph, LoadedBranch};
    use crate::audio::{AudioError, CombineBranch};
    use crate::graph_pw::configured_default_names;
    use std::sync::{Arc, Mutex, MutexGuard};
    use std::time::Instant;

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
        ClearStaleDefaultSink {
            sink_name: String,
        },
        RetargetStreams {
            sink_name: String,
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
        ClearStaleDefaultSink,
        RetargetStreams,
        SinkVolume,
        SetSinkVolume,
    }

    /// Fail calls to `op`: every one, only those naming `node`, and only once
    /// `skip` matching calls have been let through — with
    /// [`AudioError::Unanswered`] when `unanswered`, `PipeWire` otherwise.
    #[derive(Debug)]
    struct FailRule {
        op: GraphOp,
        node: Option<String>,
        skip: usize,
        unanswered: bool,
    }

    #[derive(Debug)]
    struct SeededBranch {
        combined: String,
        loaded: LoadedBranch,
    }

    /// What a test asked to have run when the sink list is next read.
    struct Hook(Box<dyn FnOnce() + Send>);

    impl std::fmt::Debug for Hook {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("Hook")
        }
    }

    #[derive(Debug)]
    struct State {
        sinks: Vec<String>,
        branches: Vec<SeededBranch>,
        volumes: Vec<(String, f32)>,
        /// The raw value of `default.configured.audio.sink`, as WirePlumber
        /// stores it: `{"name":"<node>"}`, or anything else a user left there.
        configured_default: Option<String>,
        /// What `retarget_streams` answers once a call gets through: the
        /// number of streams it claims to have asked to move.
        streams_to_retarget: usize,
        next_id: u32,
        new_branch_liveness: Option<bool>,
        log: Vec<GraphCall>,
        /// Every deadline handed through [`Graph::set_deadline`], in order.
        /// Kept out of `log`: a deadline is not a call the daemon sees.
        deadlines: Vec<Instant>,
        /// Run once, from inside the next `sinks()`, then forgotten.
        on_next_sinks_read: Option<Hook>,
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
                    configured_default: None,
                    streams_to_retarget: 0,
                    next_id: 1,
                    new_branch_liveness: Some(true),
                    log: Vec::new(),
                    deadlines: Vec::new(),
                    on_next_sinks_read: None,
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

        /// Make the loaded branch `id` report `live` from now on, as a branch
        /// that loses its links does.
        pub fn set_branch_liveness(&self, id: u32, live: Option<bool>) {
            let mut state = self.state();
            for branch in state.branches.iter_mut().filter(|b| b.loaded.id == id) {
                branch.loaded.live = live;
            }
        }

        /// Give `sink` a volume to read back.
        pub fn set_volume(&self, sink: &str, level: f32) {
            let mut state = self.state();
            state.volumes.retain(|(name, _)| name != sink);
            state.volumes.push((sink.to_string(), level));
        }

        /// Put `value` in `default.configured.audio.sink`, as an earlier run of
        /// blue2th or a `wpctl set-default` left it; `None` is no key at all.
        pub fn set_configured_default(&self, value: Option<&str>) {
            self.state().configured_default = value.map(str::to_string);
        }

        /// Make `retarget_streams` answer `count` from now on. A new fake
        /// answers `0`: a paused `librespot` holds no stream.
        pub fn set_streams_to_retarget(&self, count: usize) {
            self.state().streams_to_retarget = count;
        }

        /// Run `hook` once, from inside the next `sinks()` call — before it
        /// answers, and with the fake's own state unlocked. What happens
        /// "while a message is running" (#147): a message's first graph call
        /// is a read of the sink list.
        pub fn run_on_next_sinks_read(&self, hook: impl FnOnce() + Send + 'static) {
            self.state().on_next_sinks_read = Some(Hook(Box::new(hook)));
        }

        /// Fail every call to `op`.
        pub fn fail(&self, op: GraphOp) {
            self.state().rules.push(FailRule {
                op,
                node: None,
                skip: 0,
                unanswered: false,
            });
        }

        /// Fail the calls to `op` that name `node` (for `load_branch`, the real
        /// sink).
        pub fn fail_for(&self, op: GraphOp, node: &str) {
            self.state().rules.push(FailRule {
                op,
                node: Some(node.to_string()),
                skip: 0,
                unanswered: false,
            });
        }

        /// Fail every call to `op` with [`AudioError::Unanswered`], as a
        /// daemon that stalls past the message's deadline does, rather than
        /// with the usual `PipeWire("… told to fail")`.
        pub fn fail_unanswered(&self, op: GraphOp) {
            self.state().rules.push(FailRule {
                op,
                node: None,
                skip: 0,
                unanswered: true,
            });
        }

        /// Fail the calls to `op` that name `node` with
        /// [`AudioError::Unanswered`], as [`Self::fail_for`] does with the
        /// usual `PipeWire("… told to fail")`: a daemon that stalls on one
        /// speaker's call after answering the others'.
        pub fn fail_unanswered_for(&self, op: GraphOp, node: &str) {
            self.state().rules.push(FailRule {
                op,
                node: Some(node.to_string()),
                skip: 0,
                unanswered: true,
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
                unanswered: false,
            });
        }

        /// Let `successes` calls to `op` through, then fail every later one
        /// with [`AudioError::Unanswered`]: a daemon that answers the first
        /// reads of a message and stalls on the next (#152).
        pub fn fail_unanswered_after(&self, op: GraphOp, successes: usize) {
            self.state().rules.push(FailRule {
                op,
                node: None,
                skip: successes,
                unanswered: true,
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

        /// The mutating calls received, in order, less the stream
        /// re-targeting (#139): where that call sits among a build's branch
        /// loads is the router's choice, so a test pinning the rest of a
        /// build's sequence reads this instead of [`Self::calls`].
        pub fn routing_calls(&self) -> Vec<GraphCall> {
            self.calls()
                .into_iter()
                .filter(|call| !matches!(call, GraphCall::RetargetStreams { .. }))
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

        /// Every deadline the graph was handed, in order (#147). Not part of
        /// the call log, and not forgotten by [`Self::clear_calls`].
        pub fn deadlines(&self) -> Vec<Instant> {
            // Cloned out of the lock, as a snapshot.
            self.state().deadlines.clone()
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

        /// The raw value of `default.configured.audio.sink`, `None` when the
        /// key is absent.
        pub fn configured_default(&self) -> Option<String> {
            // Cloned out of the lock, as a snapshot.
            self.state().configured_default.clone()
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
                if rule.unanswered {
                    return Err(AudioError::Unanswered);
                }
                return Err(AudioError::PipeWire(format!(
                    "fake graph: {op:?} told to fail for {node}"
                )));
            }
            Ok(())
        }
    }

    impl Graph for FakeGraph {
        fn set_deadline(&mut self, deadline: Instant) {
            self.state().deadlines.push(deadline);
        }

        fn sinks(&mut self) -> Result<Vec<String>, AudioError> {
            // Taken out and run with the state unlocked: the hook is the
            // test's own code.
            let hook = self.state().on_next_sinks_read.take();
            if let Some(Hook(hook)) = hook {
                hook();
            }
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

        fn clear_stale_default_sink(&mut self, sink_name: &str) -> Result<bool, AudioError> {
            let mut state = self.state();
            state.log.push(GraphCall::ClearStaleDefaultSink {
                sink_name: sink_name.to_string(),
            });
            state.refuse_empty(GraphOp::ClearStaleDefaultSink, &[sink_name])?;
            state.check(GraphOp::ClearStaleDefaultSink, sink_name)?;
            // The same decision `PipeWireGraph` takes, so the fake never clears
            // what the real graph would keep.
            if !configured_default_names(state.configured_default.as_deref(), sink_name) {
                return Ok(false);
            }
            state.configured_default = None;
            Ok(true)
        }

        fn retarget_streams(&mut self, sink_name: &str) -> Result<usize, AudioError> {
            let mut state = self.state();
            state.log.push(GraphCall::RetargetStreams {
                sink_name: sink_name.to_string(),
            });
            state.refuse_empty(GraphOp::RetargetStreams, &[sink_name])?;
            state.check(GraphOp::RetargetStreams, sink_name)?;
            // Streams are not modelled, only their count: `0` unless a test
            // set one, since re-targeting nothing is a success.
            Ok(state.streams_to_retarget)
        }

        fn sink_volume(&mut self, sink: &str) -> Result<Option<f32>, AudioError> {
            let mut state = self.state();
            state.log.push(GraphCall::SinkVolume {
                sink: sink.to_string(),
            });
            state.refuse_empty(GraphOp::SinkVolume, &[sink])?;
            state.check(GraphOp::SinkVolume, sink)?;
            Ok(state
                .volumes
                .iter()
                .find(|(name, _)| name == sink)
                .map(|(_, level)| *level))
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
}
