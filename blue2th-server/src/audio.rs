// SPDX-License-Identifier: MIT OR Apache-2.0

//! PC backend audio engine: rodio sink lifecycle (play/pause/stop), PipeWire
//! sink volume, and the shared playback state.
//!
//! Phase 3 uses `rodio` (which decodes via `symphonia`) as a disposable test
//! source; the decoded stream goes to PipeWire, which routes it to the
//! connected speaker's sink. Volume targets the PipeWire sink, not rodio's
//! internal gain, so it is reused unchanged once the source becomes
//! `librespot` in phase 5.
//!
//! Kept behind a small interface (this module) so the audio engine can be
//! swapped out later without touching the route layer.

use std::{
    cell::RefCell,
    io::Cursor,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender},
        Arc,
    },
    thread::JoinHandle,
    time::Duration,
};

use blue2th_proto::{PlaybackState, PlaybackStatus, SpeakerTarget};

use crate::graph::{Graph, LoadedBranch};

/// The embedded test tone shipped with the backend (2s 440Hz stereo sine,
/// 48kHz PCM 16-bit).
pub const TEST_TONE_WAV: &[u8] = include_bytes!("../assets/test-tone.wav");

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
/// whole-percent resolution — the resolution `parse_first_percent` actually
/// reads. The returned level is therefore always in `0.0..=1.0`, as
/// `PlaybackState.volume` documents. Otherwise
/// the last `commanded` level is reported: it is true as a command, and it never
/// presents one speaker's level as if it were everyone's.
pub fn reported_volume(levels: &[Option<f32>], commanded: f32) -> f32 {
    let mut agreed: Option<u32> = None;
    for level in levels {
        // A sink that could not be read makes the selection undecidable: nothing
        // here is known to be true of every speaker. So does one whose level
        // `PlaybackState.volume` cannot express — `NaN`, an infinity, or a value
        // outside `0.0..=1.0` (pactl reports an over-amplified sink as e.g.
        // "153%"). Reporting a clamped 100% there would name a level no speaker
        // is at, which is the defect this rule exists to remove. The range check
        // also keeps the cast below meaningful: `as` saturates, so an unchecked
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
/// actual sound to an implementation of this trait, so the rodio test source can
/// be swapped for `librespot` in phase 5 and so tests can run without an audio
/// device. `Send` is required because the engine lives behind an
/// `Arc<Mutex<_>>` shared across async tasks.
pub trait AudioOutput: Send {
    /// Begin streaming `tone` to the connected speaker's sink (fresh playback).
    fn start(&mut self, tone: &'static [u8]) -> Result<(), AudioError>;
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
    fn start(&mut self, _tone: &'static [u8]) -> Result<(), AudioError> {
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
    /// router uses this with [`RodioOutput`] for real playback.
    pub fn with_output(output: Box<dyn AudioOutput>) -> Self {
        Self {
            status: PlaybackStatus::Stopped,
            volume: 1.0,
            output,
        }
    }

    /// Start (or resume) playback of the embedded test file. Idempotent while
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

    /// If the output finished playing on its own while we still believe we are
    /// `Playing`, fall back to `Stopped`.
    fn reconcile(&mut self) {
        if self.status == PlaybackStatus::Playing && self.output.is_finished() {
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
        }
    }

    // --- Output seam ---------------------------------------------------------
    //
    // These delegate to the pluggable `AudioOutput`. `NullOutput` makes them
    // no-ops (tests, no audio device); `RodioOutput` performs real playback and
    // sets the PipeWire sink volume.

    /// Begin streaming the embedded tone to the connected speaker's sink.
    fn start_output(&mut self) -> Result<(), AudioError> {
        self.output.start(TEST_TONE_WAV)
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
    /// The embedded test file is missing or could not be decoded.
    Decode(String),
    /// The PipeWire daemon is unreachable or rejected the request.
    PipeWire(String),
}

impl std::fmt::Display for AudioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AudioError::NoSpeakerConnected => write!(f, "no speaker connected"),
            AudioError::Decode(msg) => write!(f, "failed to decode audio: {msg}"),
            AudioError::PipeWire(msg) => write!(f, "PipeWire error: {msg}"),
        }
    }
}

impl std::error::Error for AudioError {}

/// Commands sent to the dedicated audio thread. The cpal output stream is
/// `!Send`, so it must stay on a single thread; the engine talks to it over this
/// channel instead of holding it directly.
enum AudioCmd {
    /// Start fresh playback of `tone`; the reply reports whether the device
    /// opened and the tone decoded.
    Play {
        tone: &'static [u8],
        reply: SyncSender<Result<(), String>>,
    },
    Pause,
    Resume,
    Stop,
}

/// Real audio output: streams the decoded tone to the default PipeWire sink via
/// rodio (cpal → ALSA → PipeWire) on a dedicated thread, and sets the sink volume
/// through `wpctl`. The thread and device are created lazily on the first
/// `start`, so constructing this (e.g. when the router is built) never touches an
/// audio device — important for CI / hosts without PipeWire.
#[derive(Default)]
pub struct RodioOutput {
    tx: Option<Sender<AudioCmd>>,
    handle: Option<JoinHandle<()>>,
    /// Set by the audio thread when the current playback reaches its end on its
    /// own; read by `is_finished` so the engine can return to `Stopped`.
    ended: Arc<AtomicBool>,
}

impl RodioOutput {
    /// Create an output that opens no device until the first `start`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Send a command to the audio thread, spawning it lazily on first use and
    /// mapping a dead thread to an error.
    fn send(&mut self, cmd: AudioCmd) -> Result<(), AudioError> {
        if self.tx.is_none() {
            let (tx, rx) = mpsc::channel::<AudioCmd>();
            // Share the end-of-playback flag with the thread.
            let ended = Arc::clone(&self.ended);
            self.handle = Some(std::thread::spawn(move || run_audio_thread(rx, ended)));
            self.tx = Some(tx);
        }
        match &self.tx {
            Some(tx) => tx
                .send(cmd)
                .map_err(|_| AudioError::PipeWire("audio thread is not running".to_string())),
            None => Err(AudioError::PipeWire("audio thread unavailable".to_string())),
        }
    }
}

impl AudioOutput for RodioOutput {
    fn start(&mut self, tone: &'static [u8]) -> Result<(), AudioError> {
        let (reply, reply_rx) = mpsc::sync_channel::<Result<(), String>>(1);
        self.send(AudioCmd::Play { tone, reply })?;
        reply_rx
            .recv()
            .map_err(|_| AudioError::PipeWire("audio thread stopped before replying".to_string()))?
            .map_err(AudioError::Decode)
    }

    fn resume(&mut self) -> Result<(), AudioError> {
        self.send(AudioCmd::Resume)
    }

    fn pause(&mut self) -> Result<(), AudioError> {
        self.send(AudioCmd::Pause)
    }

    fn stop(&mut self) -> Result<(), AudioError> {
        self.send(AudioCmd::Stop)
    }

    fn is_finished(&self) -> bool {
        self.ended.load(Ordering::Relaxed)
    }
}

/// The audio thread: owns the cpal output stream and the current player, and
/// reacts to commands. Exits when the command channel is dropped.
fn run_audio_thread(rx: Receiver<AudioCmd>, ended: Arc<AtomicBool>) {
    let mut device: Option<rodio::MixerDeviceSink> = None;
    let mut player: Option<rodio::Player> = None;

    loop {
        // Poll between commands so the natural end of the tone is detected even
        // while no command arrives.
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(AudioCmd::Play { tone, reply }) => {
                ended.store(false, Ordering::Relaxed);
                // Reopen the output device on each play so it binds to the
                // *current* default sink: the route layer points the default at
                // the combined sink just before calling play.
                if let Some(previous) = player.take() {
                    previous.stop();
                }
                // Drop the previous device so the new one binds to the current
                // default sink (pointed at the combined sink just before this
                // call).
                drop(device.take());
                let result = match rodio::DeviceSinkBuilder::open_default_sink() {
                    Ok(mut dev) => {
                        // The engine controls the stream lifecycle; suppress
                        // rodio's stderr warning when the sink is dropped.
                        dev.log_on_drop(false);
                        let outcome = match rodio::play(dev.mixer(), Cursor::new(tone)) {
                            Ok(p) => {
                                player = Some(p);
                                Ok(())
                            },
                            Err(e) => Err(format!("decode/play tone: {e}")),
                        };
                        device = Some(dev);
                        outcome
                    },
                    Err(e) => Err(format!("open default audio sink: {e}")),
                };
                let _ = reply.send(result);
            },
            Ok(AudioCmd::Pause) => {
                if let Some(p) = &player {
                    p.pause();
                }
            },
            Ok(AudioCmd::Resume) => {
                if let Some(p) = &player {
                    p.play();
                }
            },
            Ok(AudioCmd::Stop) => {
                if let Some(p) = player.take() {
                    p.stop();
                }
                ended.store(false, Ordering::Relaxed);
            },
            Err(RecvTimeoutError::Timeout) => {
                // The tone has played to the end: mark it so the engine returns
                // to Stopped on the next state query.
                if let Some(p) = &player {
                    if p.empty() {
                        ended.store(true, Ordering::Relaxed);
                    }
                }
            },
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// One branch of a PipeWire combined sink: the speaker's `bluez_output.*` sink
/// node name and the per-speaker latency (ms) to apply to that branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CombineBranch {
    /// The speaker's `bluez_output.*` sink node-name prefix (from
    /// [`bluez_sink_prefix`]); the hardware seam resolves it to the live node
    /// (which carries a trailing card suffix, e.g. `.1`).
    pub sink: String,
    /// The `latency_msec` the branch's `module-loopback` is loaded with, in
    /// milliseconds: [`branch_latency_ms`] of the speaker's offset, so it is the
    /// base buffer plus that offset and never the bare offset.
    pub latency_ms: u32,
}

/// The base buffer a branch's loopback is given, in milliseconds, on top of the
/// speaker's own offset (#75).
pub const BASE_BRANCH_LATENCY_MS: u32 = 50;

/// The loopback latency a branch carries for a speaker at `offset_ms`.
pub fn branch_latency_ms(offset_ms: u32) -> u32 {
    BASE_BRANCH_LATENCY_MS.saturating_add(offset_ms)
}

/// Pure plan for a PipeWire combined sink spanning the selected speakers' sinks,
/// each branch carrying [`branch_latency_ms`] of its speaker's offset. Building
/// this performs no I/O; [`AudioRouter`] applies it to its [`Graph`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CombineSinkSpec {
    /// Node name of the combined sink to create.
    pub sink_name: String,
    /// The member branches, one per target speaker.
    pub branches: Vec<CombineBranch>,
}

/// Build the (pure, testable) combined-sink plan for the given targets: each
/// target maps to its `bluez_output.*` sink name and to [`branch_latency_ms`] of
/// its offset. Used by every non-empty selection; performs no I/O.
pub fn combine_sink_plan(targets: &[SpeakerTarget]) -> CombineSinkSpec {
    let branches = targets
        .iter()
        .map(|t| CombineBranch {
            sink: bluez_sink_prefix(&t.address),
            latency_ms: branch_latency_ms(t.offset_ms),
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

/// How often the repair pass looks at the graph. Short enough that a speaker
/// coming back is fed again within seconds, and it reads the graph only while a
/// selection is actually playing.
pub const BRANCH_REPAIR_TICK: Duration = Duration::from_secs(5);

/// What a selection change has to do to an already-loaded combined sink: the
/// branches to load and the loaded ones to unload.
///
/// `to_unload` carries the branches as the graph reported them — i.e. with the
/// **resolved** node name — because that is what [`AudioRouter`] matches against
/// the loaded branches to find the ids to unload, while `to_load` carries the
/// plan's `bluez_output.*` prefixes, which the router resolves at load time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BranchReconciliation {
    /// Branches of the spec that are not loaded as-is and must be loaded.
    pub to_load: Vec<CombineBranch>,
    /// Loaded branches the spec no longer calls for, which must be unloaded.
    pub to_unload: Vec<CombineBranch>,
}

/// Compare the loopbacks currently loaded for a combined sink against the plan
/// and decide what to change, leaving matching branches — and the null sink —
/// alone. Pure; the caller performs the loads and unloads.
pub fn reconcile_branches(
    loaded: &[CombineBranch],
    spec: &CombineSinkSpec,
) -> BranchReconciliation {
    let missing: Vec<CombineBranch> = spec
        .branches
        .iter()
        .filter(|planned| !loaded.iter().any(|up| branch_is(up, planned)))
        .cloned()
        .collect();

    if missing.is_empty() {
        // Nothing has to start, so nothing has to restart: a selection that only
        // lost a speaker leaves the others streaming, untouched.
        return BranchReconciliation {
            to_load: missing,
            to_unload: loaded
                .iter()
                .filter(|up| !spec.branches.iter().any(|planned| branch_is(up, planned)))
                .cloned()
                .collect(),
        };
    }

    // A loopback loaded into a graph that is already running attaches its stream
    // to the right sink and leaves the Bluetooth node silent for good: measured
    // on two speakers, where the one that came back stayed mute with a branch
    // that was present, live and correctly attached, while a stream written
    // straight into its sink was silent too — and both played again the moment
    // every branch was rebuilt in one pass (#75). Speakers start together or not
    // at all, so one missing branch costs a rebuild of the whole selection. That
    // is a brief cut on the speakers already playing, and the alternative is one
    // of them silent until the operator intervenes.
    BranchReconciliation {
        to_load: spec.branches.clone(),
        to_unload: loaded.to_vec(),
    }
}

/// The one-tick delay line that carries the confirming rebuild from the pass that
/// armed it to the next one.
///
/// Split out of `AudioRouter::reconcile_combined` so the transition itself is
/// pure and can be driven tick by tick in a test, without a graph around it.
#[derive(Debug, Default, PartialEq, Eq)]
struct ConfirmationRegister {
    due: bool,
}

impl ConfirmationRegister {
    /// Answer whether *this* pass owes the confirming rebuild, and arm the next
    /// one when `arms`.
    ///
    /// The read happens before the write, and that ordering is the whole
    /// mechanism: rebuilding in the same pass that wired the returning speaker was
    /// measured not to repair it — and to break a start that worked — while
    /// rebuilding one tick later repairs it (#75).
    fn take_and_arm(&mut self, arms: bool) -> bool {
        std::mem::replace(&mut self.due, arms)
    }
}

/// The node names `pactl list short sinks` lists, second column.
pub(crate) fn sink_nodes(sink_listing: &str) -> Vec<String> {
    sink_listing
        .lines()
        .filter_map(|line| line.split('\t').nth(1))
        .filter(|name| !name.is_empty())
        .map(|name| name.to_string())
        .collect()
}

/// The sinks `sink_listing` carries that `previous` did not — the speakers that
/// have come back since the last pass. On the very first pass nothing is new:
/// every sink listed then was there before the server was, which is why a start
/// costs no rebuild.
pub fn newly_listed_sinks(previous: &Option<Vec<String>>, sink_listing: &str) -> Vec<String> {
    let Some(previous) = previous else {
        return Vec::new();
    };
    sink_nodes(sink_listing)
        .into_iter()
        .filter(|node| !previous.iter().any(|seen| seen == node))
        .collect()
}

/// Whether this pass wires a speaker whose sink has just appeared, and so owes the
/// **next** pass a rebuild.
///
/// A `module-loopback` loaded towards a `bluez_output.*` node that has just been
/// created does not start it: the stream attaches to the right sink, the module is
/// live, no error is reported anywhere — and the speaker stays silent for good, so
/// completely that a stream written straight into that sink is silent too.
/// Rebuilding every branch **one tick later** starts it.
///
/// The delay is the point, and it was measured on 2026-09-06 with two speakers.
/// Rebuilding a few milliseconds after the first load does not repair the speaker
/// and *breaks a start that worked*; rebuilding five to seven seconds later
/// repairs it, which is what the operator's habitual deselect/reselect had been
/// doing by hand all along. A single load thirty seconds after the node appeared
/// stays mute, so it is neither a settling delay nor the ordinal of the load: it
/// is the gap between two of them (#75).
pub fn wires_a_new_sink(to_load: &[CombineBranch], fresh: &[String], sink_listing: &str) -> bool {
    to_load.iter().any(|branch| {
        sink_matching_prefix(sink_listing, &branch.sink).is_some_and(|node| fresh.contains(&node))
    })
}

/// The planned branches whose speaker sink is actually present in `sink_listing`.
///
/// A speaker that is switched off has no `bluez_output.*` node, so its branch
/// cannot be loaded however often it is tried. Left in the plan it would keep the
/// reconciliation permanently one branch short, and since a missing branch
/// rebuilds the whole selection, the speakers still playing would be torn down
/// and restarted on every repair tick (#75).
pub fn reachable_branches(branches: &[CombineBranch], sink_listing: &str) -> Vec<CombineBranch> {
    branches
        .iter()
        .filter(|branch| sink_matching_prefix(sink_listing, &branch.sink).is_some())
        .cloned()
        .collect()
}

/// Whether the loaded loopback `up` is exactly what the planned branch asks for:
/// same latency, and a sink the plan's `bluez_output.*` prefix names.
fn branch_is(up: &CombineBranch, planned: &CombineBranch) -> bool {
    up.latency_ms == planned.latency_ms && prefix_names_node(&planned.sink, &up.sink)
}

/// Pick the live PipeWire sink node-name matching `prefix` out of the text
/// `pactl list short sinks` prints: tab-separated columns, the node name in the
/// second one. A `bluez_output.<MAC>` prefix resolves to the line carrying the
/// card suffix (`bluez_output.<MAC>.1`); an already exact node name resolves to
/// itself. Pure — performs no I/O.
///
/// A candidate must either *equal* `prefix` or continue it with a `.`, the
/// separator PipeWire puts before the card index. That boundary is what keeps a
/// sink merely sharing the opening characters (`blue2th_combined_old` for
/// `blue2th_combined`) from being answered instead of the target, and an exact
/// name wins over any longer namesake wherever the two sit in the listing.
///
/// An **empty** prefix matches nothing, explicitly: it starts every name, so the
/// plain `starts_with` this replaces answered the first sink in the listing —
/// the PC's own output, in index order — which is exactly the silent wrong-sink
/// fallback this resolution exists to prevent. `spotify_target_sink(&[])` is
/// empty, so the value is reachable; only the `speakers.is_empty()` guard in
/// `SpotifyBackend::start` kept it away. The boundary rule alone would not do:
/// a line whose node-name column is blank equals the empty prefix.
///
/// Among several `.`-suffixed candidates the first line wins, i.e. `pactl`'s own
/// sink-index order.
pub fn sink_matching_prefix(listing: &str, prefix: &str) -> Option<String> {
    if prefix.is_empty() {
        return None;
    }
    let mut suffixed: Option<&str> = None;
    for name in listing.lines().filter_map(|line| line.split('\t').nth(1)) {
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
/// The single copy of the rule [`sink_matching_prefix`] resolves with and
/// [`branch_is`] compares with, so one set of tests pins both.
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

/// Render sink node names back into the listing shape the planning layer reads:
/// one line per sink, tab-separated, the node name in the second column.
///
/// It exists because [`reachable_branches`], [`newly_listed_sinks`],
/// [`wires_a_new_sink`] and [`sink_matching_prefix`] keep their text signatures
/// while the [`Graph`] hands names over typed; keeping the conversion in one
/// place is what lets those functions stay as they are.
fn render_sink_listing(sinks: &[String]) -> String {
    sinks
        .iter()
        .enumerate()
        .map(|(index, name)| format!("{index}\t{name}\n"))
        .collect()
}

/// The routing logic, driven through a [`Graph`] rather than through `pactl`
/// directly (#79). It owns the two registers the reconciliation carries from one
/// pass to the next, so two routers never see each other's history.
pub struct AudioRouter {
    /// The graph every routing decision is read from and applied to.
    graph: Box<dyn Graph>,
    /// The sink node names listed at the previous reconciliation; `None` until
    /// the first pass. See [`newly_listed_sinks`].
    sinks_last_pass: Option<Vec<String>>,
    /// Whether the previous pass owes this one a rebuild. See
    /// [`wires_a_new_sink`].
    confirmation: ConfirmationRegister,
}

impl AudioRouter {
    /// A router over `graph`, with no history: its first pass treats every
    /// listed sink as already known.
    pub fn new(graph: Box<dyn Graph>) -> Self {
        Self {
            graph,
            sinks_last_pass: None,
            confirmation: ConfirmationRegister::default(),
        }
    }

    /// Apply the PipeWire routing a selection calls for: every non-empty selection
    /// goes through the combined sink, so the target never moves when a speaker is
    /// added or dropped — a moving target respawns `librespot` and leaves an open
    /// stream behind (#70, #53). The single seam used by `/play` and by the Spotify
    /// backend, so both agree on where audio goes.
    pub fn route_for_targets(&mut self, speakers: &[SpeakerTarget]) -> Result<(), AudioError> {
        if speakers.is_empty() {
            return Err(AudioError::NoSpeakerConnected);
        }
        self.route_to_combined(&combine_sink_plan(speakers))
    }

    /// Route playback to a combined sink spanning the plan's speakers, so the
    /// player (which opens the default sink) reaches each of them, delayed by its
    /// own offset for tunable sync. Built as a shared null sink the player feeds,
    /// plus one delayed loopback per speaker into its real `bluez_output.*` sink.
    ///
    /// Idempotent — when the combined sink is already up it reconciles the
    /// loopbacks in place instead of rebuilding, so a selection change does not
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

    /// Build the combined sink from nothing: the shared null sink, then one delayed
    /// loopback per speaker. Tears any leftover down first so repeated calls do not
    /// stack modules.
    fn build_combined(&mut self, spec: &CombineSinkSpec) -> Result<(), AudioError> {
        self.graph.teardown(&spec.sink_name)?;
        // The shared virtual sink the player streams into.
        self.graph.create_combined_sink(&spec.sink_name)?;
        // One delayed loopback per speaker: combined.monitor -> real sink, carrying
        // the branch latency the plan computed (base buffer plus the speaker's offset,
        // the per-branch sync tuning).
        let report = self.load_planned_branches_live(&spec.sink_name, &spec.branches);
        // Make the player target the combined sink. Done even when a branch failed, so
        // the speakers that did load are fed while the next tick retries the others.
        self.graph.set_default_sink(&spec.sink_name)?;
        report.into_result()
    }

    /// Bring an already-loaded combined sink in line with the plan, without ever
    /// touching the null sink: that is what keeps a live stream playing across a
    /// selection change, since re-pointing the default sink does not move a stream
    /// that is already open.
    ///
    /// What happens to the branches is [`reconcile_branches`]'s decision, and it is
    /// not "only what differs": a graph that matches the plan is left entirely alone,
    /// while a single missing or dead branch rebuilds every branch of the selection —
    /// see that function for the measurement behind it.
    fn reconcile_combined(&mut self, spec: &CombineSinkSpec) -> Result<(), AudioError> {
        let listed = self.graph.branches(&spec.sink_name)?;
        // A dead branch reads as absent below, so the reconciliation would load its
        // replacement without ever asking for the stale one to go. Unloaded here,
        // before that load, so the speaker never has two loopbacks feeding it.
        for dead in listed.iter().filter(|b| b.live == Some(false)) {
            // Best-effort: a branch that is already gone is not an error, and one
            // failure must not stop the rest of a repair.
            let _ = self.graph.unload_branch(dead.id);
        }
        // Unknown liveness keeps the branch: a transient read failure would
        // otherwise read as "everything is dead" and rebuild the whole graph
        // under the audio it protects.
        let kept: Vec<&LoadedBranch> = listed.iter().filter(|b| b.live != Some(false)).collect();
        let loaded: Vec<CombineBranch> = kept
            .iter()
            // Cloned because `reconcile_branches` compares plain branches, and
            // the ids stay behind in `kept` for the unloads below.
            .map(|b| b.branch.clone())
            .collect();
        // Nothing read is "cannot tell", not "every speaker is gone": acting on
        // it would unload every branch. So an unreadable list ends the pass, and
        // so does one naming no sink at all — the same answer with the same
        // meaning, in the shape a graph over text can only give.
        let sinks = match self.graph.sinks() {
            Ok(names) if !names.is_empty() => render_sink_listing(&names),
            _ => return Ok(()),
        };
        // A speaker that is switched off is absent, not broken: asking for it on every
        // tick is what rebuilds the graph under the ones that are playing.
        let reachable = CombineSinkSpec {
            // Cloned because the reachable plan is a spec of its own.
            sink_name: spec.sink_name.clone(),
            branches: reachable_branches(&spec.branches, &sinks),
        };
        let fresh = newly_listed_sinks(&self.sinks_last_pass, &sinks);
        self.sinks_last_pass = Some(sink_nodes(&sinks));
        let plan = reconcile_branches(&loaded, &reachable);
        for up in kept.iter().filter(|up| {
            plan.to_unload
                .iter()
                .any(|gone| gone.sink == up.branch.sink)
        }) {
            self.graph.unload_branch(up.id)?;
        }
        let report = self.load_planned_branches_live(&spec.sink_name, &plan.to_load);
        // The sink already exists, so it is usually already the default; this repairs
        // the case where the default moved away meanwhile — another application, or a
        // device that came back. Re-pointing the default at the sink a stream is
        // already on leaves that stream where it is.
        self.graph.set_default_sink(&spec.sink_name)?;

        // Arm the next pass if this one wired a speaker that came back, and learn
        // whether the previous one armed us — one exchange, so a pass can both confirm
        // and arm, and so the confirming rebuild below (which loads outside `plan`)
        // can never arm itself into a loop.
        let arms_the_next_pass = wires_a_new_sink(&plan.to_load, &fresh, &sinks);
        let confirm = self.confirmation.take_and_arm(arms_the_next_pass);
        if !confirm {
            return report.into_result();
        }
        // The previous pass wired a speaker that had just come back, and that load did
        // not start it; see `wires_a_new_sink`. Rebuilding every branch now — one tick
        // later, which is the measured gap — starts it.
        //
        // `report` is dropped rather than merged into the answer: `plan.to_load` is
        // either empty or the whole of `reachable.branches`, so this rebuild
        // re-attempts every branch the pass attempted and `second` reports the same
        // failures against a fresher reading of the graph.
        for branch in self.graph.branches(&spec.sink_name)? {
            self.graph.unload_branch(branch.id)?;
        }
        let second = self.load_planned_branches_live(&spec.sink_name, &reachable.branches);
        self.graph.set_default_sink(&spec.sink_name)?;
        second.into_result()
    }

    /// Attempt every branch against the graph, resolving each prefix to its node
    /// and loading a delayed loopback from `sink_name`'s monitor.
    fn load_planned_branches_live(
        &mut self,
        sink_name: &str,
        branches: &[CombineBranch],
    ) -> BranchLoadReport {
        // `load_planned_branches` holds both closures at once and each one needs
        // the graph, so the exclusive borrow is handed out per call instead.
        let graph = RefCell::new(&mut self.graph);
        load_planned_branches(
            branches,
            |branch| resolve_branch_sink(graph.borrow_mut().as_mut(), branch),
            |branch, real_sink| {
                graph
                    .borrow_mut()
                    .load_branch(sink_name, real_sink, branch.latency_ms)
            },
        )
    }

    /// Re-apply one branch's latency **without** tearing the combined sink down:
    /// unload just that speaker's loopback and reload it with the new offset. The
    /// shared null sink stays up, so whatever feeds it — the tone player or
    /// `librespot` — keeps streaming while the speaker is retuned.
    pub fn retune_branch(
        &mut self,
        sink_name: &str,
        branch: &CombineBranch,
    ) -> Result<(), AudioError> {
        let real = resolve_branch_sink(self.graph.as_mut(), branch)?;
        // Only the branches feeding this speaker's node are unloaded, leaving the
        // null sink and the other speaker's branch untouched.
        for up in self.graph.branches(sink_name)? {
            if up.branch.sink == real {
                self.graph.unload_branch(up.id)?;
            }
        }
        self.graph.load_branch(sink_name, &real, branch.latency_ms)
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
    /// [`Self::sink_volume`] reads back even if the system default differs. The
    /// level is clamped here, before it reaches the graph.
    pub fn set_sink_volume(&mut self, mac: &str, level: f32) -> Result<(), AudioError> {
        let sink = self.bluetooth_sink_for(mac)?;
        self.graph.set_sink_volume(&sink, clamp_volume(level))
    }

    /// Read the live volume of the speaker's sink — picks up a change made on the
    /// speaker itself (AVRCP). Returns `None` on any failure.
    pub fn sink_volume(&mut self, mac: &str) -> Option<f32> {
        let sink = self.bluetooth_sink_for(mac).ok()?;
        self.graph.sink_volume(&sink)
    }

    /// Find the sink BlueZ created for a speaker, matched by its MAC. The node
    /// name looks like `bluez_output.AA_BB_CC_DD_EE_FF.1` (colons → underscores),
    /// matched against the prefix from [`bluez_sink_prefix`].
    fn bluetooth_sink_for(&mut self, mac: &str) -> Result<String, AudioError> {
        find_sink_with_prefix(self.graph.as_mut(), &bluez_sink_prefix(mac))
            .ok()
            .flatten()
            .ok_or_else(|| AudioError::PipeWire(format!("no PipeWire sink for speaker {mac}")))
    }

    /// [`find_sink_with_prefix`] over this router's graph.
    fn find_sink_with_prefix(&mut self, prefix: &str) -> Result<Option<String>, AudioError> {
        find_sink_with_prefix(self.graph.as_mut(), prefix)
    }
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
    Ok(sink_matching_prefix(&render_sink_listing(&sinks), prefix))
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
    // as agreeing. `pactl` reports an over-amplified sink as e.g. "153%", which
    // `parse_first_percent` reads as 1.53; reporting it would break the DTO's
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
    // `bluez_output.*` sink names and each speaker's offset as branch latency.
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
        assert_eq!(first.latency_ms, branch_latency_ms(0));

        let second = &spec.branches[1];
        assert!(
            second.sink.starts_with("bluez_output.11_22_33_44_55_66"),
            "second branch must target the second speaker's bluez sink, got {}",
            second.sink
        );
        assert_eq!(second.latency_ms, branch_latency_ms(250));
    }

    // Criterion: `combine_sink_plan` builds exactly one branch for a lone
    // speaker, naming that speaker's sink prefix and its offset as the branch
    // latency — including at offset 0, a configuration this code never produced
    // while a lone plain speaker took the direct route.
    #[test]
    fn test_combine_sink_plan_lone_speaker_at_zero_offset_yields_one_branch() {
        let spec = combine_sink_plan(&[SpeakerTarget {
            address: "AA:BB:CC:DD:EE:FF".to_string(),
            offset_ms: 0,
        }]);

        assert_eq!(spec.branches.len(), 1);
        let only = &spec.branches[0];
        assert_eq!(only.sink, bluez_sink_prefix("AA:BB:CC:DD:EE:FF"));
        assert_eq!(
            only.latency_ms, BASE_BRANCH_LATENCY_MS,
            "offset 0 means no delay relative to the others, not no buffer"
        );
    }

    // Criterion: `combine_sink_plan` keeps a lone speaker's offset as the branch
    // latency — the offset only exists as loopback latency, so a lone speaker
    // going through the combined sink is the only way it is heard.
    #[test]
    fn test_combine_sink_plan_lone_speaker_carries_its_offset_as_latency() {
        let spec = combine_sink_plan(&[SpeakerTarget {
            address: "11:22:33:44:55:66".to_string(),
            offset_ms: 320,
        }]);

        assert_eq!(spec.branches.len(), 1);
        let only = &spec.branches[0];
        assert_eq!(only.sink, bluez_sink_prefix("11:22:33:44:55:66"));
        assert_eq!(only.latency_ms, branch_latency_ms(320));
    }

    // Criterion: a pure function maps an offset to a branch latency, and offset 0
    // yields the base rather than 0. `latency_msec` is the buffer a
    // `module-loopback` keeps to absorb scheduling jitter and clock drift; asking
    // for zero leaves the follower branch starved and silent while the graph's
    // driver plays on (#75).
    #[test]
    fn test_branch_latency_ms_at_offset_zero_is_the_base_and_never_zero() {
        assert_eq!(BASE_BRANCH_LATENCY_MS, 50);
        assert_eq!(branch_latency_ms(0), 50);
        assert_ne!(
            branch_latency_ms(0),
            0,
            "no branch is ever loaded with latency_msec=0"
        );
    }

    // Criterion: a non-zero offset yields the base plus itself — 70 becomes 120,
    // the offset that made the silent speaker play on hardware.
    #[test]
    fn test_branch_latency_ms_adds_the_base_to_a_nonzero_offset() {
        assert_eq!(branch_latency_ms(70), 120);
        assert_eq!(branch_latency_ms(250), BASE_BRANCH_LATENCY_MS + 250);
    }

    // Criterion: the offsets stay purely relative — two offsets differing by `n`
    // yield latencies differing by exactly `n`. The relation is the claim, not the
    // two constants: it is what says the base shifts every branch equally and so
    // changes no perceived delay between speakers.
    #[test]
    fn test_branch_latency_ms_preserves_the_gap_between_two_offsets() {
        for (lower, higher) in [(0_u32, 70_u32), (40, 250), (250, MAX_OFFSET_MS)] {
            assert_eq!(
                branch_latency_ms(higher) - branch_latency_ms(lower),
                higher - lower,
                "the gap between offsets {lower} and {higher} must survive the base"
            );
        }
    }

    // Criterion: the largest offset the selection accepts (`SpeakerTargets` clamps
    // to `0..=MAX_OFFSET_MS`) still yields a sane latency, and nothing overflows —
    // the input reaches this function from the wire, so an addition that wraps or
    // panics would be a subprocess argument built from garbage.
    #[test]
    fn test_branch_latency_ms_at_the_largest_accepted_offset_stays_sane() {
        assert_eq!(
            branch_latency_ms(MAX_OFFSET_MS),
            BASE_BRANCH_LATENCY_MS + MAX_OFFSET_MS
        );
        assert_eq!(branch_latency_ms(MAX_OFFSET_MS), 800);
        assert!(
            branch_latency_ms(u32::MAX) >= branch_latency_ms(MAX_OFFSET_MS),
            "an offset past the clamp saturates rather than wrapping or panicking"
        );
    }

    // Criterion: `route_for_targets` takes the combined path for every non-empty
    // selection. The routing itself is a `pactl` seam CI cannot exercise, so the
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
    // `AudioError::NoSpeakerConnected` — the guard runs before any `pactl` call,
    // which is what makes this testable without hardware.
    #[test]
    fn test_route_for_targets_empty_selection_is_refused() {
        let mut router = AudioRouter::new(Box::new(crate::graph::fake::FakeGraph::new()));
        assert!(matches!(
            router.route_for_targets(&[]),
            Err(AudioError::NoSpeakerConnected)
        ));
    }

    /// A realistic `pactl list short sinks` block: tab-separated columns, the
    /// node name second, one Bluetooth speaker whose live node carries the `.1`
    /// card suffix, the PC's own output and the combined null sink.
    const PACTL_SINKS: &str = concat!(
        "39\talsa_output.pci-0000_00_1f.3.analog-stereo\tPipeWire\ts32le 2ch 48000Hz\tSUSPENDED\n",
        "57\tbluez_output.80_99_E7_63_50_29.1\tPipeWire\ts16le 2ch 48000Hz\tRUNNING\n",
        "61\tblue2th_combined\tPipeWire\tf32le 2ch 48000Hz\tIDLE\n",
    );

    // Criterion: a pure function resolves a prefix against the text `pactl list
    // short sinks` prints, mapping `bluez_output.<MAC>` to the line carrying the
    // card suffix (`bluez_output.<MAC>.1`). This is the heart of the defect: the
    // prefix itself names no live node, so `--device <prefix>` silently falls
    // back to the default sink.
    #[test]
    fn test_sink_matching_prefix_resolves_a_bluez_prefix_to_the_card_suffixed_node() {
        assert_eq!(
            sink_matching_prefix(PACTL_SINKS, "bluez_output.80_99_E7_63_50_29"),
            Some("bluez_output.80_99_E7_63_50_29.1".to_string())
        );
    }

    // Criterion: the matcher returns `None` when no line matches — the speaker
    // vanished between the routing call and the spawn, and the caller must fail
    // rather than fall back to the default sink.
    #[test]
    fn test_sink_matching_prefix_without_a_matching_line_is_none() {
        assert_eq!(
            sink_matching_prefix(PACTL_SINKS, "bluez_output.AA_BB_CC_DD_EE_FF"),
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
            sink_matching_prefix(PACTL_SINKS, "blue2th_combined"),
            Some("blue2th_combined".to_string())
        );
    }

    // An empty prefix starts every string, so a bare `starts_with` answers the
    // first line of the listing — the PC's own output. `spotify_target_sink(&[])`
    // returns exactly that empty string, and only the `speakers.is_empty()` guard
    // at the top of `SpotifyBackend::start` stands between it and pointing
    // `--device` at the PC. Verified on a live `pactl`: before this guard,
    // `resolve_target_sink("")` answered `Ok("alsa_output.…HiFi__Speaker__sink")`.
    #[test]
    fn test_sink_matching_prefix_without_a_prefix_matches_nothing() {
        assert_eq!(sink_matching_prefix(PACTL_SINKS, ""), None);
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
    // `.`-suffixed candidates for one prefix, the first line wins — `pactl`'s own
    // sink-index order — rather than whichever the iteration happens to reach.
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

    // The listing comes from a subprocess, so it must survive whatever `pactl`
    // prints: a blank line, a line short of the node-name column, and a trailing
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

    // Criterion: a speaker added to the selection rebuilds *every* branch, the
    // one already streaming included. Loading the newcomer alone leaves its
    // Bluetooth node silent — the defect this pins.
    #[test]
    fn test_reconcile_branches_added_speaker_reloads_every_branch() {
        let spec = two_speaker_spec();
        let already_streaming = CombineBranch {
            sink: "bluez_output.80_99_E7_63_50_29.1".to_string(),
            latency_ms: branch_latency_ms(0),
        };
        let loaded = vec![already_streaming.clone()];

        let plan = reconcile_branches(&loaded, &spec);

        assert_eq!(
            plan.to_load, spec.branches,
            "every planned branch is loaded, not only the newly selected one"
        );
        assert_eq!(
            plan.to_unload,
            vec![already_streaming],
            "the branch already streaming is torn down so it restarts alongside the newcomer"
        );
    }

    /// `PACTL_SINKS` without the Bluetooth speaker — the listing while it is off.
    const PACTL_SINKS_WITHOUT_SPEAKER: &str = concat!(
        "39\talsa_output.pci-0000_00_1f.3.analog-stereo\tPipeWire\ts32le 2ch 48000Hz\tSUSPENDED\n",
        "61\tblue2th_combined\tPipeWire\tf32le 2ch 48000Hz\tIDLE\n",
    );

    // Criterion: the first pass has no previous listing, and what it finds predates
    // the server. Calling it new would cost every start a rebuild it does not need.
    #[test]
    fn test_newly_listed_sinks_of_a_first_pass_is_empty() {
        assert!(
            newly_listed_sinks(&None, PACTL_SINKS).is_empty(),
            "nothing is new when there is nothing to compare against"
        );
    }

    // Criterion: a speaker that comes back is listed now and was not before.
    #[test]
    fn test_newly_listed_sinks_names_a_speaker_that_came_back() {
        assert_eq!(
            newly_listed_sinks(&Some(sink_nodes(PACTL_SINKS_WITHOUT_SPEAKER)), PACTL_SINKS),
            vec!["bluez_output.80_99_E7_63_50_29.1".to_string()],
            "only the node absent from the previous pass is new"
        );
    }

    // Criterion: an unchanged listing carries nothing new, so a steady graph is
    // never rebuilt — that is what keeps the churn away between ticks.
    #[test]
    fn test_newly_listed_sinks_of_an_unchanged_listing_is_empty() {
        assert!(
            newly_listed_sinks(&Some(sink_nodes(PACTL_SINKS)), PACTL_SINKS).is_empty(),
            "nothing appeared, so nothing is new"
        );
    }

    // Criterion: the rebuild is owed exactly when the plan wires a node that has
    // just appeared — matched through the prefix, since the plan holds
    // `bluez_output.<MAC>` while the listing carries the resolved `….1`.
    #[test]
    fn test_wires_a_new_sink_matches_the_plan_prefix_against_the_fresh_node() {
        let spec = combine_sink_plan(&[SpeakerTarget {
            address: "80:99:E7:63:50:29".to_string(),
            offset_ms: 0,
        }]);
        let fresh = vec!["bluez_output.80_99_E7_63_50_29.1".to_string()];

        assert!(
            wires_a_new_sink(&spec.branches, &fresh, PACTL_SINKS),
            "the planned prefix names the node that just appeared"
        );
        assert!(
            !wires_a_new_sink(&spec.branches, &[], PACTL_SINKS),
            "no sink appeared, so no rebuild is owed"
        );
        assert!(
            !wires_a_new_sink(&[], &fresh, PACTL_SINKS),
            "a plan that loads nothing wires nothing, however fresh the node"
        );
    }

    // Criterion: a speaker whose sink is absent — switched off — is not part of
    // what the reconciliation compares against. It is what keeps a rebuild from
    // being asked for on every tick, since a missing branch rebuilds all.
    #[test]
    fn test_reachable_branches_drops_a_speaker_whose_sink_is_absent() {
        let spec = two_speaker_spec();

        let reachable = reachable_branches(&spec.branches, PACTL_SINKS);

        assert_eq!(
            reachable,
            vec![CombineBranch {
                sink: bluez_sink_prefix("80:99:E7:63:50:29"),
                latency_ms: branch_latency_ms(0),
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
            reachable_branches(&two_speaker_spec().branches, "").is_empty(),
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
            branches: reachable_branches(&spec.branches, PACTL_SINKS),
        };
        let loaded = vec![CombineBranch {
            sink: "bluez_output.80_99_E7_63_50_29.1".to_string(),
            latency_ms: branch_latency_ms(0),
        }];

        let plan = reconcile_branches(&loaded, &present);

        assert_eq!(
            plan,
            BranchReconciliation::default(),
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
            latency_ms: branch_latency_ms(0),
        }];

        let plan = reconcile_branches(&loaded, &spec);

        assert_eq!(
            plan,
            BranchReconciliation::default(),
            "the prefix names the loaded node, so this branch is already up"
        );
    }

    // The prefix names the loaded node only up to the `.` PipeWire puts before the
    // card index: a node that merely *opens* with the prefix is a different node,
    // the rule `sink_matching_prefix` already resolves by. A plain `starts_with`
    // here would call that foreign loopback the branch and leave it in place.
    #[test]
    fn test_reconcile_branches_prefix_matches_the_node_only_at_a_dot_boundary() {
        let spec = combine_sink_plan(&[SpeakerTarget {
            address: "80:99:E7:63:50:29".to_string(),
            offset_ms: 0,
        }]);
        let loaded = vec![CombineBranch {
            sink: format!("{}_2.1", spec.branches[0].sink),
            latency_ms: branch_latency_ms(0),
        }];

        let plan = reconcile_branches(&loaded, &spec);

        assert_eq!(
            plan.to_load, spec.branches,
            "the speaker's own loopback is still missing and must be loaded"
        );
        assert_eq!(
            plan.to_unload, loaded,
            "the node continuing the prefix without a `.` is not this branch"
        );
    }

    // A branch that names no node matches nothing, not even another nameless one:
    // an empty string equals an empty string, which is how a missing target used to
    // claim an arbitrary node (see `sink_matching_prefix`). `reconcile_branches` is
    // public and its inputs come from a subprocess, so the guard has to live in the
    // comparison rather than in its callers.
    #[test]
    fn test_reconcile_branches_a_branch_naming_no_node_matches_nothing() {
        let nameless = CombineBranch {
            sink: String::new(),
            latency_ms: 0,
        };
        let spec = CombineSinkSpec {
            sink_name: "blue2th_combined".to_string(),
            branches: vec![nameless.clone()],
        };

        let plan = reconcile_branches(std::slice::from_ref(&nameless), &spec);

        assert_eq!(
            plan,
            BranchReconciliation {
                to_load: vec![nameless.clone()],
                to_unload: vec![nameless],
            },
            "two empty names are not the same node"
        );
    }

    // Criterion (same one, the other side): the prefix comparison must not match
    // a different speaker's node, or a selection change would leave the wrong
    // loopback in place and never load the right one.
    #[test]
    fn test_reconcile_branches_prefix_does_not_match_another_speakers_node() {
        let spec = combine_sink_plan(&[SpeakerTarget {
            address: "80:99:E7:63:50:29".to_string(),
            offset_ms: 0,
        }]);
        let loaded = vec![CombineBranch {
            sink: "bluez_output.11_22_33_44_55_66.1".to_string(),
            latency_ms: 0,
        }];

        let plan = reconcile_branches(&loaded, &spec);

        assert_eq!(
            plan.to_load,
            vec![CombineBranch {
                sink: "bluez_output.80_99_E7_63_50_29".to_string(),
                latency_ms: branch_latency_ms(0),
            }],
            "the selected speaker has no loopback yet, so it is loaded"
        );
        assert_eq!(
            plan.to_unload,
            vec![CombineBranch {
                sink: "bluez_output.11_22_33_44_55_66.1".to_string(),
                latency_ms: 0,
            }],
            "the other speaker's loopback is not the selected one and goes"
        );
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

    // Criterion: a sink line naming no node contributes no name. An empty name is
    // the wildcard shape this project keeps paying for: carried into the register
    // it would be compared against, and later reported as, a node that does not
    // exist.
    #[test]
    fn test_sink_nodes_skips_a_line_naming_no_node() {
        let listing = concat!(
            "39\t\tPipeWire\ts32le 2ch 48000Hz\tSUSPENDED\n",
            "57\tbluez_output.80_99_E7_63_50_29.1\tPipeWire\ts16le 2ch 48000Hz\tRUNNING\n",
        );

        assert_eq!(
            sink_nodes(listing),
            vec!["bluez_output.80_99_E7_63_50_29.1".to_string()],
            "the nameless line names no node"
        );
        assert!(
            newly_listed_sinks(&Some(Vec::new()), listing)
                .iter()
                .all(|node| !node.is_empty()),
            "and so no empty name is ever reported as a speaker that came back"
        );
    }

    // Criterion: the confirming rebuild lands on the pass *after* the one that
    // wired the returning speaker. Rebuilding in the same pass was measured not to
    // repair it and to break a start that worked; the one-tick gap is the fix.
    #[test]
    fn test_confirmation_register_defers_the_rebuild_to_the_next_pass() {
        let mut register = ConfirmationRegister::default();

        assert!(
            !register.take_and_arm(true),
            "the pass that wires the returning speaker must not rebuild in the same tick"
        );
        assert!(
            register.take_and_arm(false),
            "the next pass is the one that owes the rebuild"
        );
        assert!(
            !register.take_and_arm(false),
            "and only that one: the rebuild is not repeated on the tick after"
        );
    }

    // Criterion: the confirming rebuild loads outside the plan, so it cannot arm
    // itself — a steady graph, where no sink ever appears, never owes a rebuild
    // however long it runs.
    #[test]
    fn test_confirmation_register_on_a_steady_graph_never_owes_a_rebuild() {
        let mut register = ConfirmationRegister::default();

        for tick in 0..10 {
            assert!(
                !register.take_and_arm(false),
                "no sink appeared, so tick {tick} owes nothing"
            );
        }
    }

    // Criterion: a speaker returning while a rebuild is already owed does not lose
    // its own rebuild — the register carries one tick of debt and re-arms.
    #[test]
    fn test_confirmation_register_rearms_when_a_second_speaker_returns() {
        let mut register = ConfirmationRegister::default();

        assert!(!register.take_and_arm(true), "first speaker back: armed");
        assert!(
            register.take_and_arm(true),
            "this pass both confirms the first and wires a second"
        );
        assert!(
            register.take_and_arm(false),
            "the second speaker still gets its own confirming rebuild"
        );
        assert!(!register.take_and_arm(false), "then the graph settles");
    }

    // Criterion: with every selected speaker switched off, nothing is reachable —
    // and the reconciliation asks for no rebuild at all rather than for the whole
    // selection. Asking would spawn a load per tick for nodes that do not exist.
    #[test]
    fn test_reconcile_branches_with_every_speaker_switched_off_loads_nothing() {
        let spec = two_speaker_spec();
        let none_present = CombineSinkSpec {
            sink_name: spec.sink_name.clone(),
            branches: reachable_branches(&spec.branches, PACTL_SINKS_WITHOUT_SPEAKER),
        };
        let loaded = vec![CombineBranch {
            sink: "bluez_output.80_99_E7_63_50_29.1".to_string(),
            latency_ms: branch_latency_ms(0),
        }];

        assert!(
            none_present.branches.is_empty(),
            "neither speaker has a node in this listing"
        );

        let plan = reconcile_branches(&loaded, &none_present);

        assert!(
            plan.to_load.is_empty(),
            "no branch can be loaded onto a node that is not there, got {:?}",
            plan.to_load
        );
        assert_eq!(
            plan.to_unload, loaded,
            "the loopback left over from the speaker that is now off is dropped"
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
        AudioRouter::new(Box::new(fake.clone()))
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

    fn set_default(sink: &str) -> GraphCall {
        GraphCall::SetDefaultSink {
            sink: sink.to_string(),
        }
    }

    /// Whether `call` removes or adds something — everything mutating except
    /// re-pointing the default sink.
    fn changes_the_graph(call: &GraphCall) -> bool {
        call.is_mutating() && !matches!(call, GraphCall::SetDefaultSink { .. })
    }

    /// A graph where both speakers are connected and the combined sink carries a
    /// live branch for each, at the latency of offsets 0 and 30. Returns the fake
    /// and the two branch ids.
    fn steady_graph(live: Option<bool>) -> (FakeGraph, u32, u32) {
        let fake = FakeGraph::with_sinks(&["alsa_output.pci.analog-stereo", SINK_A, SINK_B]);
        fake.add_sink(COMBINED);
        let a = fake.seed_branch(COMBINED, SINK_A, 50, live);
        let b = fake.seed_branch(COMBINED, SINK_B, 80, live);
        (fake, a, b)
    }

    fn steady_selection() -> Vec<SpeakerTarget> {
        vec![target(MAC_A, 0), target(MAC_B, 30)]
    }

    // Criterion: on an empty graph, `route_for_targets` creates the sink, loads
    // one branch per reachable speaker at `branch_latency_ms(offset)`, then sets
    // the default sink. The leading teardown is today's guard against stacking
    // modules on a leftover.
    #[test]
    fn test_route_on_an_empty_graph_creates_the_sink_loads_each_branch_then_sets_the_default() {
        let fake = FakeGraph::with_sinks(&["alsa_output.pci.analog-stereo", SINK_A, SINK_B]);
        let mut router = router_on(&fake);

        let result = router.route_for_targets(&[target(MAC_A, 0), target(MAC_B, 250)]);

        assert!(result.is_ok(), "route failed: {result:?}");
        assert_eq!(
            fake.calls(),
            vec![
                teardown(COMBINED),
                create(COMBINED),
                load(SINK_A, 50),
                load(SINK_B, 300),
                set_default(COMBINED),
            ]
        );
        assert_eq!(fake.default_sink().as_deref(), Some(COMBINED));
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
    // mutating call except `set_default_sink`.
    #[test]
    fn test_route_on_a_steady_graph_makes_no_mutating_call_but_the_default_sink() {
        let (fake, _, _) = steady_graph(Some(true));
        let mut router = router_on(&fake);

        let result = router.route_for_targets(&steady_selection());

        assert!(result.is_ok(), "route failed: {result:?}");
        assert_eq!(fake.calls(), vec![set_default(COMBINED)]);
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

        assert_eq!(
            fake.calls(),
            vec![
                set_default(COMBINED),
                set_default(COMBINED),
                set_default(COMBINED)
            ]
        );
        let ids: Vec<u32> = fake.loaded(COMBINED).iter().map(|l| l.id).collect();
        assert_eq!(ids, vec![a, b]);
    }

    // Criterion: a dead branch (`live == Some(false)`) is unloaded by id before
    // its replacement is loaded.
    #[test]
    fn test_route_unloads_a_dead_branch_before_loading_its_replacement() {
        let fake = FakeGraph::with_sinks(&[SINK_A, SINK_B, COMBINED]);
        let a = fake.seed_branch(COMBINED, SINK_A, 50, Some(true));
        let dead = fake.seed_branch(COMBINED, SINK_B, 80, Some(false));
        let mut router = router_on(&fake);

        let result = router.route_for_targets(&steady_selection());

        assert!(result.is_ok(), "route failed: {result:?}");
        let calls = fake.calls();
        let unloaded_at = calls.iter().position(|c| *c == unload(dead));
        let reloaded_at = calls.iter().position(|c| *c == load(SINK_B, 80));
        assert!(
            unloaded_at.is_some(),
            "dead branch never unloaded: {calls:?}"
        );
        assert!(reloaded_at.is_some(), "replacement never loaded: {calls:?}");
        assert!(
            unloaded_at < reloaded_at,
            "loaded before unloading: {calls:?}"
        );
        // A missing branch rebuilds the whole selection (#75), so the live one
        // goes too; the dead one goes first.
        assert_eq!(
            calls,
            vec![
                unload(dead),
                unload(a),
                load(SINK_A, 50),
                load(SINK_B, 80),
                set_default(COMBINED),
            ]
        );
        // Exactly one branch per speaker is left: never two loopbacks onto one.
        let sinks: Vec<String> = fake
            .loaded(COMBINED)
            .into_iter()
            .map(|l| l.branch.sink)
            .collect();
        assert_eq!(sinks, vec![SINK_A, SINK_B]);
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
    // combined sink the pass was entered for. Under `pactl` that is a
    // subprocess that printed nothing, so it is treated exactly like an
    // unreadable list: the pass ends without unloading anything. Driven through
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
        assert_eq!(fake.calls(), vec![set_default(COMBINED)]);
        let ids: Vec<u32> = fake.loaded(COMBINED).iter().map(|l| l.id).collect();
        assert_eq!(ids, vec![a, b]);
    }

    // Non-nominal: a planned speaker's sink is absent — it is dropped from the
    // reachable plan, no rebuild is requested for it, the other branch is
    // untouched.
    #[test]
    fn test_route_with_an_absent_speaker_leaves_the_other_branch_alone() {
        let fake = FakeGraph::with_sinks(&[SINK_A, COMBINED]);
        let a = fake.seed_branch(COMBINED, SINK_A, 50, Some(true));
        let mut router = router_on(&fake);

        for pass in 0..2 {
            let result = router.route_for_targets(&steady_selection());
            assert!(result.is_ok(), "pass {pass} failed: {result:?}");
        }

        assert_eq!(
            fake.calls(),
            vec![set_default(COMBINED), set_default(COMBINED)]
        );
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
        assert_eq!(fake.calls(), vec![unload(b), set_default(COMBINED)]);
        let ids: Vec<u32> = fake.loaded(COMBINED).iter().map(|l| l.id).collect();
        assert_eq!(ids, vec![a]);
    }

    // Non-nominal: one branch fails to load — the other is still attempted, the
    // default sink is still set, the failure comes back as `PipeWire`.
    #[test]
    fn test_route_with_one_failing_branch_still_loads_the_other_and_sets_the_default() {
        let fake = FakeGraph::with_sinks(&[SINK_A, SINK_B]);
        fake.fail_for(GraphOp::LoadBranch, SINK_A);
        let mut router = router_on(&fake);

        let result = router.route_for_targets(&steady_selection());

        assert_eq!(
            fake.calls(),
            vec![
                teardown(COMBINED),
                create(COMBINED),
                load(SINK_A, 50),
                load(SINK_B, 80),
                set_default(COMBINED),
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
        assert_eq!(fake.calls().last(), Some(&set_default(COMBINED)));
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

    // Non-nominal: a speaker that came back. The pass that wires it arms the
    // confirmation register; the next pass rebuilds every branch once, and does
    // not re-arm itself.
    #[test]
    fn test_route_after_a_speaker_came_back_rebuilds_once_on_the_next_pass() {
        let fake = FakeGraph::with_sinks(&[SINK_A, COMBINED]);
        let first_a = fake.seed_branch(COMBINED, SINK_A, 50, Some(true));
        let mut router = router_on(&fake);

        // Pass 1: speaker B is off. Nothing to do, and the sink list is learnt.
        let result = router.route_for_targets(&steady_selection());
        assert!(result.is_ok(), "pass 1 failed: {result:?}");
        assert_eq!(fake.calls(), vec![set_default(COMBINED)]);

        // Pass 2: B is back. One missing branch rebuilds the selection.
        fake.add_sink(SINK_B);
        fake.clear_calls();
        let result = router.route_for_targets(&steady_selection());
        assert!(result.is_ok(), "pass 2 failed: {result:?}");
        assert_eq!(
            fake.calls(),
            vec![
                unload(first_a),
                load(SINK_A, 50),
                load(SINK_B, 80),
                set_default(COMBINED),
            ]
        );
        let wired: Vec<u32> = fake.loaded(COMBINED).iter().map(|l| l.id).collect();
        assert_eq!(wired.len(), 2);

        // Pass 3: the graph matches the plan, and is rebuilt all the same — the
        // confirming rebuild, one tick after the load that wired B.
        fake.clear_calls();
        let result = router.route_for_targets(&steady_selection());
        assert!(result.is_ok(), "pass 3 failed: {result:?}");
        let calls = fake.calls();
        let rebuild: Vec<GraphCall> = calls
            .iter()
            .filter(|c| changes_the_graph(c))
            // Cloned to compare against literals below.
            .cloned()
            .collect();
        assert_eq!(
            rebuild,
            vec![
                unload(wired[0]),
                unload(wired[1]),
                load(SINK_A, 50),
                load(SINK_B, 80),
            ]
        );
        assert_eq!(calls.last(), Some(&set_default(COMBINED)));
        let confirmed: Vec<u32> = fake.loaded(COMBINED).iter().map(|l| l.id).collect();
        assert_eq!(confirmed.len(), 2);

        // Pass 4: the confirming rebuild did not arm another one.
        fake.clear_calls();
        let result = router.route_for_targets(&steady_selection());
        assert!(result.is_ok(), "pass 4 failed: {result:?}");
        assert_eq!(fake.calls(), vec![set_default(COMBINED)]);
        let kept: Vec<u32> = fake.loaded(COMBINED).iter().map(|l| l.id).collect();
        assert_eq!(kept, confirmed);
    }

    // Criterion: nothing listed on a router's first pass is new — wiring a
    // speaker then costs no confirming rebuild.
    #[test]
    fn test_route_first_pass_wiring_does_not_arm_a_rebuild() {
        let fake = FakeGraph::with_sinks(&[SINK_A, SINK_B, COMBINED]);
        let mut router = router_on(&fake);

        let result = router.route_for_targets(&steady_selection());
        assert!(result.is_ok(), "pass 1 failed: {result:?}");
        assert_eq!(
            fake.calls(),
            vec![load(SINK_A, 50), load(SINK_B, 80), set_default(COMBINED)]
        );

        fake.clear_calls();
        let result = router.route_for_targets(&steady_selection());
        assert!(result.is_ok(), "pass 2 failed: {result:?}");
        assert_eq!(fake.calls(), vec![set_default(COMBINED)]);
    }

    // Criterion: only a sink that was absent from the previous pass is "a
    // speaker that came back". A speaker deselected and reselected while its
    // sink never left is reloaded, and that reload owes no confirming rebuild —
    // the one-tick-later cut exists for a node PipeWire has just created, not
    // for every load. Pinned because a router that forgets what it listed sees
    // every sink as new on every pass, and every reload then costs a second cut.
    #[test]
    fn test_route_reselecting_a_speaker_whose_sink_never_left_owes_no_confirming_rebuild() {
        let (fake, a, b) = steady_graph(Some(true));
        let mut router = router_on(&fake);

        // Pass 1: steady. Pass 2: B deselected, its branch goes.
        let result = router.route_for_targets(&steady_selection());
        assert!(result.is_ok(), "pass 1 failed: {result:?}");
        let result = router.route_for_targets(&[target(MAC_A, 0)]);
        assert!(result.is_ok(), "pass 2 failed: {result:?}");
        assert_eq!(
            fake.calls(),
            vec![set_default(COMBINED), unload(b), set_default(COMBINED)]
        );

        // Pass 3: B reselected. One missing branch rebuilds the selection, and
        // B's sink was listed on every pass so far.
        fake.clear_calls();
        let result = router.route_for_targets(&steady_selection());
        assert!(result.is_ok(), "pass 3 failed: {result:?}");
        assert_eq!(
            fake.calls(),
            vec![
                unload(a),
                load(SINK_A, 50),
                load(SINK_B, 80),
                set_default(COMBINED),
            ]
        );

        // Pass 4: no sink appeared, so nothing is owed.
        fake.clear_calls();
        let result = router.route_for_targets(&steady_selection());
        assert!(result.is_ok(), "pass 4 failed: {result:?}");
        assert_eq!(fake.calls(), vec![set_default(COMBINED)]);
    }

    // Non-nominal: an empty target names no node, so it is answered without the
    // graph being read at all — not even the sink list. With `pactl` underneath
    // a read is a spawn, and one that could only ever answer "nothing".
    #[test]
    fn test_empty_target_is_answered_without_reading_the_graph() {
        let (fake, _, _) = steady_graph(Some(true));
        let mut router = router_on(&fake);

        assert!(router.resolve_target_sink("").is_err());
        assert!(!router.combined_sink_exists(""));

        assert_eq!(fake.all_calls(), Vec::<GraphCall>::new());
    }

    // Criterion: the confirmation register and the last-pass sink list are
    // fields of `AudioRouter` — a router that armed a rebuild, and that has seen
    // other sinks, changes nothing for a second router in the same process.
    #[test]
    fn test_two_routers_do_not_share_the_confirmation_register() {
        // The first router goes through a speaker coming back, which arms it.
        let first_fake = FakeGraph::with_sinks(&[SINK_A, COMBINED]);
        first_fake.seed_branch(COMBINED, SINK_A, 50, Some(true));
        let mut first = router_on(&first_fake);
        let result = first.route_for_targets(&steady_selection());
        assert!(result.is_ok(), "first router, pass 1: {result:?}");
        first_fake.add_sink(SINK_B);
        let result = first.route_for_targets(&steady_selection());
        assert!(result.is_ok(), "first router, pass 2: {result:?}");
        assert!(
            first_fake.calls().contains(&load(SINK_B, 80)),
            "the first router never wired the returning speaker"
        );

        // The second router owes nothing: its steady graph stays untouched, and
        // sinks the first router never listed are not "new" to it.
        let sink_c = "bluez_output.AA_BB_CC_DD_EE_03.1";
        let second_fake = FakeGraph::with_sinks(&[sink_c, COMBINED]);
        let c = second_fake.seed_branch(COMBINED, sink_c, 50, Some(true));
        let mut second = router_on(&second_fake);
        for pass in 0..2 {
            let result = second.route_for_targets(&[target("AA:BB:CC:DD:EE:03", 0)]);
            assert!(result.is_ok(), "second router, pass {pass}: {result:?}");
        }
        assert_eq!(
            second_fake.calls(),
            vec![set_default(COMBINED), set_default(COMBINED)]
        );
        let ids: Vec<u32> = second_fake.loaded(COMBINED).iter().map(|l| l.id).collect();
        assert_eq!(ids, vec![c]);

        // And the first router still owes its own rebuild.
        first_fake.clear_calls();
        let result = first.route_for_targets(&steady_selection());
        assert!(result.is_ok(), "first router, pass 3: {result:?}");
        assert!(
            first_fake.calls().iter().any(changes_the_graph),
            "the first router lost its armed rebuild: {:?}",
            first_fake.calls()
        );
    }

    // Criterion: `retune_branch` unloads only that speaker's branch and reloads
    // it; the combined sink and the other branch receive no call.
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
        assert_eq!(fake.calls(), vec![unload(a), load(SINK_A, 120)]);
        let loaded = fake.loaded(COMBINED);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].id, b);
        assert_eq!(loaded[0].branch.latency_ms, 80);
        assert_eq!(loaded[1].branch.sink, SINK_A);
        assert_eq!(loaded[1].branch.latency_ms, 120);
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

    // Criterion: the volume is read per speaker sink, over-amplification
    // included, and an absent speaker reads as `None`.
    #[test]
    fn test_sink_volume_reads_the_speaker_sink() {
        let (fake, _, _) = steady_graph(Some(true));
        fake.set_volume(SINK_A, 0.59);
        fake.set_volume(SINK_B, 1.53);
        let mut router = router_on(&fake);

        assert_eq!(router.sink_volume(MAC_A), Some(0.59));
        assert_eq!(router.sink_volume(MAC_B), Some(1.53));
        assert_eq!(router.sink_volume("AA:BB:CC:DD:EE:03"), None);
        assert!(fake.all_calls().contains(&GraphCall::SinkVolume {
            sink: SINK_A.to_string()
        }));
        assert_eq!(fake.calls(), Vec::<GraphCall>::new());
    }
}
