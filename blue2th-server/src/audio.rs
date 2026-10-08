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
// `Clone` (#147): one volume read answers every caller queued for it.
// `PartialEq, Eq` (#147): a `BranchLoadReport` holding them keeps its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioError {
    /// No speaker is connected, so playback cannot be routed anywhere.
    NoSpeakerConnected,
    /// The PipeWire daemon is unreachable or rejected the request.
    PipeWire(String),
    /// The graph thread took the command out of its queue too late and did
    /// not run it (#146). Only that thread says so: it is the one that knows
    /// the command was never started.
    Expired,
    /// The graph thread started the command and PipeWire did not answer
    /// before its deadline.
    Unanswered,
}

impl std::fmt::Display for AudioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AudioError::NoSpeakerConnected => write!(f, "no speaker connected"),
            AudioError::PipeWire(msg) => write!(f, "PipeWire error: {msg}"),
            AudioError::Expired => {
                write!(f, "the audio graph did not start the command in time")
            },
            AudioError::Unanswered => {
                write!(f, "PipeWire did not answer the command before its deadline")
            },
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
/// the error of each one that was not.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct BranchLoadReport {
    /// The `sink` prefix of each branch that was loaded, in plan order.
    pub loaded: Vec<String>,
    /// One error per branch that could not be resolved or loaded.
    pub failures: Vec<AudioError>,
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
                report.failures.push(err);
                continue;
            },
        };
        match load(branch, &resolved) {
            // Cloned because the report outlives the borrowed plan.
            Ok(()) => report.loaded.push(branch.sink.clone()),
            Err(err) => report.failures.push(err),
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
        // A daemon that stalled on one branch is not answering, whatever the
        // others said (#147): the stall decides the status, and the refusals
        // beside it stay in the log.
        if self.failures.contains(&AudioError::Unanswered) {
            for failure in self
                .failures
                .iter()
                .filter(|f| **f != AudioError::Unanswered)
            {
                tracing::warn!("branch pass: {failure}");
            }
            return Err(AudioError::Unanswered);
        }
        // A refusal's own text, not its `Display`: the joined error is a
        // `PipeWire` whose `Display` names it as one, once.
        let messages: Vec<String> = self
            .failures
            .into_iter()
            .map(|failure| match failure {
                AudioError::PipeWire(message) => message,
                other => other.to_string(),
            })
            .collect();
        Err(AudioError::PipeWire(messages.join("; ")))
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
///
/// Generic over the graph it owns (#147): the loop thread's router owns the
/// loop's own state, which is not `Send`, while a router that crosses threads
/// — the default — owns a `dyn Graph + Send`.
pub struct AudioRouter<G: Graph + ?Sized = dyn Graph + Send> {
    /// The graph every routing decision is read from and applied to.
    graph: Box<G>,
    /// The branches owed a confirming reload, and since when. See
    /// [`CONFIRM_GAP`].
    confirmation: ConfirmationRegister,
    /// What "now" is for the confirmation; a test drives it by hand.
    clock: Box<dyn Fn() -> Instant + Send>,
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
    pub fn new(graph: Box<dyn Graph + Send>) -> Self {
        Self::over(graph, Box::new(Instant::now))
    }

    /// A router over `graph` whose confirmation reads the time from `clock`.
    #[cfg(test)]
    pub(crate) fn with_clock(
        graph: Box<dyn Graph + Send>,
        clock: Box<dyn Fn() -> Instant + Send>,
    ) -> Self {
        Self::over(graph, clock)
    }
}

impl<G: Graph + ?Sized> AudioRouter<G> {
    /// A router owning `graph`, whatever it is, whose confirmation reads the
    /// time from `clock`.
    pub(crate) fn over(graph: Box<G>, clock: Box<dyn Fn() -> Instant + Send>) -> Self {
        Self {
            graph,
            confirmation: ConfirmationRegister::default(),
            clock,
            changes: 0,
            retarget_failed: false,
        }
    }

    /// The graph this router owns, for the thread that runs it: the loop
    /// thread keeps its connection alive through it between two messages.
    pub(crate) fn graph_mut(&mut self) -> &mut G {
        &mut self.graph
    }

    /// Hand the graph the instant past which the calls made from here on stop
    /// waiting for the daemon (#147).
    pub(crate) fn set_deadline(&mut self, deadline: Instant) {
        self.graph.set_deadline(deadline);
    }

    /// When the earliest confirming reload falls due; `None` when none is armed.
    pub fn next_confirmation_due(&self) -> Option<Instant> {
        self.confirmation.next_due()
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
        // The unreadable list ends it with its own error (#152): a stall
        // reported as `Ok(())` leaves nothing to re-apply once the daemon answers.
        let sinks = self.graph.sinks()?;
        if sinks.is_empty() {
            return Ok(());
        }
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
                    Err(err) => failures.push(err),
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
    /// than built from scratch. An `Err` is a sink list that could not be read,
    /// which a caller must not take for an absent sink (#147).
    pub fn combined_sink_exists(&mut self, sink_name: &str) -> Result<bool, AudioError> {
        Ok(self.find_sink_with_prefix(sink_name)?.is_some())
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
    /// (AVRCP). `Err` when that list cannot be read or a speaker's level
    /// read fails (#148); `Ok(None)` for a speaker whose sink is absent from
    /// a list that read fine, or whose sink [`Graph::sink_volume`] answered
    /// has no level.
    ///
    /// A failed read stops there: while the graph does not answer, every
    /// further call would only wait out its own timeout.
    pub fn sink_volumes(&mut self, macs: &[String]) -> Result<Vec<Option<f32>>, AudioError> {
        if macs.is_empty() {
            return Ok(Vec::new());
        }
        let sinks = self.graph.sinks()?;
        macs.iter()
            .map(
                |mac| match sink_named_by_prefix(&sinks, &bluez_sink_prefix(mac)) {
                    Some(sink) => self.graph.sink_volume(&sink),
                    None => Ok(None),
                },
            )
            .collect()
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
fn find_sink_with_prefix<G: Graph + ?Sized>(
    graph: &mut G,
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
fn resolve_branch_sink<G: Graph + ?Sized>(
    graph: &mut G,
    branch: &CombineBranch,
) -> Result<String, AudioError> {
    find_sink_with_prefix(graph, &branch.sink)?
        .ok_or_else(|| AudioError::PipeWire(format!("no PipeWire sink for prefix {}", branch.sink)))
}

#[cfg(test)]
mod tests;

/// [`AudioRouter`] driven through the in-memory graph. Every assertion is made on
/// the calls the graph recorded, against literals: two values the code computed
/// compare equal when both are absent.
#[cfg(test)]
mod router_tests;
