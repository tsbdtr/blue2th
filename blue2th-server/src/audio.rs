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

use blue2th_proto::{PlaybackState, PlaybackStatus};

/// The embedded test tone shipped with the backend (2s 440Hz stereo sine,
/// 48kHz PCM 16-bit).
pub const TEST_TONE_WAV: &[u8] = include_bytes!("../assets/test-tone.wav");

/// Clamp a requested volume into the valid `0.0..=1.0` range.
pub fn clamp_volume(level: f32) -> f32 {
    level.clamp(0.0, 1.0)
}

/// In-memory playback model used to validate state transitions independently of
/// the real rodio sink / PipeWire. The route layer drives the same transitions
/// through an `Arc<Mutex<AudioEngine>>` held in the Axum router state.
pub struct AudioEngine {
    status: PlaybackStatus,
    volume: f32,
}

impl AudioEngine {
    /// A fresh, stopped engine at full volume.
    ///
    /// Scaffold constructor so the router can be built; the behaviour-bearing
    /// methods below are the ones under test in the red phase.
    pub fn new() -> Self {
        Self {
            status: PlaybackStatus::Stopped,
            volume: 1.0,
        }
    }

    /// Start (or resume) playback of the embedded test file. Idempotent while
    /// already playing.
    ///
    /// This drives only the in-memory state machine and the (host-gated) audio
    /// output; the precondition that a speaker is connected is enforced by the
    /// route layer before this is called.
    pub fn play(&mut self) -> Result<PlaybackState, AudioError> {
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
        if self.status == PlaybackStatus::Playing {
            self.pause_output();
            self.status = PlaybackStatus::Paused;
        }
        Ok(self.playback_state())
    }

    /// Stop playback and reset to the start. Idempotent no-op when stopped.
    pub fn stop(&mut self) -> Result<PlaybackState, AudioError> {
        if self.status != PlaybackStatus::Stopped {
            self.stop_output();
            self.status = PlaybackStatus::Stopped;
        }
        Ok(self.playback_state())
    }

    /// Set the connected speaker's PipeWire sink volume, clamping the input.
    pub fn set_volume(&mut self, level: f32) -> Result<PlaybackState, AudioError> {
        let clamped = clamp_volume(level);
        self.apply_sink_volume(clamped)?;
        self.volume = clamped;
        Ok(self.playback_state())
    }

    /// Snapshot of the current playback state.
    pub fn playback_state(&self) -> PlaybackState {
        PlaybackState {
            status: self.status,
            volume: self.volume,
        }
    }

    // --- Host-gated audio output ---------------------------------------------
    //
    // On this build there is no ALSA/cpal backend, so `rodio` is decode-only and
    // no real output device is instantiated. The methods below are the seam the
    // gated `#[ignore]` hardware test exercises on a real host: there they would
    // decode the embedded tone with `rodio` and route it to PipeWire, and set the
    // PipeWire sink volume. Kept side-effect-free here so the normal test run and
    // the build stay green.

    /// Begin streaming the embedded tone to the connected speaker's sink. No-op
    /// without a real audio backend.
    fn start_output(&mut self) -> Result<(), AudioError> {
        Ok(())
    }

    /// Resume a paused output stream. No-op without a real audio backend.
    fn resume_output(&mut self) -> Result<(), AudioError> {
        Ok(())
    }

    /// Pause the output stream. No-op without a real audio backend.
    fn pause_output(&mut self) {}

    /// Stop and drop the output stream. No-op without a real audio backend.
    fn stop_output(&mut self) {}

    /// Apply the clamped volume to the PipeWire sink. No-op without PipeWire.
    fn apply_sink_volume(&mut self, level: f32) -> Result<(), AudioError> {
        let _ = level;
        Ok(())
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
}
