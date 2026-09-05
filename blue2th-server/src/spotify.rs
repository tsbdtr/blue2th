// SPDX-License-Identifier: MIT OR Apache-2.0

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

/// Node name of the combined sink every non-empty selection is routed through
/// (matches the `blue2th_combined` convention from `audio.rs`).
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
        // Autoplay off, explicitly rather than following the account setting:
        // Spotify refuses to hand librespot the `spotify:station:track:…`
        // context it asks for at the end of a queue ("context is not available.
        // type: Autoplay"), so the feature does not work — it only logs two
        // errors and an invalid spirc state on every play. Off, the behaviour is
        // the same and the log says what happened.
        "--autoplay".to_string(),
        "off".to_string(),
        // Start at full scale. librespot applies its own gain to the PCM *inside
        // its process*, before PulseAudio sees it, so the attenuation is invisible
        // in `pactl list sink-inputs` — the stream reads 0.00 dB while the samples
        // are already quieter. Its default is 50% on a logarithmic curve, i.e.
        // roughly -30 dB, which is why the backend sounded markedly softer than the
        // same speaker paired straight to a phone.
        //
        // Only the *starting* point: the Spotify client can still lower it, and no
        // argv takes that authority away — `--volume-ctrl fixed` was tried and is
        // inert with this backend. Who owns the Spotify volume afterwards is the
        // open question in #58.
        "--initial-volume".to_string(),
        "100".to_string(),
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

/// The **logical** playback target for the current selection: the
/// `blue2th_combined` null sink for every non-empty selection, and an empty
/// string when nothing is selected.
///
/// One shape for every selection is what keeps the target invariant when a
/// speaker is added or dropped, so `resync_spotify_sink` never respawns
/// `librespot` over a selection change (#70). Pure — performs no I/O, which is
/// what lets `tests/restore.rs` call it with no hardware.
pub fn spotify_target_sink(speakers: &[SpeakerTarget]) -> String {
    if speakers.is_empty() {
        return String::new();
    }
    COMBINED_SINK_NAME.to_string()
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

/// Rename decision (phase 6.2): whether a rename must restart `librespot`.
///
/// `--name` is bound at spawn, so a running backend has to be respawned for the
/// Connect device to come back under the new name; a stopped one simply picks
/// the new name up at its next start. Pure.
pub fn should_restart_for_rename(current: SpotifyStatus, running: &str, wanted: &str) -> bool {
    matches!(current, SpotifyStatus::Running) && running != wanted
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
    /// A fresh backend with no subprocess (status `Stopped`), advertising the
    /// default name until the app configures another one.
    pub fn new() -> Self {
        Self::with_name(SPOTIFY_DEVICE_NAME)
    }

    /// A backend advertising an explicit Connect device name (phase 6.2: the
    /// name the app configured, restored from the server's own store).
    pub fn with_name(device_name: &str) -> Self {
        Self {
            child: None,
            device_name: device_name.to_string(),
            sink: None,
        }
    }

    /// Adopt a new Connect device name. The caller restarts the subprocess when
    /// [`should_restart_for_rename`] says so — `--name` is fixed at spawn.
    pub fn set_device_name(&mut self, device_name: &str) {
        self.device_name = device_name.to_string();
    }

    /// The Connect device name currently advertised.
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// The argv the next spawn would use for an **already-resolved** sink node
    /// name — the seam that pins `librespot --name <configured name> --device
    /// <live node>` without spawning anything. Pure.
    pub fn librespot_args(&self, sink: &str) -> Vec<String> {
        build_librespot_args(&self.device_name, sink, &librespot_cache_dir())
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

        // Establish PipeWire routing for the selection (the combined sink).
        crate::audio::route_for_targets(speakers)
            .map_err(|e| SpotifyError::Spawn(e.to_string()))?;

        let sink = spotify_target_sink(speakers);
        // `spotify_target_sink` yields a *logical* target. Resolve it here, at the
        // argv, rather than in that pure function; a failure is reported instead
        // of letting librespot fall back to the default sink.
        let resolved = crate::audio::resolve_target_sink(&sink)
            .map_err(|e| SpotifyError::Spawn(e.to_string()))?;
        // The argv comes from the same seam the tests pin, so the spawned
        // process can never drift from `--name <configured name>`.
        let args = self.librespot_args(&resolved);
        let child = std::process::Command::new("librespot")
            .args(&args)
            .spawn()
            .map_err(map_spawn_error)?;
        self.child = Some(child);
        // Remember the *logical* target, not the resolved node: `resync_spotify_sink`
        // compares this against `spotify_target_sink(...)`, and storing the resolved
        // name would make them never compare equal — respawning librespot, hence
        // cutting the audio, on every selection change.
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
    /// A live PipeWire node name as BlueZ creates it: the `bluez_output.<MAC>`
    /// prefix plus the card suffix `spotify_target_sink` does not carry.
    const RESOLVED_SINK: &str = "bluez_output.80_99_E7_63_50_29.1";

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

    // Criterion: autoplay is turned off explicitly. Spotify refuses librespot the
    // station context it asks for at the end of a queue, so following the account
    // setting only buys two errors and an invalid spirc state per play.
    #[test]
    fn test_build_librespot_args_turns_autoplay_off() {
        let args = build_librespot_args(SPOTIFY_DEVICE_NAME, COMBINED_SINK_NAME, "/tmp/cache");
        let flag = args
            .iter()
            .position(|a| a == "--autoplay")
            .expect("argv must carry --autoplay");
        assert_eq!(
            args.get(flag + 1).map(String::as_str),
            Some("off"),
            "--autoplay must be followed by off: {args:?}"
        );
    }

    // Criterion: `build_librespot_args` emits `--initial-volume` immediately
    // followed by `100`. librespot attenuates in-process — its default is 50% on a
    // *logarithmic* curve, roughly -30 dB rather than half amplitude — before
    // PulseAudio ever sees the samples, which is why the backend sounded quieter
    // than a direct phone connection while every PipeWire stage read 0.00 dB.
    #[test]
    fn test_build_librespot_args_starts_at_full_volume() {
        let args = build_librespot_args(SPOTIFY_DEVICE_NAME, COMBINED_SINK_NAME, "/tmp/cache");
        let flag = args
            .iter()
            .position(|a| a == "--initial-volume")
            .expect("argv must carry --initial-volume");
        assert_eq!(
            args.get(flag + 1).map(String::as_str),
            Some("100"),
            "--initial-volume must be followed by 100: {args:?}"
        );
    }

    // Criterion: `--volume-ctrl` is NOT emitted. It was tried and is inert with the
    // PulseAudio backend — with the flag in the running argv, the Spotify client
    // still drove the output to silence and the sink-input stayed at 0.00 dB. A
    // no-op flag pinned by a test would read as a solved problem, so this test
    // exists to keep it out.
    #[test]
    fn test_build_librespot_args_does_not_set_a_volume_control() {
        let args = build_librespot_args(SPOTIFY_DEVICE_NAME, COMBINED_SINK_NAME, "/tmp/cache");
        assert!(
            !args.iter().any(|a| a == "--volume-ctrl"),
            "--volume-ctrl is inert with this backend and must not ship: {args:?}"
        );
    }

    // Companion to the guard above, which compares whole arguments: clap also
    // accepts the attached spelling `--volume-ctrl=fixed`, a single argument that
    // an equality check on `--volume-ctrl` lets straight through. The flag must be
    // absent in that form too, or the guard only holds against the way it happened
    // to be written the first time.
    #[test]
    fn test_build_librespot_args_does_not_set_an_attached_volume_control() {
        let args = build_librespot_args(SPOTIFY_DEVICE_NAME, COMBINED_SINK_NAME, "/tmp/cache");
        assert!(
            !args.iter().any(|a| a.starts_with("--volume-ctrl=")),
            "--volume-ctrl must not ship in its attached form either: {args:?}"
        );
    }

    // Criterion: the rest of the argv is unchanged — `--name`, `--backend
    // pulseaudio`, `--device`, `--system-cache` and `--autoplay off` all still
    // present with the same values, so adding the volume flag cannot silently drop
    // a neighbouring one.
    #[test]
    fn test_build_librespot_args_keeps_every_other_flag() {
        let args = build_librespot_args(SPOTIFY_DEVICE_NAME, COMBINED_SINK_NAME, "/tmp/cache");
        for (flag, value) in [
            ("--name", SPOTIFY_DEVICE_NAME),
            ("--backend", "pulseaudio"),
            ("--device", COMBINED_SINK_NAME),
            ("--system-cache", "/tmp/cache"),
            ("--autoplay", "off"),
        ] {
            let at = args.iter().position(|a| a == flag);
            assert!(at.is_some(), "argv must carry {flag}: {args:?}");
            assert_eq!(
                at.and_then(|i| args.get(i + 1)).map(String::as_str),
                Some(value),
                "{flag} must be followed by {value}: {args:?}"
            );
        }
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

    // Criterion: `spotify_target_sink` returns the combined sink for a lone
    // speaker with no offset — the case that used to take the direct route, and
    // whose crossing back and forth is what respawned librespot (#70).
    #[test]
    fn test_spotify_target_sink_lone_target_without_offset_is_combined() {
        assert_eq!(spotify_target_sink(&[target(A)]), COMBINED_SINK_NAME);
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

    // Criterion: the logical target is *invariant* under a selection change, so
    // `resync_spotify_sink` never sees a difference and never respawns librespot
    // (#70). Asserted as an equality between the three selections rather than as
    // three constant assertions: the equality is the property that matters, and
    // it keeps holding if the sink is ever renamed, while three constants would
    // hide one case drifting away from the others.
    #[test]
    fn test_spotify_target_sink_is_invariant_across_non_empty_selections() {
        let lone_plain = spotify_target_sink(&[target(A)]);
        let lone_delayed = spotify_target_sink(&[SpeakerTarget {
            address: A.to_string(),
            offset_ms: 750,
        }]);
        let two = spotify_target_sink(&[target(A), target(B)]);

        assert_eq!(
            lone_plain, lone_delayed,
            "an offset on a lone speaker must not move the logical target"
        );
        assert_eq!(
            lone_plain, two,
            "adding or removing a speaker must not move the logical target"
        );
    }

    // Criterion: an empty selection still yields no target at all — the guard
    // that keeps `resync_spotify_sink` and the restore path from pointing
    // librespot at a sink when nothing is selected.
    #[test]
    fn test_spotify_target_sink_empty_selection_is_empty() {
        assert!(spotify_target_sink(&[]).is_empty());
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

    // Criterion (phase 6.2): `SpotifyBackend` advertises the *configured* name,
    // not the constant — `SPOTIFY_DEVICE_NAME` is only the default.
    #[test]
    fn test_with_name_advertises_the_configured_name() {
        let backend = SpotifyBackend::with_name("Salon");
        assert_eq!(backend.device_name(), "Salon");
        assert_eq!(backend.status().device_name, "Salon");
    }

    // Criterion (phase 6.2): a rename is adopted, so `GET /spotify/status`
    // reports the name the app configured.
    #[test]
    fn test_set_device_name_replaces_the_advertised_name() {
        let mut backend = SpotifyBackend::new();
        assert_eq!(backend.device_name(), SPOTIFY_DEVICE_NAME);
        backend.set_device_name("Bureau");
        assert_eq!(backend.status().device_name, "Bureau");
    }

    // Criterion (phase 6.2): `SpotifyBackend` spawns `librespot --name <configured
    // name>`, not the constant — the argv seam pins it without spawning.
    #[test]
    fn test_librespot_args_carry_the_configured_name() {
        let backend = SpotifyBackend::with_name("Salon");
        let args = backend.librespot_args(RESOLVED_SINK);
        let flag = args
            .iter()
            .position(|a| a == "--name")
            .expect("argv must carry --name");
        assert_eq!(
            args.get(flag + 1).map(String::as_str),
            Some("Salon"),
            "--name must be followed by the configured name: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a == SPOTIFY_DEVICE_NAME),
            "the default name must not leak into the argv once renamed: {args:?}"
        );
    }

    // Criterion: `librespot_args` takes the already-resolved sink and emits it
    // verbatim after `--device`, card suffix included. Handing `--device` a
    // prefix names no live node, and librespot silently falls back to the
    // default sink — the defect this change fixes.
    #[test]
    fn test_librespot_args_emit_the_resolved_sink_verbatim() {
        let backend = SpotifyBackend::with_name("Salon");
        let args = backend.librespot_args(RESOLVED_SINK);
        let flag = args
            .iter()
            .position(|a| a == "--device")
            .expect("argv must carry --device");
        assert_eq!(
            args.get(flag + 1).map(String::as_str),
            Some(RESOLVED_SINK),
            "--device must be followed by the resolved node name, card suffix included: {args:?}"
        );
    }

    // Criterion (non-nominal): the combined sink is already an exact node name,
    // so it travels through untouched — the path that always worked.
    #[test]
    fn test_librespot_args_pass_the_combined_sink_through_unchanged() {
        let backend = SpotifyBackend::with_name("Salon");
        let args = backend.librespot_args(COMBINED_SINK_NAME);
        let flag = args
            .iter()
            .position(|a| a == "--device")
            .expect("argv must carry --device");
        assert_eq!(
            args.get(flag + 1).map(String::as_str),
            Some(COMBINED_SINK_NAME),
            "argv must point the output at the combined sink: {args:?}"
        );
    }

    // Criterion: every other argv flag is unchanged once the sink is resolved —
    // `--initial-volume 100` still there, `--volume-ctrl` still absent in both
    // its spellings, and no neighbouring flag dropped.
    #[test]
    fn test_librespot_args_keep_every_other_flag_with_a_resolved_sink() {
        let backend = SpotifyBackend::with_name("Salon");
        let args = backend.librespot_args(RESOLVED_SINK);
        for (flag, value) in [
            ("--name", "Salon"),
            ("--backend", "pulseaudio"),
            ("--device", RESOLVED_SINK),
            ("--autoplay", "off"),
            ("--initial-volume", "100"),
        ] {
            let at = args.iter().position(|a| a == flag);
            assert!(at.is_some(), "argv must carry {flag}: {args:?}");
            assert_eq!(
                at.and_then(|i| args.get(i + 1)).map(String::as_str),
                Some(value),
                "{flag} must be followed by {value}: {args:?}"
            );
        }
        assert!(
            args.iter().any(|a| a == "--system-cache"),
            "argv must still cache the credentials: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a == "--volume-ctrl"),
            "--volume-ctrl is inert with this backend and must not ship: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a.starts_with("--volume-ctrl=")),
            "--volume-ctrl must not ship in its attached form either: {args:?}"
        );
    }

    // Criterion (phase 6.2): changing the name while the Spotify backend runs
    // restarts `librespot` — `--name` is bound at spawn.
    #[test]
    fn test_should_restart_for_rename_is_true_while_running_with_a_new_name() {
        assert!(should_restart_for_rename(
            SpotifyStatus::Running,
            "blue2th-PC",
            "Salon"
        ));
    }

    // Criterion (phase 6.2): renaming to the same name restarts nothing (no
    // gratuitous playback cut).
    #[test]
    fn test_should_restart_for_rename_is_false_when_the_name_is_unchanged() {
        assert!(!should_restart_for_rename(
            SpotifyStatus::Running,
            "Salon",
            "Salon"
        ));
    }

    // Criterion (phase 6.2): renaming while Spotify is stopped stores the name and
    // uses it at the next start — nothing to restart.
    #[test]
    fn test_should_restart_for_rename_is_false_while_stopped() {
        assert!(!should_restart_for_rename(
            SpotifyStatus::Stopped,
            "blue2th-PC",
            "Salon"
        ));
    }
}
