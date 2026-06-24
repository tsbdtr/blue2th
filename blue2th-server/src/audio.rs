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
/// real two-speaker setup. Idempotent — it tears any previous combined sink down
/// first so repeated `/play` calls do not stack modules.
pub fn route_to_combined(spec: &CombineSinkSpec) -> Result<(), AudioError> {
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
        let real = find_sink_with_prefix(&branch.sink).ok_or_else(|| {
            AudioError::PipeWire(format!("no PipeWire sink for prefix {}", branch.sink))
        })?;
        load_module(&[
            "module-loopback".to_string(),
            format!("source={}.monitor", spec.sink_name),
            format!("sink={real}"),
            format!("latency_msec={}", branch.latency_ms),
            "source_dont_move=true".to_string(),
            "sink_dont_move=true".to_string(),
        ])?;
    }
    // Make the player target the combined sink.
    set_default_sink(&spec.sink_name)
}

/// Tear down a combined sink built by [`route_to_combined`]: unload the null sink
/// and every loopback whose arguments reference `sink_name`. Best-effort — a
/// missing module is not an error (the combined sink may simply not exist yet).
pub fn teardown_combined(sink_name: &str) -> Result<(), AudioError> {
    let output = Command::new("pactl")
        .args(["list", "short", "modules"])
        .output()
        .map_err(|e| AudioError::PipeWire(format!("failed to run pactl: {e}")))?;
    if !output.status.success() {
        return Err(AudioError::PipeWire(
            "pactl list modules failed".to_string(),
        ));
    }
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if line.contains(sink_name) {
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
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split('\t').nth(1))
        .find(|name| name.starts_with(prefix))
        .map(|name| name.to_string())
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

/// Read the live volume (`0.0..=1.0`) of the speaker's PipeWire sink — picks up a
/// change made on the speaker itself (AVRCP). Returns `None` on any failure.
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

/// Extract the first `<n>%` from `pactl get-sink-volume` output as `0.0..=1.0`.
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
}
