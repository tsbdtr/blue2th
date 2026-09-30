// SPDX-License-Identifier: MIT OR Apache-2.0

//! PC backend audio engine: the test tone's lifecycle (play/pause/stop),
//! PipeWire sink volume, and the shared playback state.
//!
//! The test tone is a PipeWire stream pinned to the combined sink (see
//! [`crate::tone`]), so the PC's default sink is never written (#66). Volume
//! targets each speaker's PipeWire device, not the source, so the same path
//! serves `librespot`.
//!
//! Kept behind a small interface (this module) so the audio engine can be
//! swapped out later without touching the route layer.

use std::{
    cell::RefCell,
    time::{Duration, Instant},
};

use blue2th_proto::{AudioGraphStatus, PlaybackState, PlaybackStatus, SpeakerTarget};

use crate::graph::{Graph, LoadedBranch};
use crate::graph_pw::GraphEvent;

/// Clamp a requested volume into the valid `0.0..=1.0` range.
///
/// `f32::clamp` propagates `NaN` unchanged, which would store a `NaN` volume and
/// serialize as JSON `null` (breaking the `PlaybackState` round-trip), so a
/// `NaN` input is treated as silence.
pub fn clamp_volume(level: f32) -> f32 {
    if level.is_nan() {
        return 0.0;
    }
    level.clamp(0.0, 1.0)
}

/// Decide the single volume `GET /playback` reports for the whole selection.
///
/// `levels` carries one entry per selected speaker, in selection order, `None`
/// for a sink that could not be read. The selection's levels are reported only
/// when every one of them is readable, lies in `0.0..=1.0`, and they agree at
/// whole-percent resolution. The returned level is therefore always in
/// `0.0..=1.0`, as `PlaybackState.volume` documents. Otherwise the last
/// `commanded` level is reported: it is true as a command, and it never presents
/// one speaker's level as if it were everyone's.
pub fn reported_volume(levels: &[Option<f32>], commanded: f32) -> f32 {
    let mut agreed: Option<u32> = None;
    for level in levels {
        // A sink that could not be read makes the selection undecidable: nothing
        // here is known to be true of every speaker. So does one whose level
        // `PlaybackState.volume` cannot express — `NaN`, an infinity, or a value
        // outside `0.0..=1.0` (an over-amplified sink reads as e.g. 153%).
        // Reporting a clamped 100% there would name a level no speaker is at,
        // which is the defect this rule exists to remove. The range check also
        // keeps the cast below meaningful: `as` saturates, so an unchecked
        // infinity would round-trip as 21474836, and two distinct huge levels
        // would both saturate to the same percentage and count as agreeing.
        let Some(level) = level.filter(|l| (0.0..=1.0).contains(l)) else {
            return commanded;
        };
        let pct = (level * 100.0).round() as u32;
        match agreed {
            Some(first) if first != pct => return commanded,
            Some(_) => {},
            None => agreed = Some(pct),
        }
    }
    // An empty selection agrees on nothing, so it falls back to `commanded` too.
    match agreed {
        Some(pct) => pct as f32 / 100.0,
        None => commanded,
    }
}

/// Pluggable audio output. The engine drives the state machine and delegates the
/// actual sound to an implementation of this trait, so tests can run without an
/// audio device. `Send` is required because the engine lives behind an
/// `Arc<Mutex<_>>` shared across async tasks.
pub trait AudioOutput: Send {
    /// Begin streaming the test tone from its start (fresh playback).
    fn start(&mut self) -> Result<(), AudioError>;
    /// Resume a previously paused stream.
    fn resume(&mut self) -> Result<(), AudioError>;
    /// Pause the stream, keeping its position.
    fn pause(&mut self) -> Result<(), AudioError>;
    /// Stop and discard the stream.
    fn stop(&mut self) -> Result<(), AudioError>;
    /// Whether playback has reached the end of the source on its own (so the
    /// engine can transition back to `Stopped`). `false` for outputs that never
    /// actually play (e.g. tests).
    fn is_finished(&self) -> bool;
}

/// No-op output: the state machine runs, but no device is opened and no external
/// command is spawned. Used by `AudioEngine::new()` so unit/route tests stay
/// green without PipeWire or an audio backend.
#[derive(Default)]
pub struct NullOutput;

impl AudioOutput for NullOutput {
    fn start(&mut self) -> Result<(), AudioError> {
        Ok(())
    }
    fn resume(&mut self) -> Result<(), AudioError> {
        Ok(())
    }
    fn pause(&mut self) -> Result<(), AudioError> {
        Ok(())
    }
    fn stop(&mut self) -> Result<(), AudioError> {
        Ok(())
    }
    fn is_finished(&self) -> bool {
        false
    }
}

/// In-memory playback model that drives the transitions and delegates sound to an
/// [`AudioOutput`]. The route layer drives the same transitions through an
/// `Arc<Mutex<AudioEngine>>` held in the Axum router state.
pub struct AudioEngine {
    status: PlaybackStatus,
    volume: f32,
    output: Box<dyn AudioOutput>,
}

impl AudioEngine {
    /// A fresh, stopped engine at full volume with a no-op output (no audio
    /// device). Used by tests and as the default.
    pub fn new() -> Self {
        Self::with_output(Box::new(NullOutput))
    }

    /// A fresh, stopped engine at full volume driving the given output. The
    /// router uses this with [`crate::tone::PipeWireToneOutput`] for real playback.
    pub fn with_output(output: Box<dyn AudioOutput>) -> Self {
        Self {
            status: PlaybackStatus::Stopped,
            volume: 1.0,
            output,
        }
    }

    /// Start (or resume) playback of the test tone. Idempotent while
    /// already playing.
    ///
    /// This drives only the in-memory state machine and the (host-gated) audio
    /// output; the precondition that a speaker is connected is enforced by the
    /// route layer before this is called.
    pub fn play(&mut self) -> Result<PlaybackState, AudioError> {
        self.reconcile();
        match self.status {
            PlaybackStatus::Playing => {},
            PlaybackStatus::Stopped => self.start_output()?,
            PlaybackStatus::Paused => self.resume_output()?,
        }
        self.status = PlaybackStatus::Playing;
        Ok(self.playback_state())
    }

    /// Pause playback. Idempotent no-op when nothing is playing.
    pub fn pause(&mut self) -> Result<PlaybackState, AudioError> {
        self.reconcile();
        if self.status == PlaybackStatus::Playing {
            self.pause_output()?;
            self.status = PlaybackStatus::Paused;
        }
        Ok(self.playback_state())
    }

    /// Stop playback and reset to the start. Idempotent no-op when stopped.
    pub fn stop(&mut self) -> Result<PlaybackState, AudioError> {
        if self.status != PlaybackStatus::Stopped {
            self.stop_output()?;
            self.status = PlaybackStatus::Stopped;
        }
        Ok(self.playback_state())
    }

    /// Reconcile the state with the output then return it. Used by `/playback`
    /// so the UI sees the engine return to `Stopped` once the tone ends on its
    /// own (the output has no way to push that transition).
    pub fn poll_state(&mut self) -> PlaybackState {
        self.reconcile();
        self.playback_state()
    }

    /// If the output finished on its own — the tone played out, or its stream
    /// lost its target — while we still believe we are `Playing` or `Paused`,
    /// fall back to `Stopped`, so the next play starts it afresh rather than
    /// resuming a stream that is gone (#66).
    fn reconcile(&mut self) {
        if self.status != PlaybackStatus::Stopped && self.output.is_finished() {
            self.status = PlaybackStatus::Stopped;
        }
    }

    /// Record the desired volume (clamped). The actual PipeWire sink volume is
    /// applied by the route layer, which knows the target speaker's sink; this
    /// just keeps the reported state in step.
    pub fn set_volume(&mut self, level: f32) -> Result<PlaybackState, AudioError> {
        self.volume = clamp_volume(level);
        Ok(self.playback_state())
    }

    /// Snapshot of the current playback state.
    pub fn playback_state(&self) -> PlaybackState {
        PlaybackState {
            status: self.status,
            volume: self.volume,
            // The engine does not read the graph: `GET /playback` sets the
            // field from what the router answered.
            audio_graph: AudioGraphStatus::Responsive,
        }
    }

    // --- Output seam ---------------------------------------------------------
    //
    // These delegate to the pluggable `AudioOutput`. `NullOutput` makes them
    // no-ops (tests, no audio device); `PipeWireToneOutput` performs real
    // playback.

    /// Begin streaming the test tone into the combined sink.
    fn start_output(&mut self) -> Result<(), AudioError> {
        self.output.start()
    }

    /// Resume a paused output stream.
    fn resume_output(&mut self) -> Result<(), AudioError> {
        self.output.resume()
    }

    /// Pause the output stream.
    fn pause_output(&mut self) -> Result<(), AudioError> {
        self.output.pause()
    }

    /// Stop and drop the output stream.
    fn stop_output(&mut self) -> Result<(), AudioError> {
        self.output.stop()
    }
}

impl Default for AudioEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// Errors raised by the audio engine.
#[derive(Debug)]
pub enum AudioError {
    /// No speaker is connected, so playback cannot be routed anywhere.
    NoSpeakerConnected,
    /// The PipeWire daemon is unreachable or rejected the request.
    PipeWire(String),
}

impl std::fmt::Display for AudioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AudioError::NoSpeakerConnected => write!(f, "no speaker connected"),
            AudioError::PipeWire(msg) => write!(f, "PipeWire error: {msg}"),
        }
    }
}

impl std::error::Error for AudioError {}

/// One branch of a PipeWire combined sink: the speaker's `bluez_output.*` sink
/// node name and the per-speaker delay (ms) to apply to that branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CombineBranch {
    /// The speaker's `bluez_output.*` sink node-name prefix (from
    /// [`bluez_sink_prefix`]); the hardware seam resolves it to the live node
    /// (which carries a trailing card suffix, e.g. `.1`).
    pub sink: String,
    /// The delay the branch's delay node applies, in milliseconds: the
    /// speaker's offset, as it is (#81).
    pub latency_ms: u32,
}

/// Pure plan for a PipeWire combined sink spanning the selected speakers' sinks,
/// each branch delayed by its speaker's offset. Building this performs no I/O;
/// [`AudioRouter`] applies it to its [`Graph`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CombineSinkSpec {
    /// Node name of the combined sink to create.
    pub sink_name: String,
    /// The member branches, one per target speaker.
    pub branches: Vec<CombineBranch>,
}

/// Build the (pure, testable) combined-sink plan for the given targets: each
/// target maps to its `bluez_output.*` sink name and to its offset as the
/// branch delay. Used by every non-empty selection; performs no I/O.
pub fn combine_sink_plan(targets: &[SpeakerTarget]) -> CombineSinkSpec {
    let branches = targets
        .iter()
        .map(|t| CombineBranch {
            sink: bluez_sink_prefix(&t.address),
            latency_ms: t.offset_ms,
        })
        .collect();
    CombineSinkSpec {
        sink_name: "blue2th_combined".to_string(),
        branches,
    }
}

/// Derive the `bluez_output.*` PipeWire sink node-name prefix for a speaker MAC
/// (colons → underscores, upper-cased), matching what BlueZ creates.
pub fn bluez_sink_prefix(mac: &str) -> String {
    format!("bluez_output.{}", mac.to_uppercase().replace(':', "_"))
}

/// What attempting a plan's branches produced: the branches that were loaded, and
/// a message for each one that was not.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct BranchLoadReport {
    /// The `sink` prefix of each branch that was loaded, in plan order.
    pub loaded: Vec<String>,
    /// One message per branch that could not be resolved or loaded.
    pub failures: Vec<String>,
}

/// Attempt every branch of a plan, independently: resolve it, load it, and record
/// the outcome. A speaker whose `bluez_output.*` node has not appeared yet must
/// not stop the branches that would succeed — that is what made a repair fix the
/// previous speaker and fail on the current one (#75). The failures come back as
/// a set so the caller can still warn and let the next tick retry.
///
/// `resolve` and `load` are injected so the decision is testable on its own;
/// [`AudioRouter`] passes closures over its [`Graph`], resolving each prefix
/// against the graph's sinks and loading through [`Graph::load_branch`].
pub fn load_planned_branches<R, L>(
    branches: &[CombineBranch],
    mut resolve: R,
    mut load: L,
) -> BranchLoadReport
where
    R: FnMut(&CombineBranch) -> Result<String, AudioError>,
    L: FnMut(&CombineBranch, &str) -> Result<(), AudioError>,
{
    let mut report = BranchLoadReport::default();
    for branch in branches {
        let resolved = match resolve(branch) {
            Ok(resolved) => resolved,
            Err(err) => {
                report.failures.push(err.to_string());
                continue;
            },
        };
        match load(branch, &resolved) {
            // Cloned because the report outlives the borrowed plan.
            Ok(()) => report.loaded.push(branch.sink.clone()),
            Err(err) => report.failures.push(err.to_string()),
        }
    }
    report
}

impl BranchLoadReport {
    /// Turn what the pass could not do into one error for the caller, after the
    /// whole set has been tried. Reporting rather than swallowing is what keeps
    /// the warning in the log and makes the next tick retry (#75).
    fn into_result(self) -> Result<(), AudioError> {
        if self.failures.is_empty() {
            return Ok(());
        }
        Err(AudioError::PipeWire(self.failures.join("; ")))
    }
}

/// Whether the periodic repair pass has anything to do: a branch that is missing
/// or dead only matters while audio is flowing towards it, and skipping keeps the
/// idle cost at zero.
pub fn should_repair_branches(selection: &[SpeakerTarget], anything_playing: bool) -> bool {
    !selection.is_empty() && anything_playing
}

/// How long after a branch is loaded it is reloaded once, to confirm it.
///
/// A branch loaded towards a Bluetooth sink can come up complete — linked,
/// running, no error anywhere — and silent, so completely that a stream written
/// straight into that sink is silent too. A second load **five to seven seconds
/// later** starts it; one a few milliseconds later did not, and broke a start
/// that worked (measured 2026-09-06, #75). Deselecting and reselecting the
/// silent speaker, the operator's workaround, is the same gap by hand. Seen at
/// startup and after a daemon restart on the #81 build, so every load is
/// confirmed, not only a speaker that came back.
///
/// A literal of its own, not the safety net's tick: the confirmation has its
/// own timer (#80), and a gap following a 30 s tick would delay the remedy
/// sixfold.
pub const CONFIRM_GAP: Duration = Duration::from_secs(5);

/// How often the safety-net repair pass runs (#80). Registry events drive the
/// repair; this net only catches what no event reports, such as the combined
/// sink destroyed by hand.
pub const SAFETY_NET_TICK: Duration = Duration::from_secs(30);

/// What woke a repair pass (#80).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PassReason {
    SinkAppeared { name: String, at: Instant },
    SinkVanished { name: String },
    CombinedSinkVanished { name: String },
    ConfirmationDue,
    SafetyNet,
    Reconnected,
}

impl std::fmt::Display for PassReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SinkAppeared { name, .. } => write!(f, "sink {name} appeared"),
            Self::SinkVanished { name } => write!(f, "sink {name} vanished"),
            Self::CombinedSinkVanished { name } => {
                write!(f, "combined sink {name} removed from outside")
            },
            Self::ConfirmationDue => f.write_str("a confirming reload fell due"),
            Self::SafetyNet => f.write_str("the safety net"),
            Self::Reconnected => f.write_str("the PipeWire connection came back"),
        }
    }
}

/// The reason `event` wakes a repair pass for `selection`, if it does.
///
/// A speaker sink event wakes only when a selected speaker's
/// [`bluez_sink_prefix`] names that sink under `prefix_names_node`'s rule. The
/// combined sink's removal is no speaker's sink, and wakes for any selection
/// (#139). With nothing selected there is nothing to repair, so nothing wakes.
pub fn wake_for(event: &GraphEvent, selection: &[SpeakerTarget]) -> Option<PassReason> {
    if selection.is_empty() {
        return None;
    }
    let selected = |name: &str| {
        selection
            .iter()
            .any(|speaker| prefix_names_node(&bluez_sink_prefix(&speaker.address), name))
    };
    match event {
        GraphEvent::SinkAppeared { name, at } if selected(name) => {
            Some(PassReason::SinkAppeared {
                // Cloned: the reason outlives the borrowed event.
                name: name.clone(),
                at: *at,
            })
        },
        GraphEvent::SinkVanished { name, .. } if selected(name) => {
            Some(PassReason::SinkVanished {
                // Cloned: the reason outlives the borrowed event.
                name: name.clone(),
            })
        },
        GraphEvent::CombinedSinkVanished { name, .. } => {
            Some(PassReason::CombinedSinkVanished {
                // Cloned: the reason outlives the borrowed event.
                name: name.clone(),
            })
        },
        GraphEvent::Reconnected => Some(PassReason::Reconnected),
        _ => None,
    }
}

/// The reason one drained burst of `events` wakes a single repair pass for
/// `selection`, if any of them wakes one (#139): the combined sink's removal
/// wins wherever it sits, so the fallback it may call for is never lost to a
/// speaker event drained before it; otherwise the first event that wakes.
pub fn wake_for_burst(events: &[GraphEvent], selection: &[SpeakerTarget]) -> Option<PassReason> {
    let reasons = events.iter().filter_map(|event| wake_for(event, selection));
    let mut first = None;
    for reason in reasons {
        if matches!(reason, PassReason::CombinedSinkVanished { .. }) {
            return Some(reason);
        }
        first = first.or(Some(reason));
    }
    first
}

/// Whether a repair pass woken by `reason` must fall back to pausing Spotify
/// (#139), given whether its route succeeded and whether the re-targeting of
/// the streams did.
pub fn fallback_pause_due(reason: &PassReason, routed_ok: bool, retargeted_ok: bool) -> bool {
    matches!(reason, PassReason::CombinedSinkVanished { .. }) && !(routed_ok && retargeted_ok)
}

/// What a selection change has to do to an already-loaded combined sink: the
/// branches to load, the loaded ones to retune in place, and the loaded ones to
/// unload.
///
/// `to_unload` and `to_retune` carry the branches as the graph reported them —
/// i.e. with the **resolved** node name — because that is what [`AudioRouter`]
/// matches against the loaded branches to find their ids; `to_retune` carries
/// the **planned** delay. `to_load` carries the plan's `bluez_output.*`
/// prefixes, which the router resolves at load time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BranchPlan {
    /// Planned speakers with no loaded branch, which must be loaded.
    pub to_load: Vec<CombineBranch>,
    /// Loaded branches of planned speakers at another delay, to be retuned.
    pub to_retune: Vec<CombineBranch>,
    /// Loaded branches the spec no longer calls for, which must be unloaded.
    pub to_unload: Vec<CombineBranch>,
}

/// Compare the delay branches currently loaded for a combined sink against the
/// plan and decide what to change, leaving matching branches — and the null
/// sink — alone. Pure; the caller performs the loads, retunes and unloads.
///
/// Each speaker is decided on its own (#81): a missing branch is loaded alone
/// and a branch at another delay is retuned in place, so the speakers already
/// playing are never torn down for the sake of another one.
pub fn reconcile_branches(loaded: &[CombineBranch], spec: &CombineSinkSpec) -> BranchPlan {
    let mut plan = BranchPlan::default();
    for planned in &spec.branches {
        let up = loaded
            .iter()
            .find(|up| prefix_names_node(&planned.sink, &up.sink));
        match up {
            // Cloned because the plan outlives the borrowed spec.
            None => plan.to_load.push(planned.clone()),
            Some(up) if up.latency_ms != planned.latency_ms => {
                plan.to_retune.push(CombineBranch {
                    // Cloned: the retune names the resolved node the graph reported.
                    sink: up.sink.clone(),
                    latency_ms: planned.latency_ms,
                });
            },
            Some(_) => {},
        }
    }
    plan.to_unload = loaded
        .iter()
        .filter(|up| {
            !spec
                .branches
                .iter()
                .any(|planned| prefix_names_node(&planned.sink, &up.sink))
        })
        // Cloned because the plan outlives the borrowed listing.
        .cloned()
        .collect();
    plan
}

/// The delay line that carries each confirming reload from the pass that loaded
/// a branch to the first pass at least [`CONFIRM_GAP`] later, keyed by the
/// branch's `bluez_output.*` prefix.
///
/// Split out of `AudioRouter` so the transition is pure and can be driven with
/// explicit instants in a test, without a graph around it.
#[derive(Debug, Default, PartialEq, Eq)]
struct ConfirmationRegister {
    due: Vec<(String, Instant)>,
}

impl ConfirmationRegister {
    /// Arm a confirming reload of each of `sinks`, loaded at `now`. Arming a
    /// sink again restarts its wait; an empty name arms nothing.
    fn arm(&mut self, sinks: &[String], now: Instant) {
        for sink in sinks.iter().filter(|sink| !sink.is_empty()) {
            self.due.retain(|(armed, _)| armed != sink);
            // Cloned: the register keeps the name past this pass.
            self.due.push((sink.clone(), now));
        }
    }

    /// The sinks armed at least [`CONFIRM_GAP`] before `now`, in the order they
    /// were armed. Each is handed out once: a confirming reload does not arm
    /// itself, so it never repeats.
    fn take_due(&mut self, now: Instant) -> Vec<String> {
        let (due, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.due)
            .into_iter()
            .partition(|(_, armed_at)| now.saturating_duration_since(*armed_at) >= CONFIRM_GAP);
        self.due = waiting;
        due.into_iter().map(|(sink, _)| sink).collect()
    }

    /// Forget every armed reload: a graph built from nothing owes none of them.
    fn clear(&mut self) {
        self.due.clear();
    }

    /// When the earliest armed reload falls due; `None` when nothing is armed.
    fn next_due(&self) -> Option<Instant> {
        self.due
            .iter()
            .map(|(_, armed_at)| *armed_at + CONFIRM_GAP)
            .min()
    }
}

/// The planned branches whose speaker sink is actually present in `sinks`.
///
/// A speaker that is switched off has no `bluez_output.*` node, so its branch
/// cannot be loaded however often it is tried. Left in the plan it would keep the
/// reconciliation permanently one branch short, and attempt a load that cannot
/// succeed on every repair tick (#75).
pub fn reachable_branches(branches: &[CombineBranch], sinks: &[String]) -> Vec<CombineBranch> {
    branches
        .iter()
        .filter(|branch| sink_named_by_prefix(sinks, &branch.sink).is_some())
        .cloned()
        .collect()
}

/// Pick the sink node-name matching `prefix` out of `sinks`. A
/// `bluez_output.<MAC>` prefix resolves to the name carrying the card suffix
/// (`bluez_output.<MAC>.1`); an already exact node name resolves to itself.
/// Pure — performs no I/O.
///
/// A candidate must either *equal* `prefix` or continue it with a `.`, the
/// separator PipeWire puts before the card index. That boundary is what keeps a
/// sink merely sharing the opening characters (`blue2th_combined_old` for
/// `blue2th_combined`) from being answered instead of the target, and an exact
/// name wins over any longer namesake wherever the two sit in the list.
///
/// An **empty** prefix matches nothing, explicitly: it starts every name, so a
/// plain `starts_with` answered the first sink listed — the PC's own output —
/// which is exactly the silent wrong-sink fallback this resolution exists to
/// prevent. `spotify_target_sink(&[])` is empty, so the value is reachable. The
/// boundary rule alone would not do: a blank name equals the empty prefix.
///
/// Among several `.`-suffixed candidates the first one listed wins.
pub fn sink_named_by_prefix(sinks: &[String], prefix: &str) -> Option<String> {
    first_sink_named_by(sinks.iter().map(String::as_str), prefix)
}

/// [`sink_named_by_prefix`] over a tab-separated sink table, the node name in
/// the second column of each line. Only the tests read that shape: it keeps the
/// resolution tests written against the sink tables of (#78) running on the one
/// rule production uses.
#[cfg(test)]
fn sink_matching_prefix(listing: &str, prefix: &str) -> Option<String> {
    first_sink_named_by(
        listing.lines().filter_map(|line| line.split('\t').nth(1)),
        prefix,
    )
}

/// The single resolution rule both [`sink_named_by_prefix`] and
/// [`sink_matching_prefix`] apply, whatever the names were read from.
fn first_sink_named_by<'a>(names: impl Iterator<Item = &'a str>, prefix: &str) -> Option<String> {
    if prefix.is_empty() {
        return None;
    }
    let mut suffixed: Option<&str> = None;
    for name in names {
        if name == prefix {
            return Some(name.to_string());
        }
        if suffixed.is_none() && prefix_names_node(prefix, name) {
            suffixed = Some(name);
        }
    }
    suffixed.map(|name| name.to_string())
}

/// Whether `node` is a node the `bluez_output.*`-style `prefix` names: the same
/// name, or the prefix continued by the `.` PipeWire puts before the card index.
/// The single copy of the rule [`sink_named_by_prefix`] resolves with and
/// [`reconcile_branches`] compares with, so one set of tests pins both.
///
/// An **empty** prefix names nothing: it opens every name, and it also *equals* a
/// blank one — the two ways a missing target used to claim an arbitrary node.
fn prefix_names_node(prefix: &str, node: &str) -> bool {
    !prefix.is_empty()
        && (node == prefix
            || node
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('.')))
}

/// The routing logic, driven through a [`Graph`] rather than against PipeWire
/// directly (#79). It owns the confirmation register the reconciliation carries
/// from one pass to the next, so two routers never see each other's history.
pub struct AudioRouter {
    /// The graph every routing decision is read from and applied to.
    graph: Box<dyn Graph>,
    /// The branches owed a confirming reload, and since when. See
    /// [`CONFIRM_GAP`].
    confirmation: ConfirmationRegister,
    /// What "now" is for the confirmation; a test drives it by hand.
    clock: Box<dyn Fn() -> Instant + Send>,
    /// Poked whenever a confirming reload is armed, so the confirmation timer
    /// waiting on an empty register learns of it (#80).
    armed: std::sync::Arc<tokio::sync::Notify>,
    /// How many changes the graph has accepted from this router — a load, an
    /// unload, a retune, a build — a refused call counting for none. A pass
    /// compares it before and after to tell whether it changed anything.
    changes: u64,
    /// Whether the last route created the combined sink and could not
    /// re-target the streams onto it (#139).
    retarget_failed: bool,
}

impl AudioRouter {
    /// A router over `graph`, with nothing armed.
    pub fn new(graph: Box<dyn Graph>) -> Self {
        Self::with_clock(graph, Box::new(Instant::now))
    }

    /// A router over `graph` whose confirmation reads the time from `clock`.
    pub(crate) fn with_clock(
        graph: Box<dyn Graph>,
        clock: Box<dyn Fn() -> Instant + Send>,
    ) -> Self {
        Self {
            graph,
            confirmation: ConfirmationRegister::default(),
            clock,
            armed: std::sync::Arc::default(),
            changes: 0,
            retarget_failed: false,
        }
    }

    /// When the earliest confirming reload falls due; `None` when none is armed.
    pub fn next_confirmation_due(&self) -> Option<Instant> {
        self.confirmation.next_due()
    }

    /// Notified each time a confirming reload is armed, so the confirmation
    /// timer waiting on an empty register learns of it.
    pub(crate) fn confirmation_armed(&self) -> std::sync::Arc<tokio::sync::Notify> {
        std::sync::Arc::clone(&self.armed)
    }

    /// How many changes the graph has accepted from this router so far.
    pub(crate) fn graph_changes(&self) -> u64 {
        self.changes
    }

    /// Whether the last [`Self::route_for_targets`] created the combined sink
    /// and then failed to re-target the streams onto it (#139). A route that
    /// created nothing, or re-targeted successfully, answers `false`.
    pub(crate) fn last_retarget_failed(&self) -> bool {
        self.retarget_failed
    }

    /// Arm the confirming reload of every branch a pass has just loaded.
    fn arm_confirmation(&mut self, sink_name: &str, loaded: &[String]) {
        if loaded.is_empty() {
            return;
        }
        tracing::info!(
            "confirming reload of {}'s [{}] armed, due in {} s",
            sink_name,
            loaded.join(", "),
            CONFIRM_GAP.as_secs()
        );
        let now = (self.clock)();
        self.confirmation.arm(loaded, now);
        self.armed.notify_one();
    }

    /// Apply the PipeWire routing a selection calls for: every non-empty selection
    /// goes through the combined sink, so the target never moves when a speaker is
    /// added or dropped — a moving target respawns `librespot` and leaves an open
    /// stream behind (#70, #53). The single seam used by `/play` and by the Spotify
    /// backend, so both agree on where audio goes.
    pub fn route_for_targets(&mut self, speakers: &[SpeakerTarget]) -> Result<(), AudioError> {
        self.retarget_failed = false;
        if speakers.is_empty() {
            return Err(AudioError::NoSpeakerConnected);
        }
        self.route_to_combined(&combine_sink_plan(speakers))
    }

    /// Route playback to a combined sink spanning the plan's speakers, so a
    /// player pinned to it reaches each of them, delayed by its own offset for
    /// tunable sync. Built as a shared null sink the player feeds,
    /// plus one delay branch per speaker into its real `bluez_output.*` sink.
    ///
    /// Idempotent — when the combined sink is already up it reconciles the
    /// branches in place instead of rebuilding, so a selection change does not
    /// unload the null sink the player is streaming into; otherwise it builds the
    /// whole graph from scratch.
    fn route_to_combined(&mut self, spec: &CombineSinkSpec) -> Result<(), AudioError> {
        // A sink list that cannot be read is "cannot tell", never "the combined
        // sink does not exist": building on it would tear down a graph that is
        // playing. The error goes back and the graph is left alone.
        if self.find_sink_with_prefix(&spec.sink_name)?.is_some() {
            return self.reconcile_combined(spec);
        }
        self.build_combined(spec)
    }

    /// Build the combined sink from nothing: the shared null sink, then one delay
    /// branch per speaker. Tears any leftover down first so repeated calls do not
    /// stack modules.
    fn build_combined(&mut self, spec: &CombineSinkSpec) -> Result<(), AudioError> {
        tracing::info!(
            "building {} from nothing: {} branch(es) [{}]",
            spec.sink_name,
            spec.branches.len(),
            branches_for_log(&spec.branches)
        );
        // A default an earlier version left naming this sink (#66) goes before
        // the sink is recreated, so WirePlumber falls back to a choice of its
        // own. Losing that cleanup never costs the speakers their sound.
        match self.graph.clear_stale_default_sink(&spec.sink_name) {
            Ok(true) => tracing::info!(
                "cleared the configured default sink naming {}, left by an earlier version; \
                 choose the PC's default with `wpctl set-default <id>`",
                spec.sink_name
            ),
            Ok(false) => {},
            Err(err) => tracing::warn!(
                "could not check the configured default sink for {}: {err}",
                spec.sink_name
            ),
        }
        self.graph.teardown(&spec.sink_name)?;
        // The shared virtual sink the player streams into.
        self.graph.create_combined_sink(&spec.sink_name)?;
        self.changes += 1;
        // A stream that asked for the sink before it was destroyed is not
        // moved back by the session manager on its own (#139). Losing this
        // never fails the route: the pass reads the flag and falls back.
        // Not a graph change: it moves no branch and loads nothing.
        match self.graph.retarget_streams(&spec.sink_name) {
            Ok(0) => {},
            Ok(count) => tracing::info!("re-targeted {count} stream(s) onto {}", spec.sink_name),
            Err(err) => {
                tracing::warn!(
                    "could not re-target the streams onto {}: {err}",
                    spec.sink_name
                );
                self.retarget_failed = true;
            },
        }
        // One delay branch per speaker: combined.monitor -> real sink, delayed by
        // the speaker's offset, the per-branch sync tuning.
        let report = self.load_planned_branches_live(&spec.sink_name, &spec.branches);
        // Nothing armed before this build concerns the branches it just loaded.
        self.confirmation.clear();
        self.arm_confirmation(&spec.sink_name, &report.loaded);
        report.into_result()
    }

    /// Bring an already-loaded combined sink in line with the plan, without ever
    /// touching the null sink: that is what keeps a live stream playing across a
    /// selection change, since a stream pinned to the null sink stays linked to
    /// it only while it exists.
    ///
    /// Each speaker is handled alone (#81): dead branches go, unwanted ones go, a
    /// branch at another delay is retuned in place, and a missing one is loaded —
    /// in that order. Every branch loaded is reloaded once more on the first pass
    /// at least [`CONFIRM_GAP`] later.
    fn reconcile_combined(&mut self, spec: &CombineSinkSpec) -> Result<(), AudioError> {
        let listed = self.graph.branches(&spec.sink_name)?;
        // A dead branch reads as absent below, so the reconciliation would load its
        // replacement without ever asking for the stale one to go. Unloaded here,
        // before that load, so the speaker never has two branches feeding it.
        for dead in listed.iter().filter(|b| b.live == Some(false)) {
            tracing::info!(
                "branch {} into {} ruled dead: unloading it",
                dead.id,
                dead.branch.sink
            );
            // Best-effort: a branch that is already gone is not an error, and one
            // failure must not stop the rest of a repair.
            if self.graph.unload_branch(dead.id).is_ok() {
                self.changes += 1;
            }
        }
        // Unknown liveness keeps the branch: a transient read failure would
        // otherwise read as "everything is dead" and reload every branch under
        // the audio it protects.
        let kept: Vec<&LoadedBranch> = listed.iter().filter(|b| b.live != Some(false)).collect();
        let loaded: Vec<CombineBranch> = kept
            .iter()
            // Cloned because `reconcile_branches` compares plain branches, and
            // the ids stay behind in `kept` for the calls below.
            .map(|b| b.branch.clone())
            .collect();
        // Nothing read is "cannot tell", not "every speaker is gone": acting on
        // it would unload every branch. So an unreadable list ends the pass, and
        // so does one naming no sink at all: it does not even name the combined
        // sink this pass was entered for, so it describes no graph worth acting on.
        let sinks = match self.graph.sinks() {
            Ok(names) if !names.is_empty() => names,
            _ => return Ok(()),
        };
        // A speaker that is switched off is absent, not broken: asking for it on
        // every tick would attempt a load that cannot succeed.
        let reachable = CombineSinkSpec {
            // Cloned because the reachable plan is a spec of its own.
            sink_name: spec.sink_name.clone(),
            branches: reachable_branches(&spec.branches, &sinks),
        };
        let plan = reconcile_branches(&loaded, &reachable);

        for up in kept.iter().filter(|up| {
            plan.to_unload
                .iter()
                .any(|gone| gone.sink == up.branch.sink)
        }) {
            tracing::info!(
                "branch {} into {} is no longer planned: unloading it",
                up.id,
                up.branch.sink
            );
            self.graph.unload_branch(up.id)?;
            self.changes += 1;
        }

        let mut failures = Vec::new();
        for retune in &plan.to_retune {
            for up in kept.iter().filter(|up| up.branch.sink == retune.sink) {
                tracing::info!(
                    "retuning branch {} into {} in place: {} ms -> {} ms",
                    up.id,
                    up.branch.sink,
                    up.branch.latency_ms,
                    retune.latency_ms
                );
                // One rejected delay must not stop the other speakers' repair; it
                // is reported, and the next pass retunes it again.
                match self.graph.set_branch_delay(up.id, retune.latency_ms) {
                    Ok(()) => self.changes += 1,
                    Err(err) => failures.push(err.to_string()),
                }
            }
        }

        if !plan.to_load.is_empty() {
            tracing::info!(
                "loading {} branch(es) of {} alone: [{}]",
                plan.to_load.len(),
                spec.sink_name,
                branches_for_log(&plan.to_load)
            );
        }
        let report = self.load_planned_branches_live(&spec.sink_name, &plan.to_load);
        failures.extend(report.failures);

        // Learn which branches are owed their confirmation before arming this
        // pass's loads, so a branch is never confirmed in the pass that loaded it;
        // a speaker loaded again in this very pass waits for its new gap instead.
        let now = (self.clock)();
        let owed: Vec<String> = self
            .confirmation
            .take_due(now)
            .into_iter()
            .filter(|prefix| !report.loaded.contains(prefix))
            .collect();
        self.arm_confirmation(&spec.sink_name, &report.loaded);
        let confirming: Vec<CombineBranch> = reachable
            .branches
            .iter()
            .filter(|planned| owed.contains(&planned.sink))
            // Cloned because the reload outlives the borrowed plan.
            .cloned()
            .collect();
        if confirming.is_empty() {
            return BranchLoadReport {
                loaded: report.loaded,
                failures,
            }
            .into_result();
        }
        tracing::info!(
            "confirming reload of {}: [{}]",
            spec.sink_name,
            branches_for_log(&confirming)
        );
        // Reloading the branch now, at least `CONFIRM_GAP` after its load, is the
        // measured remedy (#75); no other branch is touched, and the reload is not
        // armed again.
        for branch in self.graph.branches(&spec.sink_name)? {
            if confirming
                .iter()
                .any(|planned| prefix_names_node(&planned.sink, &branch.branch.sink))
            {
                self.graph.unload_branch(branch.id)?;
                self.changes += 1;
            }
        }
        let second = self.load_planned_branches_live(&spec.sink_name, &confirming);
        failures.extend(second.failures);
        BranchLoadReport {
            loaded: second.loaded,
            failures,
        }
        .into_result()
    }

    /// Attempt every branch against the graph, resolving each prefix to its node
    /// and loading a delay branch from `sink_name`'s monitor.
    fn load_planned_branches_live(
        &mut self,
        sink_name: &str,
        branches: &[CombineBranch],
    ) -> BranchLoadReport {
        // `load_planned_branches` holds both closures at once and each one needs
        // the graph, so the exclusive borrow is handed out per call instead.
        let graph = RefCell::new(&mut self.graph);
        let report = load_planned_branches(
            branches,
            |branch| resolve_branch_sink(graph.borrow_mut().as_mut(), branch),
            |branch, real_sink| {
                graph
                    .borrow_mut()
                    .load_branch(sink_name, real_sink, branch.latency_ms)
            },
        );
        self.changes += report.loaded.len() as u64;
        report
    }

    /// Change one speaker's delay **in place**: the new value is set on that
    /// speaker's delay node, and nothing is unloaded or loaded (#81). The shared
    /// null sink and the other speakers' branches receive no call, so whatever
    /// feeds the sink — the tone player or `librespot` — keeps streaming.
    ///
    /// A speaker whose sink is listed but carries no branch is not an error:
    /// its offset is stored by the caller, and the branch loads with it on the
    /// next reconciliation. A speaker whose sink is absent is an `Err`, as it
    /// was when a retune reloaded the branch.
    pub fn retune_branch(
        &mut self,
        sink_name: &str,
        branch: &CombineBranch,
    ) -> Result<(), AudioError> {
        let real = resolve_branch_sink(self.graph.as_mut(), branch)?;
        for up in self.graph.branches(sink_name)? {
            if up.branch.sink == real {
                tracing::info!(
                    "retuning branch {} into {real} in place: {} ms -> {} ms",
                    up.id,
                    up.branch.latency_ms,
                    branch.latency_ms
                );
                self.graph.set_branch_delay(up.id, branch.latency_ms)?;
            }
        }
        Ok(())
    }

    /// Tear down a combined sink built by [`Self::route_for_targets`]: the null
    /// sink and every branch belonging to it. A sink that does not exist yet is
    /// not an error.
    pub fn teardown(&mut self, sink_name: &str) -> Result<(), AudioError> {
        self.graph.teardown(sink_name)
    }

    /// Whether the combined null sink is currently loaded, i.e. whether the graph can
    /// be reconciled in place — a branch retuned, a selection change applied — rather
    /// than built from scratch. An unreadable graph answers `false`.
    pub fn combined_sink_exists(&mut self, sink_name: &str) -> bool {
        matches!(self.find_sink_with_prefix(sink_name), Ok(Some(_)))
    }

    /// Resolve a logical playback target to the live PipeWire node name to hand a
    /// player. The target is either a `bluez_output.*` prefix (from
    /// [`bluez_sink_prefix`], which carries no card suffix) or an exact node name
    /// such as `blue2th_combined`, which resolves to itself. Errors rather than
    /// falling back to the default sink, so a vanished speaker — or an empty target,
    /// which no sink can carry — is reported instead of silently sending audio
    /// elsewhere.
    pub fn resolve_target_sink(&mut self, target: &str) -> Result<String, AudioError> {
        find_sink_with_prefix(self.graph.as_mut(), target)
            .ok()
            .flatten()
            .ok_or_else(|| AudioError::PipeWire(format!("no PipeWire sink for target {target}")))
    }

    /// Set the speaker's sink volume by sink name, so it matches what
    /// [`Self::sink_volumes`] reads back even if the system default differs. The
    /// level is clamped here, before it reaches the graph.
    pub fn set_sink_volume(&mut self, mac: &str, level: f32) -> Result<(), AudioError> {
        let sink = self.bluetooth_sink_for(mac)?;
        self.graph.set_sink_volume(&sink, clamp_volume(level))
    }

    /// The live volume of each speaker in `macs`, in order, over **one** read
    /// of the sink list (#145) — picks up a change made on a speaker itself
    /// (AVRCP). `Err` when that list cannot be read, `Ok(None)` for a speaker
    /// whose sink is absent from a list that read fine, or whose level
    /// [`Graph::sink_volume`] could not read: that call answers an `Option`,
    /// so it cannot tell a failure from an unreadable route.
    ///
    /// An unreadable list stops the read there: while the graph does not
    /// answer, every further call would only wait out its own timeout.
    pub fn sink_volumes(&mut self, macs: &[String]) -> Result<Vec<Option<f32>>, AudioError> {
        if macs.is_empty() {
            return Ok(Vec::new());
        }
        let sinks = self.graph.sinks()?;
        Ok(macs
            .iter()
            .map(|mac| {
                sink_named_by_prefix(&sinks, &bluez_sink_prefix(mac))
                    .and_then(|sink| self.graph.sink_volume(&sink))
            })
            .collect())
    }

    /// Find the sink BlueZ created for a speaker, matched by its MAC. The node
    /// name looks like `bluez_output.AA_BB_CC_DD_EE_FF.1` (colons → underscores),
    /// matched against the prefix from [`bluez_sink_prefix`].
    ///
    /// A sink list that cannot be read keeps its own error: it is a graph
    /// failure, not a speaker without a sink (#145).
    fn bluetooth_sink_for(&mut self, mac: &str) -> Result<String, AudioError> {
        find_sink_with_prefix(self.graph.as_mut(), &bluez_sink_prefix(mac))?
            .ok_or_else(|| AudioError::PipeWire(format!("no PipeWire sink for speaker {mac}")))
    }

    /// [`find_sink_with_prefix`] over this router's graph.
    fn find_sink_with_prefix(&mut self, prefix: &str) -> Result<Option<String>, AudioError> {
        find_sink_with_prefix(self.graph.as_mut(), prefix)
    }
}

/// `branches` on one log line: each sink with its latency.
fn branches_for_log(branches: &[CombineBranch]) -> String {
    branches
        .iter()
        .map(|branch| format!("{} @ {} ms", branch.sink, branch.latency_ms))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Resolve a live sink node-name from its `bluez_output.*` prefix (which the
/// combined-sink plan stores without the trailing card suffix). `Ok(None)` when
/// no sink currently matches; an `Err` is a sink list that could not be read,
/// which a caller must not take for an absent sink.
///
/// An empty prefix names no node, so it resolves to nothing without the graph
/// being asked.
fn find_sink_with_prefix(
    graph: &mut dyn Graph,
    prefix: &str,
) -> Result<Option<String>, AudioError> {
    if prefix.is_empty() {
        return Ok(None);
    }
    let sinks = graph.sinks()?;
    Ok(sink_named_by_prefix(&sinks, prefix))
}

/// Resolve a branch's `bluez_output.*` prefix to the live node name, erroring
/// rather than sending audio elsewhere when the speaker's sink has vanished.
fn resolve_branch_sink(
    graph: &mut dyn Graph,
    branch: &CombineBranch,
) -> Result<String, AudioError> {
    find_sink_with_prefix(graph, &branch.sink)
        .ok()
        .flatten()
        .ok_or_else(|| AudioError::PipeWire(format!("no PipeWire sink for prefix {}", branch.sink)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::targets::MAX_OFFSET_MS;
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    // Criterion: `POST /volume` clamps to `0.0..=1.0` — value below 0 saturates
    // to 0.0.
    #[test]
    fn test_clamp_volume_below_range_saturates_to_zero() {
        assert_eq!(clamp_volume(-0.5), 0.0);
        assert_eq!(clamp_volume(-1000.0), 0.0);
    }

    // Criterion: `POST /volume` clamps to `0.0..=1.0` — value above 1 saturates
    // to 1.0.
    #[test]
    fn test_clamp_volume_above_range_saturates_to_one() {
        assert_eq!(clamp_volume(1.5), 1.0);
        assert_eq!(clamp_volume(1000.0), 1.0);
    }

    // Criterion: `POST /volume` clamps to `0.0..=1.0` — a NaN level (which
    // `f32::clamp` would otherwise propagate, serializing as JSON `null`) is
    // treated as silence rather than stored.
    #[test]
    fn test_clamp_volume_nan_saturates_to_zero() {
        assert_eq!(clamp_volume(f32::NAN), 0.0);
    }

    // Criterion: `POST /volume` clamps to `0.0..=1.0` — in-range values are kept
    // unchanged, including the boundaries.
    #[test]
    fn test_clamp_volume_in_range_is_unchanged() {
        assert_eq!(clamp_volume(0.0), 0.0);
        assert_eq!(clamp_volume(0.5), 0.5);
        assert_eq!(clamp_volume(1.0), 1.0);
    }

    // Criterion: `POST /play` starts playback and returns `status: playing` —
    // a fresh engine begins stopped.
    #[test]
    fn test_new_engine_starts_stopped() {
        let engine = AudioEngine::new();
        assert_eq!(engine.playback_state().status, PlaybackStatus::Stopped);
    }

    // Criterion: pause/resume/stop transitions —
    // Stopped -> Playing -> Paused -> Playing -> Stopped.
    #[test]
    fn test_playback_state_machine_full_cycle() {
        let mut engine = AudioEngine::new();

        let state = engine.play().expect("play succeeds");
        assert_eq!(state.status, PlaybackStatus::Playing);

        let state = engine.pause().expect("pause succeeds");
        assert_eq!(state.status, PlaybackStatus::Paused);

        let state = engine.play().expect("resume succeeds");
        assert_eq!(state.status, PlaybackStatus::Playing);

        let state = engine.stop().expect("stop succeeds");
        assert_eq!(state.status, PlaybackStatus::Stopped);
    }

    /// An output recording what the engine asked of it, in order, and
    /// optionally refusing to start — as the tone stream does with no daemon.
    struct RecordingOutput {
        log: Arc<std::sync::Mutex<Vec<&'static str>>>,
        refuse_start: bool,
        /// Raised by the test to play a stream that ended on its own.
        finished: Arc<AtomicBool>,
    }

    impl AudioOutput for RecordingOutput {
        fn start(&mut self) -> Result<(), AudioError> {
            self.log.lock().unwrap().push("start");
            if self.refuse_start {
                return Err(AudioError::PipeWire("no PipeWire daemon".to_string()));
            }
            Ok(())
        }
        fn resume(&mut self) -> Result<(), AudioError> {
            self.log.lock().unwrap().push("resume");
            Ok(())
        }
        fn pause(&mut self) -> Result<(), AudioError> {
            self.log.lock().unwrap().push("pause");
            Ok(())
        }
        fn stop(&mut self) -> Result<(), AudioError> {
            self.log.lock().unwrap().push("stop");
            Ok(())
        }
        fn is_finished(&self) -> bool {
            self.finished.load(Ordering::Relaxed)
        }
    }

    // Criterion: `AudioOutput::start` takes no tone, and the state machine is
    // otherwise unchanged — each transition is one call of its own on the
    // output: a pause and a play resume (keeping the tone's position) rather
    // than stop and start it again, and a play after a stop starts afresh.
    #[test]
    fn test_engine_drives_start_pause_resume_stop_on_the_output() {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut engine = AudioEngine::with_output(Box::new(RecordingOutput {
            log: Arc::clone(&log),
            refuse_start: false,
            finished: Arc::default(),
        }));

        assert!(engine.play().is_ok());
        assert!(engine.play().is_ok(), "a play while playing is idempotent");
        assert!(engine.pause().is_ok());
        assert!(engine.play().is_ok());
        assert!(engine.stop().is_ok());
        assert!(engine.play().is_ok());

        assert_eq!(
            *log.lock().unwrap(),
            vec!["start", "pause", "resume", "stop", "start"]
        );
    }

    // Non-nominal: the tone's target vanished while it was paused — the last
    // speaker deselected, whose handler pauses the engine and then tears the
    // combined sink down, so the stream went `Unconnected`. The engine reads
    // that as `Stopped`, and the next play starts the tone afresh: resuming a
    // dead stream played nothing until a stop. The near miss, a pause then a
    // play on a live stream, resumes it (`test_engine_drives_start_pause_…`).
    #[test]
    fn test_engine_play_after_the_output_ended_while_paused_starts_afresh() {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let finished = Arc::new(AtomicBool::new(false));
        let mut engine = AudioEngine::with_output(Box::new(RecordingOutput {
            log: Arc::clone(&log),
            refuse_start: false,
            finished: Arc::clone(&finished),
        }));
        assert!(engine.play().is_ok());
        assert!(engine.pause().is_ok());

        finished.store(true, Ordering::Relaxed);

        assert_eq!(engine.poll_state().status, PlaybackStatus::Stopped);
        finished.store(false, Ordering::Relaxed);
        assert!(engine.play().is_ok());
        assert_eq!(*log.lock().unwrap(), vec!["start", "pause", "start"]);
        assert_eq!(engine.poll_state().status, PlaybackStatus::Playing);
    }

    // Non-nominal: with no PipeWire daemon the output refuses to start — the
    // error comes back from `play` at once, and the engine stays `Stopped`.
    #[test]
    fn test_engine_play_with_a_refused_start_errs_and_stays_stopped() {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut engine = AudioEngine::with_output(Box::new(RecordingOutput {
            log: Arc::clone(&log),
            refuse_start: true,
            finished: Arc::default(),
        }));

        let played = engine.play();

        assert!(
            matches!(played, Err(AudioError::PipeWire(_))),
            "got {played:?}"
        );
        assert_eq!(engine.poll_state().status, PlaybackStatus::Stopped);
    }

    // Criterion: pause/stop while stopped are idempotent (unchanged state).
    #[test]
    fn test_pause_while_stopped_is_idempotent() {
        let mut engine = AudioEngine::new();
        let state = engine.pause().expect("idempotent pause succeeds");
        assert_eq!(state.status, PlaybackStatus::Stopped);
    }

    // Criterion: pause/stop while stopped are idempotent (unchanged state).
    #[test]
    fn test_stop_while_stopped_is_idempotent() {
        let mut engine = AudioEngine::new();
        let state = engine.stop().expect("idempotent stop succeeds");
        assert_eq!(state.status, PlaybackStatus::Stopped);
    }

    // Criterion: all levels readable and equal -> that value is reported.
    #[test]
    fn test_reported_volume_agreeing_levels_reports_the_common_value() {
        assert_eq!(
            reported_volume(&[Some(0.4), Some(0.4)], 0.7),
            0.4,
            "two sinks at 40% report 40%, not the commanded level"
        );
    }

    // Criterion: all levels readable and equal -> that value is reported (a
    // single selected speaker agrees with itself, so its live level is reported).
    #[test]
    fn test_reported_volume_single_readable_level_reports_that_level() {
        assert_eq!(reported_volume(&[Some(0.55)], 0.2), 0.55);
    }

    // Criterion: levels readable but not all equal -> the commanded level.
    #[test]
    fn test_reported_volume_differing_levels_reports_commanded() {
        assert_eq!(
            reported_volume(&[Some(0.3), Some(0.8)], 0.55),
            0.55,
            "the slider must not jump to one speaker's value"
        );
    }

    // Criterion: any level unreadable -> the commanded level, even when the
    // readable ones agree.
    #[test]
    fn test_reported_volume_one_unreadable_level_reports_commanded() {
        assert_eq!(reported_volume(&[Some(0.4), None, Some(0.4)], 0.65), 0.65);
    }

    // Criterion: any level unreadable -> the commanded level.
    #[test]
    fn test_reported_volume_all_levels_unreadable_reports_commanded() {
        assert_eq!(reported_volume(&[None, None], 0.25), 0.25);
    }

    // Criterion: no selected speaker -> the commanded level (unchanged
    // behaviour).
    #[test]
    fn test_reported_volume_empty_selection_reports_commanded() {
        assert_eq!(reported_volume(&[], 0.6), 0.6);
    }

    // Criterion: agreement is decided at whole-percent resolution — two levels a
    // whole percent apart disagree, so the commanded level is reported.
    #[test]
    fn test_reported_volume_levels_one_percent_apart_reports_commanded() {
        assert_eq!(reported_volume(&[Some(0.40), Some(0.41)], 0.75), 0.75);
    }

    // Criterion: agreement is decided at whole-percent resolution rather than by
    // float equality — two levels differing only below that resolution agree, so
    // their common whole-percent level (40%) is reported, not `commanded`.
    #[test]
    fn test_reported_volume_sub_percent_difference_reports_the_common_value() {
        let reported = reported_volume(&[Some(0.401), Some(0.404)], 0.9);
        assert!(
            (reported - 0.40).abs() < 0.005,
            "expected the shared 40% level, got {reported}"
        );
    }

    // Criterion: a `NaN` level is never counted as agreeing. `parse_first_percent`
    // cannot currently produce one, so this guards a future reader rather than
    // pinning a live bug.
    #[test]
    fn test_reported_volume_nan_level_reports_commanded() {
        assert_eq!(reported_volume(&[Some(0.5), Some(f32::NAN)], 0.35), 0.35);
        assert_eq!(reported_volume(&[Some(f32::NAN)], 0.35), 0.35);
    }

    // Criterion: a level `PlaybackState.volume` cannot express is never counted
    // as agreeing. An over-amplified sink reads as e.g. 1.53 (a 153% volume);
    // reporting it would break the DTO's
    // documented `0.0..=1.0` range, and clamping it to 100% would name a level
    // no speaker is at.
    #[test]
    fn test_reported_volume_out_of_range_level_reports_commanded() {
        assert_eq!(reported_volume(&[Some(1.53)], 0.35), 0.35);
        assert_eq!(
            reported_volume(&[Some(1.53), Some(1.53)], 0.35),
            0.35,
            "two over-amplified sinks agree on a level the API cannot report"
        );
        assert_eq!(reported_volume(&[Some(0.4), Some(1.2)], 0.35), 0.35);
        assert_eq!(reported_volume(&[Some(-0.2)], 0.35), 0.35);
    }

    // Criterion: a level that is not a number is never counted as agreeing —
    // infinities included. The percentage cast saturates, so an unfiltered
    // infinity would be reported as 21474836, and two *distinct* huge levels
    // would saturate to the same percentage and manufacture an agreement.
    #[test]
    fn test_reported_volume_infinite_level_reports_commanded() {
        assert_eq!(reported_volume(&[Some(f32::INFINITY)], 0.35), 0.35);
        assert_eq!(reported_volume(&[Some(f32::NEG_INFINITY)], 0.35), 0.35);
        assert_eq!(reported_volume(&[Some(1e30), Some(1e31)], 0.35), 0.35);
    }

    // Criterion: any level unreadable -> the commanded level, wherever it sits in
    // the selection order — the first entry decides just as the last one does.
    #[test]
    fn test_reported_volume_unreadable_level_at_either_end_reports_commanded() {
        assert_eq!(reported_volume(&[None, Some(0.4)], 0.65), 0.65);
        assert_eq!(reported_volume(&[Some(0.4), None], 0.65), 0.65);
    }

    // Criterion: `POST /volume` sets the sink volume and returns the new state;
    // out-of-range input is clamped (no error).
    #[test]
    fn test_set_volume_clamps_and_updates_state() {
        let mut engine = AudioEngine::new();

        let state = engine.set_volume(0.3).expect("set in-range volume");
        assert_eq!(state.volume, 0.3);

        let state = engine.set_volume(2.0).expect("set out-of-range volume");
        assert_eq!(state.volume, 1.0);

        let state = engine.set_volume(-1.0).expect("set negative volume");
        assert_eq!(state.volume, 0.0);
    }

    // The PipeWire sink node-name prefix is derived from a MAC by upper-casing and
    // replacing colons with underscores (matches `bluetooth_sink_for`).
    #[test]
    fn test_bluez_sink_prefix_maps_mac_to_node_prefix() {
        assert_eq!(
            bluez_sink_prefix("aa:bb:cc:dd:ee:ff"),
            "bluez_output.AA_BB_CC_DD_EE_FF"
        );
    }

    // Criterion: with two targets, the combined-sink plan lists both speakers'
    // `bluez_output.*` sink names and each speaker's offset as its branch's
    // delay — offset 250 is a delay of 250 ms, with no base on top.
    #[test]
    fn test_combine_sink_plan_lists_both_sinks_and_offsets() {
        let targets = vec![
            SpeakerTarget {
                address: "AA:BB:CC:DD:EE:FF".to_string(),
                offset_ms: 0,
            },
            SpeakerTarget {
                address: "11:22:33:44:55:66".to_string(),
                offset_ms: 250,
            },
        ];

        let spec = combine_sink_plan(&targets);
        assert_eq!(spec.branches.len(), 2);

        let first = &spec.branches[0];
        assert!(
            first.sink.starts_with("bluez_output.AA_BB_CC_DD_EE_FF"),
            "first branch must target the first speaker's bluez sink, got {}",
            first.sink
        );
        assert_eq!(first.latency_ms, 0);

        let second = &spec.branches[1];
        assert!(
            second.sink.starts_with("bluez_output.11_22_33_44_55_66"),
            "second branch must target the second speaker's bluez sink, got {}",
            second.sink
        );
        assert_eq!(second.latency_ms, 250);
    }

    // Criterion: `combine_sink_plan` builds exactly one branch for a lone
    // speaker, naming that speaker's sink prefix and its offset as the branch
    // delay — including at offset 0, which is one branch at a delay of zero,
    // not "no branch".
    #[test]
    fn test_combine_sink_plan_lone_speaker_at_zero_offset_yields_one_branch() {
        let spec = combine_sink_plan(&[SpeakerTarget {
            address: "AA:BB:CC:DD:EE:FF".to_string(),
            offset_ms: 0,
        }]);

        assert_eq!(spec.branches.len(), 1);
        let only = &spec.branches[0];
        assert_eq!(only.sink, bluez_sink_prefix("AA:BB:CC:DD:EE:FF"));
        assert_eq!(only.latency_ms, 0, "offset 0 is a delay of zero");
    }

    // Criterion: `combine_sink_plan` keeps a lone speaker's offset as the branch
    // delay — the offset only exists as the delay of its branch, so a lone
    // speaker going through the combined sink is the only way it is heard.
    #[test]
    fn test_combine_sink_plan_lone_speaker_carries_its_offset_as_latency() {
        let spec = combine_sink_plan(&[SpeakerTarget {
            address: "11:22:33:44:55:66".to_string(),
            offset_ms: 320,
        }]);

        assert_eq!(spec.branches.len(), 1);
        let only = &spec.branches[0];
        assert_eq!(only.sink, bluez_sink_prefix("11:22:33:44:55:66"));
        assert_eq!(only.latency_ms, 320);
    }

    // Criterion: the 50 ms base is gone — a branch's delay is exactly the
    // speaker's offset, so offset 0 plans a delay of 0 ms, and the largest
    // accepted offset plans exactly that many milliseconds.
    #[test]
    fn test_combine_sink_plan_offset_zero_is_delay_zero() {
        let delays = |offset_ms| {
            combine_sink_plan(&[SpeakerTarget {
                address: "AA:BB:CC:DD:EE:FF".to_string(),
                offset_ms,
            }])
            .branches
            .iter()
            .map(|b| b.latency_ms)
            .collect::<Vec<_>>()
        };

        assert_eq!(delays(0), vec![0]);
        assert_eq!(delays(MAX_OFFSET_MS), vec![MAX_OFFSET_MS]);
    }

    // Criterion: the offsets stay purely relative — two offsets differing by `n`
    // plan delays differing by exactly `n`. It is what keeps a calibration
    // made with the 50 ms base valid once the base is gone: every speaker
    // plays 50 ms earlier, and the gap between two of them is unchanged.
    #[test]
    fn test_combine_sink_plan_keeps_the_gap_between_two_offsets() {
        for (lower, higher) in [(0_u32, 70_u32), (40, 250), (250, MAX_OFFSET_MS)] {
            let spec = combine_sink_plan(&[
                SpeakerTarget {
                    address: "AA:BB:CC:DD:EE:FF".to_string(),
                    offset_ms: lower,
                },
                SpeakerTarget {
                    address: "11:22:33:44:55:66".to_string(),
                    offset_ms: higher,
                },
            ]);
            let delays: Vec<u32> = spec.branches.iter().map(|b| b.latency_ms).collect();

            assert_eq!(delays.len(), 2);
            assert_eq!(
                delays[1] - delays[0],
                higher - lower,
                "the gap between offsets {lower} and {higher}"
            );
        }
    }

    // Criterion: `route_for_targets` takes the combined path for every non-empty
    // selection. The routing itself needs a PipeWire daemon CI does not have, so the
    // pure half is pinned instead: the sink the plan names is the sink the
    // Spotify backend is pointed at, for a lone speaker as much as for two.
    // If they ever disagree, playback goes somewhere the plan did not build.
    #[test]
    fn test_combine_sink_plan_names_the_sink_spotify_is_pointed_at() {
        let lone = vec![SpeakerTarget {
            address: "AA:BB:CC:DD:EE:FF".to_string(),
            offset_ms: 0,
        }];
        let two = vec![
            SpeakerTarget {
                address: "AA:BB:CC:DD:EE:FF".to_string(),
                offset_ms: 0,
            },
            SpeakerTarget {
                address: "11:22:33:44:55:66".to_string(),
                offset_ms: 250,
            },
        ];

        for selection in [&lone, &two] {
            assert_eq!(
                combine_sink_plan(selection).sink_name,
                crate::spotify::spotify_target_sink(selection),
                "the plan must build the very sink Spotify is pointed at, for {selection:?}"
            );
        }
    }

    // Criterion: `route_for_targets` still refuses an empty selection with
    // `AudioError::NoSpeakerConnected` — the guard runs before the graph is asked,
    // which is what makes this testable without hardware.
    #[test]
    fn test_route_for_targets_empty_selection_is_refused() {
        let mut router = AudioRouter::new(Box::new(crate::graph::fake::FakeGraph::new()));
        assert!(matches!(
            router.route_for_targets(&[]),
            Err(AudioError::NoSpeakerConnected)
        ));
    }

    /// A realistic sink table (#78): tab-separated columns, the node name
    /// second, one Bluetooth speaker whose live node carries the `.1` card
    /// suffix, the PC's own output and the combined null sink.
    const SINK_TABLE: &str = concat!(
        "39\talsa_output.pci-0000_00_1f.3.analog-stereo\tPipeWire\ts32le 2ch 48000Hz\tSUSPENDED\n",
        "57\tbluez_output.80_99_E7_63_50_29.1\tPipeWire\ts16le 2ch 48000Hz\tRUNNING\n",
        "61\tblue2th_combined\tPipeWire\tf32le 2ch 48000Hz\tIDLE\n",
    );

    // Criterion: a pure function resolves a prefix against a sink table (#78),
    // mapping `bluez_output.<MAC>` to the line carrying the
    // card suffix (`bluez_output.<MAC>.1`). This is the heart of the defect: the
    // prefix itself names no live node, so `--device <prefix>` silently falls
    // back to the default sink.
    #[test]
    fn test_sink_matching_prefix_resolves_a_bluez_prefix_to_the_card_suffixed_node() {
        assert_eq!(
            sink_matching_prefix(SINK_TABLE, "bluez_output.80_99_E7_63_50_29"),
            Some("bluez_output.80_99_E7_63_50_29.1".to_string())
        );
    }

    // Criterion: the matcher returns `None` when no line matches — the speaker
    // vanished between the routing call and the spawn, and the caller must fail
    // rather than fall back to the default sink.
    #[test]
    fn test_sink_matching_prefix_without_a_matching_line_is_none() {
        assert_eq!(
            sink_matching_prefix(SINK_TABLE, "bluez_output.AA_BB_CC_DD_EE_FF"),
            None
        );
    }

    // Criterion: with several sinks present the matcher picks the right
    // `bluez_output.*` line — the second speaker's node, not the first one and
    // not the PC's own output.
    #[test]
    fn test_sink_matching_prefix_picks_the_right_line_among_several_sinks() {
        let listing = concat!(
            "39\talsa_output.pci-0000_00_1f.3.analog-stereo\tPipeWire\ts32le 2ch 48000Hz\tSUSPENDED\n",
            "57\tbluez_output.80_99_E7_63_50_29.1\tPipeWire\ts16le 2ch 48000Hz\tRUNNING\n",
            "58\tbluez_output.11_22_33_44_55_66.2\tPipeWire\ts16le 2ch 48000Hz\tIDLE\n",
            "61\tblue2th_combined\tPipeWire\tf32le 2ch 48000Hz\tIDLE\n",
        );
        assert_eq!(
            sink_matching_prefix(listing, "bluez_output.11_22_33_44_55_66"),
            Some("bluez_output.11_22_33_44_55_66.2".to_string())
        );
    }

    // Criterion: the match is anchored at the start of the node name, so a sink
    // that merely *contains* the prefix is not mistaken for the speaker's own
    // node. A `contains` implementation would answer the wrong sink here.
    #[test]
    fn test_sink_matching_prefix_ignores_a_sink_that_only_contains_the_prefix() {
        let listing = concat!(
            "44\tvirtual_bluez_output.80_99_E7_63_50_29.9\tPipeWire\ts16le 2ch 48000Hz\tIDLE\n",
            "57\tbluez_output.80_99_E7_63_50_29.1\tPipeWire\ts16le 2ch 48000Hz\tRUNNING\n",
        );
        assert_eq!(
            sink_matching_prefix(listing, "bluez_output.80_99_E7_63_50_29"),
            Some("bluez_output.80_99_E7_63_50_29.1".to_string())
        );
    }

    // Criterion (non-nominal, the path that always worked): `blue2th_combined`
    // is already an exact node name, so resolving it is a no-op and the combined
    // route keeps behaving exactly as it does today.
    #[test]
    fn test_sink_matching_prefix_leaves_an_exact_node_name_unchanged() {
        assert_eq!(
            sink_matching_prefix(SINK_TABLE, "blue2th_combined"),
            Some("blue2th_combined".to_string())
        );
    }

    // An empty prefix starts every string, so a bare `starts_with` answers the
    // first line of the listing — the PC's own output. `spotify_target_sink(&[])`
    // returns exactly that empty string, and only the `speakers.is_empty()` guard
    // at the top of `SpotifyBackend::start` stands between it and pointing
    // `--device` at the PC. Verified on a live graph (#78): before this guard,
    // `resolve_target_sink("")` answered `Ok("alsa_output.…HiFi__Speaker__sink")`.
    #[test]
    fn test_sink_matching_prefix_without_a_prefix_matches_nothing() {
        assert_eq!(sink_matching_prefix(SINK_TABLE, ""), None);
        // Also against a listing carrying a blank node-name column, which the
        // boundary rule would otherwise accept as *equal* to the empty prefix.
        assert_eq!(sink_matching_prefix("39\t\tPipeWire\tIDLE\n", ""), None);
    }

    // Criterion: the matcher picks the `bluez_output.*` line rather than an
    // unrelated sink that happens to share a prefix. A longer namesake that does
    // not continue with the `.` card separator is not the target, whatever its
    // sink index — a bare `starts_with` answers it as soon as it sorts first.
    #[test]
    fn test_sink_matching_prefix_ignores_a_longer_namesake_without_a_card_separator() {
        let listing = "60\tblue2th_combined_old\tPipeWire\tf32le 2ch 48000Hz\tIDLE\n";
        assert_eq!(sink_matching_prefix(listing, "blue2th_combined"), None);
    }

    // The exact node wins over a longer namesake wherever the two sit: a stale
    // `blue2th_combined_old` carrying a lower sink index must not shadow the
    // combined sink librespot is about to be pointed at.
    #[test]
    fn test_sink_matching_prefix_prefers_the_exact_node_over_a_longer_namesake() {
        let listing = concat!(
            "60\tblue2th_combined_old\tPipeWire\tf32le 2ch 48000Hz\tIDLE\n",
            "61\tblue2th_combined\tPipeWire\tf32le 2ch 48000Hz\tIDLE\n",
        );
        assert_eq!(
            sink_matching_prefix(listing, "blue2th_combined"),
            Some("blue2th_combined".to_string())
        );
    }

    // The tie-break is stated in the doc, so it is pinned here: with two
    // `.`-suffixed candidates for one prefix, the first line wins — the order
    // the sinks were listed in — rather than whichever the iteration happens to reach.
    #[test]
    fn test_sink_matching_prefix_takes_the_first_suffixed_candidate() {
        let listing = concat!(
            "57\tbluez_output.80_99_E7_63_50_29.1\tPipeWire\ts16le 2ch 48000Hz\tRUNNING\n",
            "58\tbluez_output.80_99_E7_63_50_29.2\tPipeWire\ts16le 2ch 48000Hz\tIDLE\n",
        );
        assert_eq!(
            sink_matching_prefix(listing, "bluez_output.80_99_E7_63_50_29"),
            Some("bluez_output.80_99_E7_63_50_29.1".to_string())
        );
    }

    // A sink table must survive whatever a tool prints (#78): a blank line, a line short of the node-name column, and a trailing
    // newline all get skipped rather than panicking or answering an empty name.
    #[test]
    fn test_sink_matching_prefix_skips_lines_without_a_node_name_column() {
        let listing = concat!(
            "\n",
            "39\n",
            "\t\n",
            "57\tbluez_output.80_99_E7_63_50_29.1\tPipeWire\ts16le 2ch 48000Hz\tRUNNING\n",
            "\n",
        );
        assert_eq!(
            sink_matching_prefix(listing, "bluez_output.80_99_E7_63_50_29"),
            Some("bluez_output.80_99_E7_63_50_29.1".to_string())
        );
    }

    /// The spec the reconciliation tests compare a loaded graph against: two
    /// speakers, offsets 0 and 250 ms, branch sinks held as `bluez_output.*`
    /// **prefixes** the way [`combine_sink_plan`] builds them.
    fn two_speaker_spec() -> CombineSinkSpec {
        combine_sink_plan(&[
            SpeakerTarget {
                address: "80:99:E7:63:50:29".to_string(),
                offset_ms: 0,
            },
            SpeakerTarget {
                address: "11:22:33:44:55:66".to_string(),
                offset_ms: 250,
            },
        ])
    }

    // Criterion: a speaker added to the selection loads its own branch and
    // nothing else — the branch already streaming is neither unloaded nor
    // retuned. The #75 rule tore it down to restart it with the newcomer.
    #[test]
    fn test_reconcile_branches_added_speaker_loads_only_that_branch() {
        let spec = two_speaker_spec();
        let already_streaming = CombineBranch {
            sink: "bluez_output.80_99_E7_63_50_29.1".to_string(),
            latency_ms: 0,
        };

        let plan = reconcile_branches(&[already_streaming], &spec);

        assert_eq!(
            plan,
            BranchPlan {
                to_load: vec![CombineBranch {
                    sink: bluez_sink_prefix("11:22:33:44:55:66"),
                    latency_ms: 250,
                }],
                to_retune: Vec::new(),
                to_unload: Vec::new(),
            }
        );
    }

    /// The sinks a graph reports: one Bluetooth speaker whose live node carries
    /// the `.1` card suffix, the PC's own output and the combined null sink.
    fn sinks_present() -> Vec<String> {
        [
            "alsa_output.pci-0000_00_1f.3.analog-stereo",
            "bluez_output.80_99_E7_63_50_29.1",
            "blue2th_combined",
        ]
        .map(String::from)
        .to_vec()
    }

    /// [`sinks_present`] without the Bluetooth speaker — the graph while it is off.
    fn sinks_without_speaker() -> Vec<String> {
        [
            "alsa_output.pci-0000_00_1f.3.analog-stereo",
            "blue2th_combined",
        ]
        .map(String::from)
        .to_vec()
    }

    // Criterion: a speaker whose sink is absent — switched off — is not part of
    // what the reconciliation compares against. It is what keeps a rebuild from
    // being asked for on every tick, since a missing branch rebuilds all.
    #[test]
    fn test_reachable_branches_drops_a_speaker_whose_sink_is_absent() {
        let spec = two_speaker_spec();

        let reachable = reachable_branches(&spec.branches, &sinks_present());

        assert_eq!(
            reachable,
            vec![CombineBranch {
                sink: bluez_sink_prefix("80:99:E7:63:50:29"),
                latency_ms: 0,
            }],
            "only the speaker with a live node survives, got {reachable:?}"
        );
    }

    // Criterion: an unreadable listing must not read as "every speaker is gone".
    // The caller guards on it; this pins what the pure half answers, so the guard
    // cannot be dropped as redundant.
    #[test]
    fn test_reachable_branches_of_an_empty_listing_keeps_nothing() {
        assert!(
            reachable_branches(&two_speaker_spec().branches, &[]).is_empty(),
            "an empty listing names no node, so it reaches no speaker"
        );
    }

    // Criterion: the churn that made the whole selection rebuild every five
    // seconds while one speaker was switched off. Reconciled against the
    // speakers actually present, a selection missing an absent one is a no-op.
    #[test]
    fn test_reconcile_branches_a_switched_off_speaker_asks_for_no_rebuild() {
        let spec = two_speaker_spec();
        let present = CombineSinkSpec {
            sink_name: spec.sink_name.clone(),
            branches: reachable_branches(&spec.branches, &sinks_present()),
        };
        let loaded = vec![CombineBranch {
            sink: "bluez_output.80_99_E7_63_50_29.1".to_string(),
            latency_ms: 0,
        }];

        let plan = reconcile_branches(&loaded, &present);

        assert_eq!(
            plan,
            BranchPlan::default(),
            "the speaker that is playing keeps its branch while the other is off"
        );
    }

    // Criterion: `CombineBranch::sink` holds the `bluez_output.<MAC>` prefix while
    // a loaded module names the resolved node `bluez_output.<MAC>.1`. Without
    // this, reconciliation would believe nothing is loaded and rebuild the whole
    // graph on every change — the very defect being fixed.
    #[test]
    fn test_reconcile_branches_matches_a_prefix_against_the_resolved_node() {
        let spec = combine_sink_plan(&[SpeakerTarget {
            address: "80:99:E7:63:50:29".to_string(),
            offset_ms: 0,
        }]);
        assert_eq!(
            spec.branches[0].sink, "bluez_output.80_99_E7_63_50_29",
            "the plan holds the prefix, not the resolved node"
        );
        let loaded = vec![CombineBranch {
            sink: "bluez_output.80_99_E7_63_50_29.1".to_string(),
            latency_ms: 0,
        }];

        let plan = reconcile_branches(&loaded, &spec);

        assert_eq!(
            plan,
            BranchPlan::default(),
            "the prefix names the loaded node, so this branch is already up"
        );
    }

    // The prefix names the loaded node only up to the `.` PipeWire puts before the
    // card index: a node that merely *opens* with the prefix is a different node,
    // the rule `sink_matching_prefix` already resolves by. A plain `starts_with`
    // here would call that foreign branch this speaker's and leave it in place.
    #[test]
    fn test_reconcile_branches_prefix_matches_the_node_only_at_a_dot_boundary() {
        let spec = combine_sink_plan(&[SpeakerTarget {
            address: "80:99:E7:63:50:29".to_string(),
            offset_ms: 0,
        }]);
        // At the planned delay, a prefix match would call it "already up"; at
        // another delay, it would retune a foreign branch in place.
        for latency_ms in [0, 30] {
            let loaded = vec![CombineBranch {
                sink: format!("{}_2.1", spec.branches[0].sink),
                latency_ms,
            }];

            let plan = reconcile_branches(&loaded, &spec);

            assert_eq!(
                plan,
                BranchPlan {
                    to_load: spec.branches.clone(),
                    to_retune: Vec::new(),
                    to_unload: loaded,
                },
                "the node continuing the prefix without a `.` is not this branch"
            );
        }
    }

    // A branch that names no node matches nothing, not even another nameless one:
    // an empty string equals an empty string, which is how a missing target used to
    // claim an arbitrary node (see `sink_matching_prefix`). `reconcile_branches` is
    // public and its inputs come from a subprocess, so the guard has to live in the
    // comparison rather than in its callers.
    #[test]
    fn test_reconcile_branches_a_branch_naming_no_node_matches_nothing() {
        for (loaded_ms, planned_ms) in [(0, 0), (30, 0)] {
            let loaded = CombineBranch {
                sink: String::new(),
                latency_ms: loaded_ms,
            };
            let planned = CombineBranch {
                sink: String::new(),
                latency_ms: planned_ms,
            };
            let spec = CombineSinkSpec {
                sink_name: "blue2th_combined".to_string(),
                branches: vec![planned.clone()],
            };

            let plan = reconcile_branches(std::slice::from_ref(&loaded), &spec);

            assert_eq!(
                plan,
                BranchPlan {
                    to_load: vec![planned],
                    to_retune: Vec::new(),
                    to_unload: vec![loaded],
                },
                "two empty names are not the same node"
            );
        }
    }

    // Criterion (same one, the other side): the prefix comparison must not match
    // a different speaker's node, or a selection change would leave the wrong
    // branch in place and never load the right one.
    #[test]
    fn test_reconcile_branches_prefix_does_not_match_another_speakers_node() {
        let spec = combine_sink_plan(&[SpeakerTarget {
            address: "80:99:E7:63:50:29".to_string(),
            offset_ms: 0,
        }]);
        for latency_ms in [0, 30] {
            let other = CombineBranch {
                sink: "bluez_output.11_22_33_44_55_66.1".to_string(),
                latency_ms,
            };

            let plan = reconcile_branches(std::slice::from_ref(&other), &spec);

            assert_eq!(
                plan,
                BranchPlan {
                    to_load: vec![CombineBranch {
                        sink: "bluez_output.80_99_E7_63_50_29".to_string(),
                        latency_ms: 0,
                    }],
                    to_retune: Vec::new(),
                    to_unload: vec![other],
                },
                "the other speaker's branch is neither this one nor retuned into it"
            );
        }
    }

    // Criterion: a pure decision says whether the repair pass runs at all — a
    // non-empty selection with something playing runs.
    #[test]
    fn test_should_repair_branches_with_a_selection_and_audio_runs() {
        let selection = vec![SpeakerTarget {
            address: "80:99:E7:63:50:29".to_string(),
            offset_ms: 0,
        }];

        assert!(should_repair_branches(&selection, true));
    }

    // Criterion: an empty selection does not run — there is nothing to repair, and
    // the pass must not build a combined sink on its own.
    #[test]
    fn test_should_repair_branches_with_an_empty_selection_does_not_run() {
        assert!(!should_repair_branches(&[], true));
    }

    // Criterion: nothing playing does not run — a dead branch matters only while
    // audio flows, and skipping keeps the idle cost at zero.
    #[test]
    fn test_should_repair_branches_with_nothing_playing_does_not_run() {
        let selection = vec![SpeakerTarget {
            address: "80:99:E7:63:50:29".to_string(),
            offset_ms: 0,
        }];

        assert!(!should_repair_branches(&selection, false));
    }

    const DEAD: &str = "bluez_output.10_28_74_E7_6A_56";
    const LIVE: &str = "bluez_output.80_99_E7_63_50_29";

    fn planned_branch(sink: &str, latency_ms: u32) -> CombineBranch {
        CombineBranch {
            sink: sink.to_string(),
            latency_ms,
        }
    }

    // Criterion: with one branch failing and one succeeding, the succeeding one is
    // loaded — the rule the hardware caught, where a repair fixed the previous
    // speaker and failed on the current one. Two speakers, the first unresolvable:
    // the second must still be attempted and loaded.
    #[test]
    fn test_load_planned_branches_failing_first_still_loads_the_second() {
        let plan = vec![planned_branch(DEAD, 0), planned_branch(LIVE, 40)];
        let resolved = RefCell::new(Vec::new());
        let loaded = RefCell::new(Vec::new());

        let report = load_planned_branches(
            &plan,
            |branch| {
                resolved.borrow_mut().push(branch.sink.clone());
                if branch.sink == DEAD {
                    Err(AudioError::PipeWire(format!(
                        "no PipeWire sink for prefix {}",
                        branch.sink
                    )))
                } else {
                    Ok(format!("{}.1", branch.sink))
                }
            },
            |_branch, real_sink| {
                loaded.borrow_mut().push(real_sink.to_string());
                Ok(())
            },
        );

        assert_eq!(
            *resolved.borrow(),
            vec![DEAD.to_string(), LIVE.to_string()],
            "an unresolvable branch must not stop the next one being attempted"
        );
        assert_eq!(
            *loaded.borrow(),
            vec![format!("{LIVE}.1")],
            "the speaker whose node exists must be fed"
        );
        assert_eq!(report.loaded, vec![LIVE.to_string()]);
        assert_eq!(
            report.failures.len(),
            1,
            "the failure is still reported, so the caller warns and the next tick retries"
        );
    }

    // Criterion: every branch in the plan is attempted, and the failures are
    // reported only after the whole set has been tried — here the failing branch
    // comes last, and the first must already have been loaded.
    #[test]
    fn test_load_planned_branches_failing_second_still_loads_the_first() {
        let plan = vec![planned_branch(LIVE, 40), planned_branch(DEAD, 0)];
        let resolved = RefCell::new(Vec::new());
        let loaded = RefCell::new(Vec::new());

        let report = load_planned_branches(
            &plan,
            |branch| {
                resolved.borrow_mut().push(branch.sink.clone());
                if branch.sink == DEAD {
                    Err(AudioError::PipeWire(format!(
                        "no PipeWire sink for prefix {}",
                        branch.sink
                    )))
                } else {
                    Ok(format!("{}.1", branch.sink))
                }
            },
            |_branch, real_sink| {
                loaded.borrow_mut().push(real_sink.to_string());
                Ok(())
            },
        );

        assert_eq!(*resolved.borrow(), vec![LIVE.to_string(), DEAD.to_string()]);
        assert_eq!(*loaded.borrow(), vec![format!("{LIVE}.1")]);
        assert_eq!(report.loaded, vec![LIVE.to_string()]);
        assert_eq!(report.failures.len(), 1);
    }

    // Criterion: every branch in the plan is attempted — when they all resolve,
    // they are all loaded and nothing is reported as failed.
    #[test]
    fn test_load_planned_branches_all_resolvable_loads_every_branch() {
        let plan = vec![planned_branch(LIVE, 40), planned_branch(DEAD, 120)];
        let loaded = RefCell::new(Vec::new());

        let report = load_planned_branches(
            &plan,
            |branch| Ok(format!("{}.1", branch.sink)),
            |branch, real_sink| {
                loaded
                    .borrow_mut()
                    .push((real_sink.to_string(), branch.latency_ms));
                Ok(())
            },
        );

        assert_eq!(
            *loaded.borrow(),
            vec![(format!("{LIVE}.1"), 40), (format!("{DEAD}.1"), 120)],
            "each branch is loaded onto its resolved node with its own latency"
        );
        assert_eq!(report.loaded, vec![LIVE.to_string(), DEAD.to_string()]);
        assert!(
            report.failures.is_empty(),
            "a fully resolvable plan reports no failure"
        );
    }

    // Criterion: every branch in the plan is attempted — an empty plan attempts
    // nothing and reports no failure, so an idle selection stays a no-op.
    #[test]
    fn test_load_planned_branches_of_an_empty_plan_attempts_nothing() {
        let attempts = RefCell::new(0_usize);

        let report = load_planned_branches(
            &[],
            |branch| {
                *attempts.borrow_mut() += 1;
                Ok(branch.sink.clone())
            },
            |_branch, _real_sink| {
                *attempts.borrow_mut() += 1;
                Ok(())
            },
        );

        assert_eq!(*attempts.borrow(), 0);
        assert_eq!(report, BranchLoadReport::default());
    }

    // Criterion: the failures are reported only after the whole set has been
    // tried — with every branch unresolvable, every branch is still attempted and
    // each failure comes back.
    #[test]
    fn test_load_planned_branches_reports_every_failure_after_trying_all() {
        let plan = vec![planned_branch(DEAD, 0), planned_branch(LIVE, 40)];
        let resolved = RefCell::new(Vec::new());

        let report = load_planned_branches(
            &plan,
            |branch| {
                resolved.borrow_mut().push(branch.sink.clone());
                Err(AudioError::PipeWire(format!(
                    "no PipeWire sink for prefix {}",
                    branch.sink
                )))
            },
            |_branch, _real_sink| Ok(()),
        );

        assert_eq!(
            *resolved.borrow(),
            vec![DEAD.to_string(), LIVE.to_string()],
            "the first failure must not end the pass"
        );
        assert!(report.loaded.is_empty());
        assert_eq!(
            report.failures.len(),
            2,
            "one message per branch that could not be loaded"
        );
    }

    fn names(sinks: &[&str]) -> Vec<String> {
        sinks.iter().map(|s| s.to_string()).collect()
    }

    // Criterion: a branch is confirmed once, at the first pass at least
    // `CONFIRM_GAP` after its load — never in the pass that loaded it. The near
    // miss: one millisecond short of the gap, as when the app sends a
    // selection then a play a moment later (#75: a reload a few milliseconds
    // after the load broke a start that worked).
    #[test]
    fn test_confirmation_register_owes_a_branch_only_once_the_gap_has_passed() {
        let mut register = ConfirmationRegister::default();
        let loaded = Instant::now();
        register.arm(&names(&["bluez_output.B"]), loaded);

        assert!(
            register.take_due(loaded).is_empty(),
            "not in the loading pass"
        );
        assert!(
            register
                .take_due(loaded + CONFIRM_GAP - Duration::from_millis(1))
                .is_empty(),
            "not a moment short of the gap"
        );
        assert_eq!(
            register.take_due(loaded + CONFIRM_GAP),
            names(&["bluez_output.B"])
        );
        assert!(
            register.take_due(loaded + CONFIRM_GAP * 3).is_empty(),
            "and only once: the confirming reload does not arm itself"
        );
    }

    // Criterion: every branch a pass loads is armed, and two branches loaded
    // together are both owed — neither one's reload is lost to the other's.
    #[test]
    fn test_confirmation_register_owes_every_branch_loaded_together() {
        let mut register = ConfirmationRegister::default();
        let loaded = Instant::now();
        register.arm(&names(&["bluez_output.A", "bluez_output.B"]), loaded);

        assert_eq!(
            register.take_due(loaded + CONFIRM_GAP),
            names(&["bluez_output.A", "bluez_output.B"])
        );
    }

    // Criterion: each branch waits for its own gap. A branch loaded later is
    // still waiting when an earlier one is owed; and loading a branch again
    // restarts its wait rather than keeping the older arming.
    #[test]
    fn test_confirmation_register_times_each_branch_from_its_own_load() {
        let mut register = ConfirmationRegister::default();
        let first = Instant::now();
        let later = first + Duration::from_secs(3);
        register.arm(&names(&["bluez_output.A", "bluez_output.B"]), first);
        register.arm(&names(&["bluez_output.B"]), later);

        assert_eq!(
            register.take_due(first + CONFIRM_GAP),
            names(&["bluez_output.A"]),
            "B was loaded again three seconds later: it waits"
        );
        assert_eq!(
            register.take_due(later + CONFIRM_GAP),
            names(&["bluez_output.B"])
        );
    }

    // Criterion: a register nothing was armed in never owes a reload however
    // long it runs, and an empty name arms nothing — it would name every sink
    // to a prefix match.
    #[test]
    fn test_confirmation_register_owes_nothing_it_was_not_armed_with() {
        let mut register = ConfirmationRegister::default();
        let start = Instant::now();
        register.arm(&names(&[""]), start);

        for tick in 0..10 {
            assert!(
                register.take_due(start + CONFIRM_GAP * tick).is_empty(),
                "tick {tick} owes nothing"
            );
        }
    }

    // Criterion: the gap is at least the lower bound measured in #75 — five
    // seconds between the load and the reload repaired the speaker.
    #[test]
    fn test_confirm_gap_is_at_least_the_measured_five_seconds() {
        assert!(CONFIRM_GAP >= Duration::from_secs(5));
    }

    // Criterion: clearing the register — a graph built from nothing — forgets
    // every reload armed before.
    #[test]
    fn test_confirmation_register_clear_forgets_every_armed_reload() {
        let mut register = ConfirmationRegister::default();
        let loaded = Instant::now();
        register.arm(&names(&["bluez_output.A"]), loaded);

        register.clear();

        assert!(register.take_due(loaded + CONFIRM_GAP).is_empty());
    }

    // ─── #80: the confirmation timer's due time ──────────────────────────────

    // Criterion: `next_due` is `None` while nothing is armed — a fresh
    // register, and one armed only with an empty name, which arms nothing.
    // The control: a named sink armed makes it `Some`.
    #[test]
    fn test_confirmation_register_next_due_is_none_when_nothing_is_armed() {
        let mut register = ConfirmationRegister::default();
        let loaded = Instant::now();
        assert_eq!(register.next_due(), None, "a fresh register");

        register.arm(&names(&[""]), loaded);
        assert_eq!(register.next_due(), None, "an empty name arms nothing");

        register.arm(&names(&["bluez_output.A"]), loaded);
        assert!(
            register.next_due().is_some(),
            "control: a named sink is armed"
        );
    }

    // Criterion: `next_due` is the **earliest** armed time plus `CONFIRM_GAP`,
    // not the first armed: here A is armed first but later in time, so an
    // answer read from the arming order is off by three seconds. Arming B
    // again restarts its wait, and the earliest becomes A's.
    #[test]
    fn test_confirmation_register_next_due_is_the_earliest_armed_plus_the_gap() {
        let mut register = ConfirmationRegister::default();
        let start = Instant::now();
        register.arm(&names(&["bluez_output.A"]), start + Duration::from_secs(3));
        register.arm(&names(&["bluez_output.B"]), start);

        assert_eq!(register.next_due(), Some(start + CONFIRM_GAP));

        register.arm(&names(&["bluez_output.B"]), start + Duration::from_secs(4));
        assert_eq!(
            register.next_due(),
            Some(start + Duration::from_secs(3) + CONFIRM_GAP),
            "B's wait restarted, so A's reload comes first"
        );
    }

    // Criterion: once a due sink is taken, `next_due` moves on to the next one
    // still armed, and to `None` once every reload was handed out — a
    // confirming reload does not arm itself.
    #[test]
    fn test_confirmation_register_next_due_moves_on_once_a_due_sink_is_taken() {
        let mut register = ConfirmationRegister::default();
        let first = Instant::now();
        let second = first + Duration::from_secs(2);
        register.arm(&names(&["bluez_output.A"]), first);
        register.arm(&names(&["bluez_output.B"]), second);

        assert_eq!(
            register.take_due(first + CONFIRM_GAP),
            names(&["bluez_output.A"])
        );
        assert_eq!(register.next_due(), Some(second + CONFIRM_GAP));

        assert_eq!(
            register.take_due(second + CONFIRM_GAP),
            names(&["bluez_output.B"])
        );
        assert_eq!(register.next_due(), None);
    }

    // Criterion (guard, the gap does not follow the tick): `CONFIRM_GAP` is
    // 5 s while the safety net ticks every 30 s. A derivation left in place
    // would make the gap 30 s and delay #81's remedy sixfold.
    #[test]
    fn test_confirm_gap_stays_five_seconds_whatever_the_tick() {
        assert_eq!(CONFIRM_GAP, Duration::from_secs(5));
        assert_ne!(
            CONFIRM_GAP, SAFETY_NET_TICK,
            "the gap no longer derives from the tick"
        );
    }

    // Criterion: the safety net ticks every 30 s, longer than the gap: the
    // confirmation has a timer of its own and never waits for the net.
    #[test]
    fn test_safety_net_tick_is_thirty_seconds_and_longer_than_the_gap() {
        assert_eq!(SAFETY_NET_TICK, Duration::from_secs(30));
        assert!(SAFETY_NET_TICK > CONFIRM_GAP);
    }

    // ─── #80: which events wake a repair pass ────────────────────────────────

    /// The JBL Xtreme 3 and the WH-1000XM5, as #81's manual verification and a
    /// live `pw-dump` (2026-09-26/27) named them.
    const JBL: &str = "2C:FD:B4:D3:AC:21";
    const JBL_SINK: &str = "bluez_output.2C_FD_B4_D3_AC_21.1";
    const SONY: &str = "80:99:E7:63:50:29";
    const SONY_SINK: &str = "bluez_output.80_99_E7_63_50_29.1";

    fn selected(macs: &[&str]) -> Vec<SpeakerTarget> {
        macs.iter()
            .map(|mac| SpeakerTarget {
                address: mac.to_string(),
                offset_ms: 0,
            })
            .collect()
    }

    fn appeared(name: &str, at: Instant) -> GraphEvent {
        GraphEvent::SinkAppeared {
            name: name.to_string(),
            at,
        }
    }

    fn vanished(name: &str, at: Instant) -> GraphEvent {
        GraphEvent::SinkVanished {
            name: name.to_string(),
            at,
        }
    }

    // Criterion: a sink event wakes a pass when a selected speaker's
    // `bluez_sink_prefix` names that sink — both ways, appearing and
    // vanishing. The address is mapped through `bluez_sink_prefix`, so a
    // selection holding it in lower case still names the upper-case node.
    #[test]
    fn test_wake_for_wakes_for_a_selected_speakers_sink() {
        let at = Instant::now();
        let selection = selected(&[SONY, JBL]);

        assert!(wake_for(&appeared(JBL_SINK, at), &selection).is_some());
        assert_eq!(
            wake_for(&vanished(SONY_SINK, at), &selection),
            Some(PassReason::SinkVanished {
                name: SONY_SINK.to_string()
            })
        );
        assert!(
            wake_for(&appeared(JBL_SINK, at), &selected(&["2c:fd:b4:d3:ac:21"])).is_some(),
            "a lower-case address names the same sink"
        );
    }

    // Criterion (non-nominal): an event for a sink no selected speaker names —
    // an unselected speaker, the PC's own output — wakes nothing. The control:
    // the selected speaker's own sink, in the same selection, wakes.
    #[test]
    fn test_wake_for_ignores_a_sink_no_selected_speaker_names() {
        let at = Instant::now();
        let selection = selected(&[SONY]);
        assert!(
            wake_for(&appeared(SONY_SINK, at), &selection).is_some(),
            "control: the selected speaker's sink wakes"
        );

        assert_eq!(wake_for(&appeared(JBL_SINK, at), &selection), None);
        assert_eq!(wake_for(&vanished(JBL_SINK, at), &selection), None);
        assert_eq!(
            wake_for(
                &appeared("alsa_output.pci-0000_00_1f.3.analog-stereo", at),
                &selection
            ),
            None
        );
    }

    // Criterion (guard, exact sink, never a longer address): a selection
    // holding `AA:BB:CC:DD:EE:01` does not wake for a sink whose address only
    // starts the same. Both near misses pass a bare `starts_with` of the
    // prefix; only #81's `.` boundary rejects them. The controls: the exact
    // prefix and the prefix followed by `.` wake.
    #[test]
    fn test_wake_for_does_not_take_a_longer_address_for_a_selected_one() {
        let at = Instant::now();
        let selection = selected(&["AA:BB:CC:DD:EE:01"]);
        assert!(wake_for(
            &appeared("bluez_output.AA_BB_CC_DD_EE_01.1", at),
            &selection
        )
        .is_some());
        assert!(wake_for(&appeared("bluez_output.AA_BB_CC_DD_EE_01", at), &selection).is_some());

        for longer in [
            "bluez_output.AA_BB_CC_DD_EE_01_02.1",
            "bluez_output.AA_BB_CC_DD_EE_010.1",
        ] {
            assert_eq!(
                wake_for(&appeared(longer, at), &selection),
                None,
                "{longer} is another speaker"
            );
            assert_eq!(wake_for(&vanished(longer, at), &selection), None);
        }
    }

    // Criterion (guard, the empty value): an empty selection wakes nothing —
    // no sink event, and not `Reconnected` either: with nothing selected the
    // pass has nothing to repair. The control: the same events with a
    // selection wake.
    #[test]
    fn test_wake_for_of_an_empty_selection_wakes_nothing() {
        let at = Instant::now();
        let events = [
            appeared(SONY_SINK, at),
            vanished(SONY_SINK, at),
            GraphEvent::Reconnected,
        ];

        for event in &events {
            assert!(
                wake_for(event, &selected(&[SONY])).is_some(),
                "control: {event:?} wakes with a selection"
            );
            assert_eq!(
                wake_for(event, &[]),
                None,
                "{event:?} with nothing selected"
            );
        }
    }

    // Criterion: `Reconnected` always wakes a pass, whichever speakers are
    // selected — the registry was re-read, so every branch may be gone.
    #[test]
    fn test_wake_for_always_wakes_on_reconnected() {
        for selection in [selected(&[SONY]), selected(&[JBL]), selected(&[SONY, JBL])] {
            assert_eq!(
                wake_for(&GraphEvent::Reconnected, &selection),
                Some(PassReason::Reconnected)
            );
        }
    }

    // Criterion: the reason carries the event's sink name and time, so the
    // pass line can say what woke it and how long after. Two speakers are
    // selected, so a name taken from the selection instead of the event shows.
    #[test]
    fn test_wake_for_carries_the_sink_name_and_time() {
        let at = Instant::now();
        let selection = selected(&[SONY, JBL]);

        assert_eq!(
            wake_for(&appeared(JBL_SINK, at), &selection),
            Some(PassReason::SinkAppeared {
                name: JBL_SINK.to_string(),
                at
            })
        );
        assert_eq!(
            wake_for(&vanished(JBL_SINK, at), &selection),
            Some(PassReason::SinkVanished {
                name: JBL_SINK.to_string()
            })
        );
    }

    // Criterion (non-nominal, `restore_during_playback = false`): a speaker
    // that dropped off during playback stays in the intent but is not
    // re-selected, so its returning sink wakes nothing and it does not start
    // playing under the user's hands. The near miss: the speaker is in the
    // intent — a filter read from the intent would wake. The control: the
    // speaker still selected wakes.
    #[test]
    fn test_wake_for_ignores_a_returning_speaker_left_out_of_the_selection() {
        let at = Instant::now();
        let both = vec![SONY.to_string(), JBL.to_string()];
        let mut targets = crate::targets::SpeakerTargets::new();
        targets.select(SONY, &both).unwrap();
        targets.select(JBL, &both).unwrap();
        // The JBL is switched off: pruned from the selection, kept in the intent.
        targets.retain_connected(&[SONY.to_string()]);
        assert!(targets.intended().contains(&JBL.to_string()));

        assert_eq!(wake_for(&appeared(JBL_SINK, at), &targets.speakers()), None);
        assert!(
            wake_for(&appeared(SONY_SINK, at), &targets.speakers()).is_some(),
            "control: the speaker still selected wakes"
        );
    }

    // Criterion: every pass logs what woke it — a sink appeared or vanished
    // with its name, the confirmation came due, the safety net, or a
    // reconnection. Five reasons, five distinct renderings, and the sink
    // events name their sink.
    #[test]
    fn test_pass_reason_names_what_woke_the_pass() {
        let reasons = [
            PassReason::SinkAppeared {
                name: JBL_SINK.to_string(),
                at: Instant::now(),
            },
            PassReason::SinkVanished {
                name: SONY_SINK.to_string(),
            },
            PassReason::ConfirmationDue,
            PassReason::SafetyNet,
            PassReason::Reconnected,
        ];
        let lines: Vec<String> = reasons.iter().map(|r| r.to_string()).collect();

        assert!(lines[0].contains(JBL_SINK), "got {:?}", lines[0]);
        assert!(lines[1].contains(SONY_SINK), "got {:?}", lines[1]);
        for (i, line) in lines.iter().enumerate() {
            assert!(!line.is_empty(), "reason {i} renders as nothing");
            for other in &lines[i + 1..] {
                assert_ne!(line, other, "two reasons read the same");
            }
        }
    }

    // ─── #139: the combined sink removed from outside ────────────────────────

    const COMBINED_SINK: &str = "blue2th_combined";

    fn combined_vanished(at: Instant) -> GraphEvent {
        GraphEvent::CombinedSinkVanished {
            name: COMBINED_SINK.to_string(),
            at,
        }
    }

    fn combined_reason() -> PassReason {
        PassReason::CombinedSinkVanished {
            name: COMBINED_SINK.to_string(),
        }
    }

    // Criterion (#139): `wake_for(CombinedSinkVanished, selection)` is
    // `Some(PassReason::CombinedSinkVanished)` carrying the sink's name, for
    // any non-empty selection — the combined sink is no speaker's sink, so no
    // speaker's prefix has to name it.
    #[test]
    fn test_wake_for_wakes_for_the_combined_sink_s_removal_with_a_selection() {
        let at = Instant::now();
        for selection in [selected(&[SONY]), selected(&[JBL]), selected(&[SONY, JBL])] {
            assert_eq!(
                wake_for(&combined_vanished(at), &selection),
                Some(combined_reason())
            );
        }
    }

    // Criterion (#139, guard, the empty value): with nothing selected, the
    // combined sink's removal wakes nothing — there is nothing to rebuild, and
    // nothing to pause for. The control: one selected speaker, and it wakes.
    #[test]
    fn test_wake_for_the_combined_sink_s_removal_with_nothing_selected_wakes_nothing() {
        let at = Instant::now();
        assert!(
            wake_for(&combined_vanished(at), &selected(&[JBL])).is_some(),
            "control: a selection wakes"
        );

        assert_eq!(wake_for(&combined_vanished(at), &[]), None);
    }

    // Criterion (#139, guard, the reason is not lost in a burst): wherever the
    // combined sink's removal sits in a drained burst — after a selected
    // speaker's sink appearing, which a first-wins fold would keep, before
    // it, or in the middle of three others that each wake on their own — the
    // one pass carries its reason.
    #[test]
    fn test_wake_for_burst_names_the_combined_sink_s_removal_wherever_it_sits() {
        let at = Instant::now();
        let selection = selected(&[JBL]);
        let bursts = [
            vec![appeared(JBL_SINK, at), combined_vanished(at)],
            vec![combined_vanished(at), appeared(JBL_SINK, at)],
            vec![
                vanished(JBL_SINK, at),
                GraphEvent::Reconnected,
                combined_vanished(at),
                appeared(JBL_SINK, at),
            ],
            vec![combined_vanished(at)],
        ];

        for burst in &bursts {
            assert_eq!(
                wake_for_burst(burst, &selection),
                Some(combined_reason()),
                "burst {burst:?}"
            );
        }
    }

    // Criterion (#139): without the combined sink's removal, a burst keeps
    // today's rule — the first event that wakes names the pass. The
    // unselected speaker's sink, first in the burst, wakes nothing and is
    // skipped.
    #[test]
    fn test_wake_for_burst_without_the_combined_sink_s_removal_keeps_the_first_reason() {
        let at = Instant::now();
        let burst = [
            appeared(SONY_SINK, at),
            appeared(JBL_SINK, at),
            GraphEvent::Reconnected,
        ];

        assert_eq!(
            wake_for_burst(&burst, &selected(&[JBL])),
            Some(PassReason::SinkAppeared {
                name: JBL_SINK.to_string(),
                at
            })
        );
    }

    // Criterion (#139, guard, the empty value): with nothing selected a burst
    // carrying the combined sink's removal wakes nothing, and an empty burst
    // wakes nothing whatever is selected.
    #[test]
    fn test_wake_for_burst_of_an_empty_selection_or_burst_wakes_nothing() {
        let at = Instant::now();
        let burst = [appeared(JBL_SINK, at), combined_vanished(at)];

        assert_eq!(wake_for_burst(&burst, &[]), None);
        assert_eq!(wake_for_burst(&[], &selected(&[JBL])), None);
    }

    // Criterion (#139): `PassReason::CombinedSinkVanished` renders a line of
    // its own naming the combined sink — not the line a speaker's sink
    // vanishing reads, even for the same name, and none of the others.
    #[test]
    fn test_pass_reason_of_the_combined_sink_s_removal_names_the_sink_and_reads_apart() {
        let line = combined_reason().to_string();

        assert!(line.contains(COMBINED_SINK), "got {line:?}");
        let others = [
            PassReason::SinkAppeared {
                name: COMBINED_SINK.to_string(),
                at: Instant::now(),
            },
            PassReason::SinkVanished {
                name: COMBINED_SINK.to_string(),
            },
            PassReason::ConfirmationDue,
            PassReason::SafetyNet,
            PassReason::Reconnected,
        ];
        for other in &others {
            assert_ne!(line, other.to_string(), "reads like {other:?}");
        }
    }

    /// Every reason but the combined sink's removal.
    fn other_reasons() -> Vec<PassReason> {
        vec![
            PassReason::SinkAppeared {
                name: JBL_SINK.to_string(),
                at: Instant::now(),
            },
            PassReason::SinkVanished {
                name: JBL_SINK.to_string(),
            },
            PassReason::ConfirmationDue,
            PassReason::SafetyNet,
            PassReason::Reconnected,
        ]
    }

    // Criterion (#139): a pass woken by the combined sink's removal whose
    // route failed falls back to the pause, whatever the re-targeting said —
    // a failed build may never have reached it.
    #[test]
    fn test_fallback_pause_due_after_the_combined_sink_s_removal_when_the_route_fails() {
        assert!(fallback_pause_due(&combined_reason(), false, true));
        assert!(fallback_pause_due(&combined_reason(), false, false));
    }

    // Criterion (#139): a pass woken by the combined sink's removal whose
    // route succeeded but whose re-targeting failed falls back too: the sink
    // is back, the music is not on it.
    #[test]
    fn test_fallback_pause_due_after_the_combined_sink_s_removal_when_the_retarget_fails() {
        assert!(fallback_pause_due(&combined_reason(), true, false));
    }

    // Criterion (#139, guard, only on failure): a pass woken by the combined
    // sink's removal that rebuilt and re-targeted never pauses. The control:
    // the same reason with the route failed does.
    #[test]
    fn test_fallback_pause_due_never_once_the_rebuild_and_the_retarget_succeeded() {
        assert!(
            fallback_pause_due(&combined_reason(), false, true),
            "control: a failed route falls back"
        );

        assert!(!fallback_pause_due(&combined_reason(), true, true));
    }

    // Criterion (#139, guard, only this reason pauses): a pass woken by any
    // other reason never falls back, whichever of the route and the
    // re-targeting failed. The control: the combined sink's removal with the
    // same failed route does.
    #[test]
    fn test_fallback_pause_due_never_for_another_reason() {
        assert!(
            fallback_pause_due(&combined_reason(), false, true),
            "control: the combined sink's removal falls back"
        );

        for reason in &other_reasons() {
            for (routed_ok, retargeted_ok) in
                [(false, true), (false, false), (true, false), (true, true)]
            {
                assert!(
                    !fallback_pause_due(reason, routed_ok, retargeted_ok),
                    "{reason:?} with routed_ok={routed_ok} retargeted_ok={retargeted_ok}"
                );
            }
        }
    }

    // Criterion: with every selected speaker switched off, nothing is reachable —
    // and the reconciliation asks for no rebuild at all rather than for the whole
    // selection. Asking would spawn a load per tick for nodes that do not exist.
    #[test]
    fn test_reconcile_branches_with_every_speaker_switched_off_loads_nothing() {
        let spec = two_speaker_spec();
        let none_present = CombineSinkSpec {
            sink_name: spec.sink_name.clone(),
            branches: reachable_branches(&spec.branches, &sinks_without_speaker()),
        };
        let loaded = vec![CombineBranch {
            sink: "bluez_output.80_99_E7_63_50_29.1".to_string(),
            latency_ms: 0,
        }];

        assert!(
            none_present.branches.is_empty(),
            "neither speaker has a node in this listing"
        );

        let plan = reconcile_branches(&loaded, &none_present);

        assert_eq!(
            plan,
            BranchPlan {
                to_load: Vec::new(),
                to_retune: Vec::new(),
                to_unload: loaded,
            },
            "nothing is loaded onto a node that is not there; the branch left \
             over from the speaker that is now off is dropped"
        );
    }

    /// What a graph loaded from [`two_speaker_spec`] reports: one branch per
    /// speaker on the **resolved** node, each at the latency the plan asked for.
    fn two_speakers_loaded() -> Vec<CombineBranch> {
        vec![
            CombineBranch {
                sink: "bluez_output.80_99_E7_63_50_29.1".to_string(),
                latency_ms: 0,
            },
            CombineBranch {
                sink: "bluez_output.11_22_33_44_55_66.1".to_string(),
                latency_ms: 250,
            },
        ]
    }

    // Criterion (moved back from the graph module of (#78)): what the graph
    // reports loaded and what the plan asks for are the same quantity — the
    // offset, with no base. Adding anything at load time would have every
    // reconciliation see a mismatch and retune every branch on every tick.
    #[test]
    fn test_branch_latency_round_trips_from_the_plan_through_the_loaded_branches() {
        let spec = combine_sink_plan(&[
            SpeakerTarget {
                address: "80:99:E7:63:50:29".to_string(),
                offset_ms: 0,
            },
            SpeakerTarget {
                address: "11:22:33:44:55:66".to_string(),
                offset_ms: 70,
            },
        ]);
        // What the graph reports back for a graph loaded from this very plan: one
        // branch per planned one, on the resolved node, at the planned latency.
        let loaded: Vec<CombineBranch> = spec
            .branches
            .iter()
            .map(|branch| CombineBranch {
                sink: format!("{}.1", branch.sink),
                latency_ms: branch.latency_ms,
            })
            .collect();

        assert_eq!(
            loaded.iter().map(|b| b.latency_ms).collect::<Vec<_>>(),
            vec![0, 70],
            "the plan carries the offsets as they are, and so do the loaded branches"
        );
        assert_eq!(
            reconcile_branches(&loaded, &spec),
            BranchPlan::default(),
            "a graph loaded from the plan reconciles against it as a no-op"
        );
    }

    // Criterion (moved back): a speaker present in both the loaded set and the
    // spec with the same latency appears in neither list — an unchanged
    // selection touches nothing, which is what keeps the stream alive.
    #[test]
    fn test_reconcile_branches_unchanged_selection_changes_nothing() {
        let spec = two_speaker_spec();

        let plan = reconcile_branches(&two_speakers_loaded(), &spec);

        assert_eq!(plan, BranchPlan::default());
    }

    // Criterion (moved back): a speaker dropped from the selection yields exactly
    // one unload, naming the resolved node its branch feeds, and never the null
    // sink.
    #[test]
    fn test_reconcile_branches_dropped_speaker_unloads_only_that_branch() {
        let spec = combine_sink_plan(&[SpeakerTarget {
            address: "80:99:E7:63:50:29".to_string(),
            offset_ms: 0,
        }]);

        let plan = reconcile_branches(&two_speakers_loaded(), &spec);

        assert_eq!(
            plan,
            BranchPlan {
                to_load: Vec::new(),
                to_retune: Vec::new(),
                to_unload: vec![CombineBranch {
                    sink: "bluez_output.11_22_33_44_55_66.1".to_string(),
                    latency_ms: 250,
                }],
            },
            "only the deselected speaker's branch is unloaded"
        );
        assert!(
            !plan
                .to_unload
                .iter()
                .any(|b| b.sink.contains(&spec.sink_name)),
            "the null sink is never unloaded by a selection change: {:?}",
            plan.to_unload
        );
    }

    // Criterion (guard, retune, never reload): a loaded branch for the same
    // speaker at another delay is retuned in place — it goes to `to_retune`,
    // carrying the resolved node and the planned delay, never to `to_unload`
    // plus `to_load`. The other speaker is untouched.
    #[test]
    fn test_reconcile_branches_offset_change_retunes_in_place() {
        let spec = combine_sink_plan(&[
            SpeakerTarget {
                address: "80:99:E7:63:50:29".to_string(),
                offset_ms: 0,
            },
            SpeakerTarget {
                address: "11:22:33:44:55:66".to_string(),
                offset_ms: 400,
            },
        ]);

        let plan = reconcile_branches(&two_speakers_loaded(), &spec);

        assert_eq!(
            plan,
            BranchPlan {
                to_load: Vec::new(),
                to_retune: vec![CombineBranch {
                    sink: "bluez_output.11_22_33_44_55_66.1".to_string(),
                    latency_ms: 400,
                }],
                to_unload: Vec::new(),
            }
        );
    }

    // Criterion (guard, missing branch loads alone): with speaker A loaded and
    // live and speaker B's branch missing — dropped by the router once ruled
    // dead — the plan loads B alone. The #75 rule would have unloaded A too.
    #[test]
    fn test_reconcile_branches_missing_branch_loads_it_alone() {
        let spec = two_speaker_spec();
        let a = CombineBranch {
            sink: "bluez_output.80_99_E7_63_50_29.1".to_string(),
            latency_ms: 0,
        };

        let plan = reconcile_branches(&[a], &spec);

        assert_eq!(
            plan,
            BranchPlan {
                to_load: vec![CombineBranch {
                    sink: "bluez_output.11_22_33_44_55_66".to_string(),
                    latency_ms: 250,
                }],
                to_retune: Vec::new(),
                to_unload: Vec::new(),
            }
        );
    }

    // Criterion: the three sets are disjoint — every speaker lands in exactly
    // one of them, or in none when its branch already matches: one to load,
    // one to retune, one to unload and one left alone, in the same pass.
    #[test]
    fn test_reconcile_branches_sorts_each_speaker_into_exactly_one_set() {
        let spec = combine_sink_plan(&[
            SpeakerTarget {
                address: "AA:BB:CC:DD:EE:01".to_string(),
                offset_ms: 100,
            },
            SpeakerTarget {
                address: "AA:BB:CC:DD:EE:02".to_string(),
                offset_ms: 30,
            },
            SpeakerTarget {
                address: "AA:BB:CC:DD:EE:03".to_string(),
                offset_ms: 60,
            },
        ]);
        let loaded = vec![
            CombineBranch {
                sink: "bluez_output.AA_BB_CC_DD_EE_01.1".to_string(),
                latency_ms: 0,
            },
            CombineBranch {
                sink: "bluez_output.AA_BB_CC_DD_EE_03.1".to_string(),
                latency_ms: 60,
            },
            CombineBranch {
                sink: "bluez_output.AA_BB_CC_DD_EE_04.1".to_string(),
                latency_ms: 30,
            },
        ];

        let plan = reconcile_branches(&loaded, &spec);

        assert_eq!(
            plan,
            BranchPlan {
                to_load: vec![CombineBranch {
                    sink: "bluez_output.AA_BB_CC_DD_EE_02".to_string(),
                    latency_ms: 30,
                }],
                to_retune: vec![CombineBranch {
                    sink: "bluez_output.AA_BB_CC_DD_EE_01.1".to_string(),
                    latency_ms: 100,
                }],
                to_unload: vec![CombineBranch {
                    sink: "bluez_output.AA_BB_CC_DD_EE_04.1".to_string(),
                    latency_ms: 30,
                }],
            }
        );
    }
}

/// [`AudioRouter`] driven through the in-memory graph. Every assertion is made on
/// the calls the graph recorded, against literals: two values the code computed
/// compare equal when both are absent.
#[cfg(test)]
mod router_tests {
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

    // Non-nominal: the sink list stops being readable in the middle of a pass —
    // the reconciliation returns `Ok(())` without unloading anything.
    #[test]
    fn test_route_with_a_sink_list_lost_mid_pass_returns_ok_and_unloads_nothing() {
        let (fake, a, b) = steady_graph(Some(true));
        fake.fail_after(GraphOp::Sinks, 1);
        let mut router = router_on(&fake);

        let result = router.route_for_targets(&steady_selection());

        assert!(result.is_ok(), "route failed: {result:?}");
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

        assert!(!router.combined_sink_exists(""));
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
    // namesake sharing the opening characters is not the combined sink, and an
    // unreadable list is not "exists".
    #[test]
    fn test_combined_sink_exists_reads_the_graph() {
        let present = FakeGraph::with_sinks(&[SINK_A, COMBINED]);
        assert!(router_on(&present).combined_sink_exists(COMBINED));

        let namesake = FakeGraph::with_sinks(&[SINK_A, "blue2th_combined_old"]);
        assert!(!router_on(&namesake).combined_sink_exists(COMBINED));

        let unreadable = FakeGraph::with_sinks(&[COMBINED]);
        unreadable.fail(GraphOp::Sinks);
        assert!(!router_on(&unreadable).combined_sink_exists(COMBINED));
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
        assert!(!router.combined_sink_exists(""));

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

    // Criterion (#145): a sink that is listed but whose level cannot be read
    // is `None` in its own slot, like an absent one, and the other speaker is
    // still read — `Graph::sink_volume` gives no reason to stop there.
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
}
