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
mod tests;
