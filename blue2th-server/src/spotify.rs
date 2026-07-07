//! Spotify source backend (phase 5.1): manage a `librespot` **subprocess** that
//! advertises the PC as a Spotify Connect device (`blue2th-PC`) and feeds its
//! PipeWire output into the existing audio graph.
//!
//! The crate never links `librespot`; it is an external runtime binary on `PATH`.
//! Interaction is argv + process lifecycle. The pure helpers here
//! ([`build_librespot_args`], [`spotify_target_sink`], [`map_spawn_error`],
//! [`should_spawn`]) are unit-testable; the real spawn/kill/liveness path is a
//! manual hardware/process seam validated on a live setup.

use std::process::Child;

use blue2th_proto::{SpeakerTarget, SpotifyState, SpotifyStatus};

/// The Spotify Connect device name the PC advertises.
pub const SPOTIFY_DEVICE_NAME: &str = "blue2th-PC";

/// Node name of the combined sink used when two speakers are targeted (matches
/// the `blue2th_combined` convention from `audio.rs`).
pub const COMBINED_SINK_NAME: &str = "blue2th_combined";

/// Errors raised while activating/deactivating the Spotify source backend.
#[derive(Debug)]
pub enum SpotifyError {
    /// No speaker is selected as a playback target — nowhere to route Spotify to.
    NoSpeakerSelected,
    /// The `librespot` binary was not found on `PATH` (spawn `NotFound`).
    BackendMissing,
    /// Any other spawn/exec failure (permission, exec error…), carrying the OS message.
    Spawn(String),
}

impl std::fmt::Display for SpotifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpotifyError::NoSpeakerSelected => {
                write!(f, "no speaker selected as a playback target")
            },
            SpotifyError::BackendMissing => {
                write!(f, "Spotify backend unavailable (librespot not found)")
            },
            SpotifyError::Spawn(msg) => write!(f, "failed to spawn Spotify backend: {msg}"),
        }
    }
}

impl std::error::Error for SpotifyError {}

/// Build the argv for the `librespot` subprocess: a Connect device named
/// `device_name`, the PulseAudio/PipeWire backend, and the output pointed at
/// `sink_name`. Pure — performs no I/O.
pub fn build_librespot_args(device_name: &str, sink_name: &str) -> Vec<String> {
    vec![
        "--name".to_string(),
        device_name.to_string(),
        "--backend".to_string(),
        "pulseaudio".to_string(),
        "--device".to_string(),
        sink_name.to_string(),
    ]
}

/// Resolve the PipeWire sink `librespot` should feed for the current selection:
/// the `blue2th_combined` null sink for two targets, or the single speaker's
/// `bluez_output.*` sink prefix for one. Pure — performs no I/O.
pub fn spotify_target_sink(speakers: &[SpeakerTarget]) -> String {
    match speakers {
        // Two (or more) targets fan out through the shared combined sink.
        [_, _, ..] => COMBINED_SINK_NAME.to_string(),
        // A single target feeds its own `bluez_output.*` sink directly.
        [only] => crate::audio::bluez_sink_prefix(&only.address),
        [] => String::new(),
    }
}

/// Map a spawn `io::Error` to a typed [`SpotifyError`]: `NotFound` (the binary is
/// missing) becomes [`SpotifyError::BackendMissing`]; anything else becomes
/// [`SpotifyError::Spawn`] carrying the OS message. Pure.
pub fn map_spawn_error(err: std::io::Error) -> SpotifyError {
    match err.kind() {
        std::io::ErrorKind::NotFound => SpotifyError::BackendMissing,
        _ => SpotifyError::Spawn(err.to_string()),
    }
}

/// Idempotence decision: whether `start` should actually spawn a process given
/// the current status. Already `Running` ⇒ do not spawn. Pure.
pub fn should_spawn(current: SpotifyStatus) -> bool {
    !matches!(current, SpotifyStatus::Running)
}

/// Owns the `librespot` subprocess lifecycle. Held behind the router's
/// `Arc<Mutex<_>>`. A dead child must never poison the server, so `poll_liveness`
/// reconciles the state back to `Stopped` once the child exits.
pub struct SpotifyBackend {
    /// The running subprocess, if any.
    child: Option<Child>,
    /// The advertised Connect device name.
    device_name: String,
}

impl SpotifyBackend {
    /// A fresh backend with no subprocess (status `Stopped`).
    pub fn new() -> Self {
        Self {
            child: None,
            device_name: SPOTIFY_DEVICE_NAME.to_string(),
        }
    }

    /// Current backend state (derived from whether a live child is held).
    pub fn status(&self) -> SpotifyState {
        let status = if self.child.is_some() {
            SpotifyStatus::Running
        } else {
            SpotifyStatus::Stopped
        };
        SpotifyState {
            status,
            // Owned copy: `SpotifyState` is a plain DTO handed back to the caller.
            device_name: self.device_name.clone(),
        }
    }

    /// Establish routing for the selection and spawn `librespot` (idempotent while
    /// already running). The real spawn is a manual process seam.
    pub fn start(&mut self, speakers: &[SpeakerTarget]) -> Result<SpotifyState, SpotifyError> {
        if speakers.is_empty() {
            return Err(SpotifyError::NoSpeakerSelected);
        }
        // Reconcile first so a self-exited child does not make us skip a respawn.
        self.poll_liveness();
        if !should_spawn(self.status().status) {
            return Ok(self.status());
        }

        // Establish PipeWire routing for the selection (single sink vs combined).
        match speakers {
            [_, _, ..] => {
                crate::audio::route_to_combined(&crate::audio::combine_sink_plan(speakers))
                    .map_err(|e| SpotifyError::Spawn(e.to_string()))?
            },
            [only] => crate::audio::route_to_speaker(&only.address)
                .map_err(|e| SpotifyError::Spawn(e.to_string()))?,
            [] => return Err(SpotifyError::NoSpeakerSelected),
        }

        let sink = spotify_target_sink(speakers);
        let args = build_librespot_args(&self.device_name, &sink);
        let child = std::process::Command::new("librespot")
            .args(&args)
            .spawn()
            .map_err(map_spawn_error)?;
        self.child = Some(child);
        Ok(self.status())
    }

    /// Kill the subprocess and return to `Stopped` (idempotent while stopped).
    pub fn stop(&mut self) -> Result<SpotifyState, SpotifyError> {
        if let Some(mut child) = self.child.take() {
            // Best-effort teardown: the process may already have exited.
            let _ = child.kill();
            let _ = child.wait();
        }
        Ok(self.status())
    }

    /// Detect a subprocess that exited on its own and reset the state to
    /// `Stopped`, then return the reconciled state.
    pub fn poll_liveness(&mut self) -> SpotifyState {
        if let Some(child) = self.child.as_mut() {
            if matches!(child.try_wait(), Ok(Some(_)) | Err(_)) {
                self.child = None;
            }
        }
        self.status()
    }
}

impl Default for SpotifyBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "AA:BB:CC:DD:EE:FF";
    const B: &str = "11:22:33:44:55:66";

    fn target(addr: &str) -> SpeakerTarget {
        SpeakerTarget {
            address: addr.to_string(),
            offset_ms: 0,
        }
    }

    // Criterion: `build_librespot_args(device_name, sink_name)` yields the expected
    // argv — `--name blue2th-PC`, `--backend pulseaudio`, and the sink name.
    #[test]
    fn test_build_librespot_args_includes_name_backend_and_sink() {
        let args = build_librespot_args(SPOTIFY_DEVICE_NAME, COMBINED_SINK_NAME);
        assert!(
            args.iter().any(|a| a == "--name"),
            "argv must carry --name: {args:?}"
        );
        assert!(
            args.iter().any(|a| a == "blue2th-PC"),
            "argv must advertise blue2th-PC: {args:?}"
        );
        assert!(
            args.iter().any(|a| a == "--backend"),
            "argv must carry --backend: {args:?}"
        );
        assert!(
            args.iter().any(|a| a == "pulseaudio"),
            "argv must use the pulseaudio backend: {args:?}"
        );
        assert!(
            args.iter().any(|a| a.contains(COMBINED_SINK_NAME)),
            "argv must point the output at the sink: {args:?}"
        );
    }

    // Criterion: `spotify_target_sink(&speakers)` returns `blue2th_combined` for
    // two targets.
    #[test]
    fn test_spotify_target_sink_two_targets_is_combined() {
        let sink = spotify_target_sink(&[target(A), target(B)]);
        assert_eq!(sink, COMBINED_SINK_NAME);
    }

    // Criterion: `spotify_target_sink(&speakers)` returns the speaker's
    // `bluez_output.*` prefix for a single target.
    #[test]
    fn test_spotify_target_sink_single_target_is_bluez_prefix() {
        let sink = spotify_target_sink(&[target(A)]);
        assert_eq!(sink, crate::audio::bluez_sink_prefix(A));
        assert!(
            sink.starts_with("bluez_output."),
            "single-target sink must be a bluez_output.* prefix, got {sink}"
        );
    }

    // Criterion: a `NotFound` spawn error maps to the `BackendMissing` variant.
    #[test]
    fn test_map_spawn_error_not_found_is_backend_missing() {
        let err = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert!(matches!(map_spawn_error(err), SpotifyError::BackendMissing));
    }

    // Criterion: other spawn failures (permission, exec error) map to `Spawn`.
    #[test]
    fn test_map_spawn_error_other_is_spawn() {
        let err = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert!(matches!(map_spawn_error(err), SpotifyError::Spawn(_)));
    }

    // Criterion: `GET /spotify/status` on a fresh server returns `Stopped` — a
    // brand-new backend reports `Stopped` and the `blue2th-PC` device name.
    #[test]
    fn test_new_backend_reports_stopped() {
        let backend = SpotifyBackend::new();
        let state = backend.status();
        assert_eq!(state.status, SpotifyStatus::Stopped);
        assert_eq!(state.device_name, SPOTIFY_DEVICE_NAME);
    }

    // Criterion: starting while `Running` is idempotent (the decision helper does
    // not re-spawn).
    #[test]
    fn test_should_spawn_is_false_when_running() {
        assert!(!should_spawn(SpotifyStatus::Running));
    }

    // Criterion: when stopped, the decision helper allows a spawn.
    #[test]
    fn test_should_spawn_is_true_when_stopped() {
        assert!(should_spawn(SpotifyStatus::Stopped));
    }
}
