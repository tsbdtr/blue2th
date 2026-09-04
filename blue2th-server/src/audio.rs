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
    io::Cursor,
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender},
        Arc,
    },
    thread::JoinHandle,
    time::Duration,
};

use blue2th_proto::{PlaybackState, PlaybackStatus, SpeakerTarget};

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
                // the connected speaker just before calling play.
                if let Some(previous) = player.take() {
                    previous.stop();
                }
                // Drop the previous device so the new one binds to the current
                // default sink (pointed at the speaker just before this call).
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
    /// Branch latency in milliseconds (the speaker's offset).
    pub latency_ms: u32,
}

/// Pure plan for a PipeWire combined sink spanning two speakers' sinks, with each
/// speaker's offset captured as branch latency. Building this performs no I/O; the
/// hardware seam (`route_to_combined` / `teardown_combined`) consumes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CombineSinkSpec {
    /// Node name of the combined sink to create.
    pub sink_name: String,
    /// The member branches, one per target speaker.
    pub branches: Vec<CombineBranch>,
}

/// Build the (pure, testable) combined-sink plan for the given targets: each
/// target maps to its `bluez_output.*` sink name and its offset as branch
/// latency. Used by the two-speaker route path; performs no I/O.
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

/// Whether a selection needs the combined-sink path: two speakers to fan out, or
/// a single one carrying an offset. The offset only exists as `module-loopback`
/// latency, so routing a single speaker straight to its sink would silently drop
/// it — which is why a lone speaker's offset used to have no audible effect. Pure.
pub fn needs_combined(speakers: &[SpeakerTarget]) -> bool {
    speakers.len() > 1 || speakers.iter().any(|s| s.offset_ms > 0)
}

/// Apply the PipeWire routing a selection calls for: the combined sink when
/// [`needs_combined`], the direct single-sink route otherwise. The single seam
/// used by `/play` and by the Spotify backend, so both agree on where audio goes.
pub fn route_for_targets(speakers: &[SpeakerTarget]) -> Result<(), AudioError> {
    let Some(only) = speakers.first() else {
        return Err(AudioError::NoSpeakerConnected);
    };
    if needs_combined(speakers) {
        route_to_combined(&combine_sink_plan(speakers))
    } else {
        route_to_speaker(&only.address)
    }
}

/// Derive the `bluez_output.*` PipeWire sink node-name prefix for a speaker MAC
/// (colons → underscores, upper-cased), matching what BlueZ creates.
pub fn bluez_sink_prefix(mac: &str) -> String {
    format!("bluez_output.{}", mac.to_uppercase().replace(':', "_"))
}

/// Point the default PipeWire sink at the connected Bluetooth speaker so the
/// rodio output (which opens the default device) and the `wpctl` volume both
/// target it. Phase 4 will route to a combined sink instead of hijacking the
/// system default.
pub fn route_to_speaker(mac: &str) -> Result<(), AudioError> {
    let sink = bluetooth_sink_for(mac)?;
    set_default_sink(&sink)
}

/// Route playback to a PipeWire combined sink spanning the plan's speakers, so the
/// player (which opens the default sink) fans out to both, each delayed by its own
/// offset for tunable sync. Built as a shared null sink the player feeds, plus one
/// delayed `module-loopback` per speaker into its real `bluez_output.*` sink.
///
/// Hardware seam (PipeWire/`pactl`): not exercised by CI, validated manually on a
/// real two-speaker setup. Idempotent — when the combined sink is already up it
/// reconciles the loopbacks in place instead of rebuilding, so a selection change
/// does not unload the null sink the player is streaming into; otherwise it
/// builds the whole graph from scratch.
pub fn route_to_combined(spec: &CombineSinkSpec) -> Result<(), AudioError> {
    if combined_sink_exists(&spec.sink_name) {
        return reconcile_combined(spec);
    }
    build_combined(spec)
}

/// Build the combined sink from nothing: the shared null sink, then one delayed
/// loopback per speaker. Tears any leftover down first so repeated calls do not
/// stack modules.
fn build_combined(spec: &CombineSinkSpec) -> Result<(), AudioError> {
    teardown_combined(&spec.sink_name)?;
    // The shared virtual sink the player streams into.
    load_module(&[
        "module-null-sink".to_string(),
        format!("sink_name={}", spec.sink_name),
        format!("sink_properties=node.description={}", spec.sink_name),
    ])?;
    // One delayed loopback per speaker: combined.monitor -> real sink, with the
    // speaker's offset applied as loopback latency (the per-branch sync tuning).
    for branch in &spec.branches {
        load_branch_loopback(
            &spec.sink_name,
            &resolve_branch_sink(branch)?,
            branch.latency_ms,
        )?;
    }
    // Make the player target the combined sink.
    set_default_sink(&spec.sink_name)
}

/// Bring an already-loaded combined sink in line with the plan: unload only the
/// loopbacks the selection no longer calls for, load only the missing ones, and
/// leave the null sink — and every branch already correct — alone. That is what
/// keeps a live stream playing across a selection change, since `set_default_sink`
/// does not move a stream that is already open.
///
/// Hardware seam (PipeWire/`pactl`): not exercised by CI; the decisions it acts on
/// are [`loaded_branches`] and [`reconcile_branches`], which are pure and tested.
fn reconcile_combined(spec: &CombineSinkSpec) -> Result<(), AudioError> {
    let listing = module_listing()?;
    let loaded = loaded_branches(&listing, &spec.sink_name);
    let plan = reconcile_branches(&loaded, spec);
    let source = format!("source={}.monitor", spec.sink_name);
    for branch in &plan.to_unload {
        // Match on both ends, as `retune_combined_branch` does: on `sink=` alone a
        // module merely feeding *into* the combined sink would be unloaded too.
        unload_modules_matching(&[&source, &format!("sink={}", branch.sink)])?;
    }
    for branch in &plan.to_load {
        load_branch_loopback(
            &spec.sink_name,
            &resolve_branch_sink(branch)?,
            branch.latency_ms,
        )?;
    }
    // The sink already exists, so it is usually already the default; this repairs
    // the case where the default moved away meanwhile — a selection that dropped to
    // the direct route and came back. Re-pointing the default at the sink a stream
    // is already on leaves that stream where it is.
    set_default_sink(&spec.sink_name)
}

/// Resolve a branch's `bluez_output.*` prefix to the live node name, erroring
/// rather than sending audio elsewhere when the speaker's sink has vanished.
fn resolve_branch_sink(branch: &CombineBranch) -> Result<String, AudioError> {
    find_sink_with_prefix(&branch.sink)
        .ok_or_else(|| AudioError::PipeWire(format!("no PipeWire sink for prefix {}", branch.sink)))
}

/// Load one delayed loopback from the combined sink's monitor into a resolved
/// speaker sink. `*_dont_move=true` pins both ends, so a default-sink change
/// cannot drag the branch off the speaker it was built for.
fn load_branch_loopback(
    sink_name: &str,
    real_sink: &str,
    latency_ms: u32,
) -> Result<(), AudioError> {
    load_module(&[
        "module-loopback".to_string(),
        format!("source={sink_name}.monitor"),
        format!("sink={real_sink}"),
        format!("latency_msec={latency_ms}"),
        "source_dont_move=true".to_string(),
        "sink_dont_move=true".to_string(),
    ])
}

/// Tear down a combined sink built by [`route_to_combined`]: unload the null sink
/// and every loopback whose arguments reference `sink_name`. Best-effort — a
/// missing module is not an error (the combined sink may simply not exist yet).
pub fn teardown_combined(sink_name: &str) -> Result<(), AudioError> {
    unload_modules_matching(&[sink_name])
}

/// Whether the combined null sink is currently loaded, i.e. whether a branch can
/// be retuned in place rather than routed from scratch.
pub fn combined_sink_exists(sink_name: &str) -> bool {
    find_sink_with_prefix(sink_name).is_some()
}

/// Re-apply one branch's latency **without** tearing the combined sink down:
/// unload just that speaker's loopback and reload it with the new offset. The
/// shared null sink stays up, so whatever feeds it — the tone player or
/// `librespot` — keeps streaming while the speaker is retuned.
///
/// Hardware seam (PipeWire/`pactl`): not exercised by CI.
pub fn retune_combined_branch(sink_name: &str, branch: &CombineBranch) -> Result<(), AudioError> {
    let real = resolve_branch_sink(branch)?;
    // Match on both ends so only this branch's loopback is unloaded, leaving the
    // null sink and the other speaker's branch untouched.
    unload_modules_matching(&[
        &format!("source={sink_name}.monitor"),
        &format!("sink={real}"),
    ])?;
    load_branch_loopback(sink_name, &real, branch.latency_ms)
}

/// The loopback branches currently loaded for the combined sink `sink_name`,
/// read out of the text `pactl list short modules` prints: one entry per
/// `module-loopback` fed by `<sink_name>.monitor`, carrying the **resolved** sink
/// node it feeds and its `latency_msec`. Pure — performs no I/O.
///
/// The input comes from a subprocess, so anything that does not parse is skipped
/// rather than reported: a truncated or unexpected listing yields fewer branches,
/// never a failure.
pub fn loaded_branches(listing: &str, sink_name: &str) -> Vec<CombineBranch> {
    let source = format!("source={sink_name}.monitor");
    listing
        .lines()
        .filter_map(|line| {
            let mut columns = line.split('\t');
            let _index = columns.next()?;
            if columns.next()? != "module-loopback" {
                return None;
            }
            let args: Vec<&str> = columns.next()?.split_whitespace().collect();
            if !args.contains(&source.as_str()) {
                return None;
            }
            let sink = args.iter().find_map(|a| a.strip_prefix("sink="))?;
            let latency_ms = args
                .iter()
                .find_map(|a| a.strip_prefix("latency_msec="))?
                .parse()
                .ok()?;
            Some(CombineBranch {
                sink: sink.to_string(),
                latency_ms,
            })
        })
        .collect()
}

/// What a selection change has to do to an already-loaded combined sink: the
/// branches to load and the loaded ones to unload.
///
/// `to_unload` carries the branches as they were read from the module listing —
/// i.e. with the **resolved** node name — because that is what the unload seam
/// matches its `sink=` pattern on, while `to_load` carries the plan's
/// `bluez_output.*` prefixes, which the load seam resolves.
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
    BranchReconciliation {
        to_load: spec
            .branches
            .iter()
            .filter(|planned| !loaded.iter().any(|up| branch_is(up, planned)))
            .cloned()
            .collect(),
        to_unload: loaded
            .iter()
            .filter(|up| !spec.branches.iter().any(|planned| branch_is(up, planned)))
            .cloned()
            .collect(),
    }
}

/// Whether the loaded loopback `up` is exactly what the planned branch asks for:
/// same latency, and a sink the plan's `bluez_output.*` prefix names. The prefix
/// must either equal the loaded node or continue it with the `.` PipeWire puts
/// before the card index, so one speaker's prefix cannot claim another's node.
fn branch_is(up: &CombineBranch, planned: &CombineBranch) -> bool {
    up.latency_ms == planned.latency_ms
        && !planned.sink.is_empty()
        && (up.sink == planned.sink
            || up
                .sink
                .strip_prefix(&planned.sink)
                .is_some_and(|rest| rest.starts_with('.')))
}

/// The text `pactl list short modules` prints, for [`loaded_branches`] and
/// [`unload_modules_matching`] to read.
fn module_listing() -> Result<String, AudioError> {
    let output = Command::new("pactl")
        .args(["list", "short", "modules"])
        .output()
        .map_err(|e| AudioError::PipeWire(format!("failed to run pactl: {e}")))?;
    if !output.status.success() {
        return Err(AudioError::PipeWire(
            "pactl list modules failed".to_string(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Unload every loaded module whose `pactl list short modules` line contains all
/// of `patterns`. Best-effort — a missing module is not an error.
fn unload_modules_matching(patterns: &[&str]) -> Result<(), AudioError> {
    for line in module_listing()?.lines() {
        if patterns.iter().all(|pattern| line.contains(pattern)) {
            if let Some(id) = line.split('\t').next() {
                // Best-effort: ignore failures so one stale module cannot block teardown.
                let _ = Command::new("pactl").args(["unload-module", id]).status();
            }
        }
    }
    Ok(())
}

/// Load a PipeWire module via `pactl load-module <args...>`, mapping a failure to
/// an [`AudioError::PipeWire`].
fn load_module(args: &[String]) -> Result<(), AudioError> {
    let status = Command::new("pactl")
        .arg("load-module")
        .args(args)
        .status()
        .map_err(|e| AudioError::PipeWire(format!("failed to run pactl: {e}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(AudioError::PipeWire(format!(
            "pactl load-module failed: {status}"
        )))
    }
}

/// Find the PipeWire sink BlueZ created for a speaker, matched by its MAC. The
/// node name looks like `bluez_output.AA_BB_CC_DD_EE_FF.1` (colons → underscores),
/// matched against the prefix from [`bluez_sink_prefix`].
fn bluetooth_sink_for(mac: &str) -> Result<String, AudioError> {
    find_sink_with_prefix(&bluez_sink_prefix(mac))
        .ok_or_else(|| AudioError::PipeWire(format!("no PipeWire sink for speaker {mac}")))
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
        if suffixed.is_none()
            && name
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('.'))
        {
            suffixed = Some(name);
        }
    }
    suffixed.map(|name| name.to_string())
}

/// Resolve a live PipeWire sink node-name from its `bluez_output.*` prefix (which
/// the combined-sink plan stores without the trailing card suffix). Returns
/// `None` if no sink currently matches or `pactl` is unavailable.
fn find_sink_with_prefix(prefix: &str) -> Option<String> {
    let output = Command::new("pactl")
        .args(["list", "short", "sinks"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    sink_matching_prefix(&String::from_utf8_lossy(&output.stdout), prefix)
}

/// Resolve a logical playback target to the live PipeWire node name to hand a
/// player. The target is either a `bluez_output.*` prefix (from
/// [`bluez_sink_prefix`], which carries no card suffix) or an exact node name
/// such as `blue2th_combined`, which resolves to itself. Errors rather than
/// falling back to the default sink, so a vanished speaker — or an empty target,
/// which no sink can carry — is reported instead of silently sending audio
/// elsewhere.
pub fn resolve_target_sink(target: &str) -> Result<String, AudioError> {
    find_sink_with_prefix(target)
        .ok_or_else(|| AudioError::PipeWire(format!("no PipeWire sink for target {target}")))
}

/// Make `sink` the default PipeWire sink (by node name) via `pactl`.
fn set_default_sink(sink: &str) -> Result<(), AudioError> {
    let status = Command::new("pactl")
        .args(["set-default-sink", sink])
        .status()
        .map_err(|e| AudioError::PipeWire(format!("failed to run pactl: {e}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(AudioError::PipeWire(format!(
            "pactl set-default-sink failed: {status}"
        )))
    }
}

/// Set the speaker's PipeWire sink volume (`0.0..=1.0`) by sink name, so it
/// matches what `sink_volume` reads back even if the system default differs.
pub fn set_sink_volume(mac: &str, level: f32) -> Result<(), AudioError> {
    let sink = bluetooth_sink_for(mac)?;
    let pct = (clamp_volume(level) * 100.0).round() as u32;
    let status = Command::new("pactl")
        .args(["set-sink-volume", &sink, &format!("{pct}%")])
        .status()
        .map_err(|e| AudioError::PipeWire(format!("failed to run pactl: {e}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(AudioError::PipeWire(format!(
            "pactl set-sink-volume failed: {status}"
        )))
    }
}

/// Read the live volume of the speaker's PipeWire sink — picks up a change made
/// on the speaker itself (AVRCP). Normally `0.0..=1.0`, but an over-amplified
/// sink reads above `1.0` (pactl prints e.g. "153%"). Returns `None` on any
/// failure.
pub fn sink_volume(mac: &str) -> Option<f32> {
    let sink = bluetooth_sink_for(mac).ok()?;
    let output = Command::new("pactl")
        .args(["get-sink-volume", &sink])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    // e.g. "Volume: front-left: 38666 /  59% / -13.75 dB,   front-right: ..."
    parse_first_percent(&String::from_utf8_lossy(&output.stdout))
}

/// Extract the first `<n>%` from `pactl get-sink-volume` output as a fraction
/// (`59%` -> `0.59`). Above `1.0` for an over-amplified sink, which prints `153%`.
fn parse_first_percent(text: &str) -> Option<f32> {
    let pct_end = text.find('%')?;
    // Collect the digit run immediately before '%' (char-wise, no byte slicing).
    let digits: String = text[..pct_end]
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    let pct: u32 = digits.parse().ok()?;
    Some(pct as f32 / 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Criterion: a lone speaker with an offset needs the combined sink — the
    // offset is realised as loopback latency, which the direct single-sink route
    // does not have, so it would otherwise be silently dropped.
    #[test]
    fn test_needs_combined_single_target_with_offset() {
        let delayed = SpeakerTarget {
            address: "AA:BB:CC:DD:EE:FF".to_string(),
            offset_ms: 750,
        };
        assert!(needs_combined(std::slice::from_ref(&delayed)));
    }

    // Criterion: a lone speaker with no offset keeps the direct route, and two
    // speakers always fan out through the combined sink.
    #[test]
    fn test_needs_combined_covers_plain_single_and_fan_out() {
        let plain = SpeakerTarget {
            address: "AA:BB:CC:DD:EE:FF".to_string(),
            offset_ms: 0,
        };
        let other = SpeakerTarget {
            address: "11:22:33:44:55:66".to_string(),
            offset_ms: 0,
        };
        assert!(!needs_combined(std::slice::from_ref(&plain)));
        assert!(needs_combined(&[plain, other]));
        assert!(!needs_combined(&[]));
    }

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

    // The live sink volume read from `pactl get-sink-volume` is parsed into
    // `0.0..=1.0`.
    #[test]
    fn test_parse_first_percent_reads_volume_fraction() {
        let line = "Volume: front-left: 38666 /  59% / -13.75 dB,   front-right: 38666 /  59%";
        assert_eq!(parse_first_percent(line), Some(0.59));
        assert_eq!(
            parse_first_percent("Volume: front-left: 0 / 0% / -inf dB"),
            Some(0.0)
        );
        assert_eq!(parse_first_percent("no percentage here"), None);
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
        assert_eq!(first.latency_ms, 0);

        let second = &spec.branches[1];
        assert!(
            second.sink.starts_with("bluez_output.11_22_33_44_55_66"),
            "second branch must target the second speaker's bluez sink, got {}",
            second.sink
        );
        assert_eq!(second.latency_ms, 250);
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

    // Criterion: `start()` propagates a *resolution failure*, so the resolver
    // must report one rather than hand the target back unresolved — a silent
    // fallback to the default sink is the defect this change fixes. Needs no
    // hardware and holds either way: with no `pactl` the lookup fails outright,
    // and with a live one no sink can carry this name.
    #[test]
    fn test_resolve_target_sink_for_an_absent_node_is_an_error() {
        assert!(
            resolve_target_sink("blue2th_no_such_sink_ever").is_err(),
            "an unresolvable target must be an error, never the target handed back"
        );
    }

    // The empty target `spotify_target_sink(&[])` yields must not resolve to the
    // first sink `pactl` happens to list.
    #[test]
    fn test_resolve_target_sink_for_an_empty_target_is_an_error() {
        assert!(
            resolve_target_sink("").is_err(),
            "an empty target names no node and must not resolve to an arbitrary sink"
        );
    }

    /// A realistic `pactl list short modules` block: index, module name and the
    /// argument string, tab-separated. It carries the combined sink's own null
    /// sink, its two delayed loopbacks, a loopback belonging to a *different*
    /// combined sink, a loopback feeding *into* the combined sink (whose
    /// `sink=` mentions it but which is not one of its branches) and ordinary
    /// unrelated modules.
    const PACTL_MODULES: &str = concat!(
        "10\tmodule-device-restore\t\n",
        "26\tmodule-null-sink\tsink_name=blue2th_combined sink_properties=node.description=blue2th_combined\n",
        "27\tmodule-loopback\tsource=blue2th_combined.monitor sink=bluez_output.80_99_E7_63_50_29.1 latency_msec=0 source_dont_move=true sink_dont_move=true\n",
        "28\tmodule-loopback\tsource=blue2th_combined.monitor sink=bluez_output.11_22_33_44_55_66.1 latency_msec=250 source_dont_move=true sink_dont_move=true\n",
        "29\tmodule-loopback\tsource=other_combined.monitor sink=bluez_output.AA_BB_CC_DD_EE_FF.1 latency_msec=120 source_dont_move=true sink_dont_move=true\n",
        "30\tmodule-loopback\tsource=alsa_input.pci-0000_00_1f.3.analog-stereo sink=blue2th_combined latency_msec=40\n",
        "31\tmodule-switch-on-connect\t\n",
    );

    // Criterion: a pure function reads `pactl list short modules` and returns the
    // loopback branches loaded for a given combined sink, each with the real sink
    // it feeds and its `latency_msec`.
    #[test]
    fn test_loaded_branches_reads_each_loopback_sink_and_latency() {
        let branches = loaded_branches(PACTL_MODULES, "blue2th_combined");

        assert_eq!(
            branches,
            vec![
                CombineBranch {
                    sink: "bluez_output.80_99_E7_63_50_29.1".to_string(),
                    latency_ms: 0,
                },
                CombineBranch {
                    sink: "bluez_output.11_22_33_44_55_66.1".to_string(),
                    latency_ms: 250,
                },
            ],
            "the two loopbacks fed by blue2th_combined.monitor, with their latencies"
        );
    }

    // Criterion: the parser ignores modules belonging to another sink name, and
    // any module that is not a `module-loopback` — in particular the null sink,
    // which a selection change must never unload.
    #[test]
    fn test_loaded_branches_ignores_other_sinks_and_non_loopback_modules() {
        let branches = loaded_branches(PACTL_MODULES, "blue2th_combined");

        assert!(
            !branches
                .iter()
                .any(|b| b.sink.contains("AA_BB_CC_DD_EE_FF")),
            "a loopback of another combined sink is not one of our branches: {branches:?}"
        );
        assert!(
            !branches.iter().any(|b| b.sink.contains("blue2th_combined")),
            "neither the null sink nor a loopback feeding into it is a branch: {branches:?}"
        );
    }

    // Criterion: the same parser reads another sink's branches without picking
    // ours up, i.e. the match is on `source=<sink_name>.monitor`.
    #[test]
    fn test_loaded_branches_reads_only_the_named_sinks_branches() {
        let branches = loaded_branches(PACTL_MODULES, "other_combined");

        assert_eq!(
            branches,
            vec![CombineBranch {
                sink: "bluez_output.AA_BB_CC_DD_EE_FF.1".to_string(),
                latency_ms: 120,
            }],
            "only the loopback fed by other_combined.monitor"
        );
    }

    // Criterion: a listing with nothing matching yields an empty set rather than
    // a panic — the input comes from a subprocess and may be anything.
    #[test]
    fn test_loaded_branches_without_a_matching_module_is_empty() {
        assert!(loaded_branches("", "blue2th_combined").is_empty());
        assert!(loaded_branches(
            "10\tmodule-device-restore\t\n31\tmodule-switch-on-connect\t\n",
            "blue2th_combined"
        )
        .is_empty());
        assert!(
            loaded_branches("27\tmodule-loopback", "blue2th_combined").is_empty(),
            "a truncated line names no sink and no latency, so it is no branch"
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

    // Criterion: a speaker present in both the loaded set and the spec with the
    // same latency appears in neither list — an unchanged selection touches
    // nothing, which is what keeps the stream alive.
    #[test]
    fn test_reconcile_branches_unchanged_selection_changes_nothing() {
        let spec = two_speaker_spec();
        let loaded = loaded_branches(PACTL_MODULES, &spec.sink_name);

        let plan = reconcile_branches(&loaded, &spec);

        assert_eq!(plan, BranchReconciliation::default());
    }

    // Criterion: a speaker added to the selection yields exactly one load and no
    // unload — the loopback already streaming stays up.
    #[test]
    fn test_reconcile_branches_added_speaker_loads_only_the_new_branch() {
        let spec = two_speaker_spec();
        let loaded = vec![CombineBranch {
            sink: "bluez_output.80_99_E7_63_50_29.1".to_string(),
            latency_ms: 0,
        }];

        let plan = reconcile_branches(&loaded, &spec);

        assert_eq!(
            plan.to_load,
            vec![CombineBranch {
                sink: bluez_sink_prefix("11:22:33:44:55:66"),
                latency_ms: 250,
            }],
            "only the newly selected speaker is loaded"
        );
        assert!(
            plan.to_unload.is_empty(),
            "nothing is unloaded when a speaker is added, got {:?}",
            plan.to_unload
        );
    }

    // Criterion: a speaker dropped from the selection yields exactly one unload,
    // naming the resolved node its loopback feeds, and never the null sink.
    #[test]
    fn test_reconcile_branches_dropped_speaker_unloads_only_that_branch() {
        let spec = combine_sink_plan(&[SpeakerTarget {
            address: "80:99:E7:63:50:29".to_string(),
            offset_ms: 0,
        }]);
        let loaded = loaded_branches(PACTL_MODULES, &spec.sink_name);

        let plan = reconcile_branches(&loaded, &spec);

        assert!(
            plan.to_load.is_empty(),
            "the remaining speaker's loopback is already loaded, got {:?}",
            plan.to_load
        );
        assert_eq!(
            plan.to_unload,
            vec![CombineBranch {
                sink: "bluez_output.11_22_33_44_55_66.1".to_string(),
                latency_ms: 250,
            }],
            "only the deselected speaker's loopback is unloaded"
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

    // Criterion: a speaker present in both but with a different latency is
    // reloaded — the new branch is loaded and the stale loopback must not
    // survive.
    #[test]
    fn test_reconcile_branches_latency_change_replaces_the_stale_loopback() {
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
        let loaded = loaded_branches(PACTL_MODULES, &spec.sink_name);

        let plan = reconcile_branches(&loaded, &spec);

        assert_eq!(
            plan.to_load,
            vec![CombineBranch {
                sink: bluez_sink_prefix("11:22:33:44:55:66"),
                latency_ms: 400,
            }],
            "the retuned speaker is reloaded with its new latency, alone"
        );
        assert_eq!(
            plan.to_unload,
            vec![CombineBranch {
                sink: "bluez_output.11_22_33_44_55_66.1".to_string(),
                latency_ms: 250,
            }],
            "the stale 250 ms loopback is unloaded, and only it"
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
            BranchReconciliation::default(),
            "the prefix names the loaded node, so this branch is already up"
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
                latency_ms: 0,
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
}
