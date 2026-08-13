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
pub fn build_librespot_args(device_name: &str, sink_name: &str, cache_dir: &str) -> Vec<String> {
    vec![
        "--name".to_string(),
        device_name.to_string(),
        "--backend".to_string(),
        "pulseaudio".to_string(),
        "--device".to_string(),
        sink_name.to_string(),
        // Cache the Spotify credentials: in plain zeroconf mode librespot only
        // advertises over mDNS and is NOT logged into the account, so it never
        // appears in `GET /me/player/devices` and the Web API cannot target it.
        // Once the user has picked blue2th-PC in a Spotify client, the cached
        // credentials let librespot log in by itself on every later start.
        "--system-cache".to_string(),
        cache_dir.to_string(),
    ]
}

/// Directory where `librespot` caches the Spotify credentials, honouring
/// `XDG_CACHE_HOME` and falling back to `~/.cache` (then the current directory
/// if even `HOME` is unset).
pub fn librespot_cache_dir() -> String {
    let base = std::env::var("XDG_CACHE_HOME")
        .ok()
        .filter(|p| !p.trim().is_empty())
        .or_else(|| std::env::var("HOME").ok().map(|h| format!("{h}/.cache")))
        .unwrap_or_else(|| ".".to_string());
    format!("{base}/blue2th/librespot")
}

/// Resolve the PipeWire sink `librespot` should feed for the current selection:
/// the `blue2th_combined` null sink for two targets, or the single speaker's
/// `bluez_output.*` sink prefix for one. Pure — performs no I/O.
pub fn spotify_target_sink(speakers: &[SpeakerTarget]) -> String {
    match speakers {
        [] => String::new(),
        // Fan-out, or a single speaker carrying an offset: both go through the
        // combined sink, the only place where the offset exists (loopback latency).
        _ if crate::audio::needs_combined(speakers) => COMBINED_SINK_NAME.to_string(),
        // A single target with no offset feeds its own `bluez_output.*` sink.
        [only, ..] => crate::audio::bluez_sink_prefix(&only.address),
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
    /// The PipeWire sink the running `librespot` was pointed at (`--device`),
    /// so a selection change that moves the sink can trigger a respawn.
    sink: Option<String>,
}

impl SpotifyBackend {
    /// A fresh backend with no subprocess (status `Stopped`).
    pub fn new() -> Self {
        Self {
            child: None,
            device_name: SPOTIFY_DEVICE_NAME.to_string(),
            sink: None,
        }
    }

    /// The sink the running subprocess feeds, or `None` while stopped.
    pub fn current_sink(&self) -> Option<&str> {
        self.child.as_ref().and(self.sink.as_deref())
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
        crate::audio::route_for_targets(speakers)
            .map_err(|e| SpotifyError::Spawn(e.to_string()))?;

        let sink = spotify_target_sink(speakers);
        let args = build_librespot_args(&self.device_name, &sink, &librespot_cache_dir());
        let child = std::process::Command::new("librespot")
            .args(&args)
            .spawn()
            .map_err(map_spawn_error)?;
        self.child = Some(child);
        // Remember where librespot was pointed: `--device` is fixed at spawn, so a
        // later selection change that moves the sink requires a respawn.
        self.sink = Some(sink);
        Ok(self.status())
    }

    /// Kill the subprocess and return to `Stopped` (idempotent while stopped).
    pub fn stop(&mut self) -> Result<SpotifyState, SpotifyError> {
        if let Some(mut child) = self.child.take() {
            // Best-effort teardown: the process may already have exited.
            let _ = child.kill();
            let _ = child.wait();
        }
        self.sink = None;
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
        let args = build_librespot_args(SPOTIFY_DEVICE_NAME, COMBINED_SINK_NAME, "/tmp/cache");
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

    // Criterion: the argv caches credentials, so librespot logs into the account
    // by itself on later starts and shows up in `GET /me/player/devices` — without
    // it the Web API cannot target blue2th-PC and transport hits the wrong device.
    #[test]
    fn test_build_librespot_args_caches_credentials() {
        let args = build_librespot_args(SPOTIFY_DEVICE_NAME, COMBINED_SINK_NAME, "/tmp/cache");
        let flag = args
            .iter()
            .position(|a| a == "--system-cache")
            .expect("argv must carry --system-cache");
        assert_eq!(
            args.get(flag + 1).map(String::as_str),
            Some("/tmp/cache"),
            "--system-cache must be followed by the cache dir: {args:?}"
        );
    }

    // Criterion: the cache dir honours XDG_CACHE_HOME and is app-scoped, so the
    // credentials do not land in a random working directory.
    #[test]
    fn test_librespot_cache_dir_is_app_scoped() {
        let dir = librespot_cache_dir();
        assert!(
            dir.ends_with("/blue2th/librespot"),
            "cache dir must be app-scoped, got {dir}"
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

    // Criterion: a single target carrying an offset goes through the combined
    // sink, the only routing where the offset exists (as loopback latency).
    // Routed straight to its bluez sink, the offset would be silently ignored.
    #[test]
    fn test_spotify_target_sink_single_target_with_offset_is_combined() {
        let delayed = SpeakerTarget {
            address: A.to_string(),
            offset_ms: 750,
        };
        assert_eq!(spotify_target_sink(&[delayed]), COMBINED_SINK_NAME);
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
