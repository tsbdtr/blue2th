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
        // in the stream volume `pw-dump` shows — it reads 0.00 dB while the samples
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

/// Spawn `program` with `args`, bound to the lifetime of **the calling thread**
/// (#122): the child receives SIGTERM when that thread ends, whether the server
/// exits, crashes or is killed.
///
/// Linux ties `PR_SET_PDEATHSIG` to the *thread* that forked, not to the
/// process. Every spawn happens on a tokio worker thread, which lives as long
/// as the runtime, so the binding lasts as long as the server does. A spawn
/// must never move to `spawn_blocking`: its pool threads exit after an idle
/// timeout and would take the child with them mid-playback.
pub fn spawn_bound_to_this_thread(program: &str, args: &[String]) -> std::io::Result<Child> {
    use std::os::unix::process::CommandExt;

    let mut command = std::process::Command::new(program);
    command.args(args);
    // SAFETY: the closure runs in the forked child, between `fork` and `exec`,
    // where only async-signal-safe calls are allowed: no allocation, no locks,
    // no logging, nothing that touches the parent's runtime. `prctl` is a
    // single syscall and qualifies. Its failure is mapped to an `io::Error`, so
    // the spawn fails rather than leaving an unprotected child behind.
    unsafe {
        command.pre_exec(|| {
            nix::sys::prctl::set_pdeathsig(nix::sys::signal::Signal::SIGTERM)
                .map_err(std::io::Error::from)
        });
    }
    command.spawn()
}

/// The program a start spawns: `librespot` — except under this crate's own
/// tests, where the name resolves to nothing. A test that reaches the spawn —
/// a regression, a red phase — gets [`SpotifyError::BackendMissing`], never
/// the operator's own `librespot` started with its cached credentials (#147).
///
/// A `cfg!` expression rather than two attributed constants: the tests of
/// #122 read this file's production half as everything above the first test
/// attribute.
const LIBRESPOT_PROGRAM: &str = if cfg!(test) {
    "blue2th-no-librespot-under-test"
} else {
    "librespot"
};

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

    /// Whether a start towards `speakers` has a `librespot` to spawn (#147):
    /// `Err` for an empty selection, `Ok(false)` while a live child is held. A
    /// child that exited on its own is reconciled first, so it does not make
    /// a respawn be skipped.
    pub fn needs_spawn(&mut self, speakers: &[SpeakerTarget]) -> Result<bool, SpotifyError> {
        if speakers.is_empty() {
            return Err(SpotifyError::NoSpeakerSelected);
        }
        Ok(should_spawn(self.poll_liveness().status))
    }

    /// Spawn `librespot` pointed at `resolved`, the live node the routing
    /// message answered for `speakers` (#147). The routing is the caller's:
    /// it runs in the graph thread, and the spawn must not — the child is
    /// bound to the thread that forks it (#122). The real spawn is a manual
    /// process seam.
    pub fn spawn_towards(
        &mut self,
        resolved: &str,
        speakers: &[SpeakerTarget],
    ) -> Result<SpotifyState, SpotifyError> {
        // The argv comes from the same seam the tests pin, so the spawned
        // process can never drift from `--name <configured name>`.
        let args = self.librespot_args(resolved);
        let child =
            spawn_bound_to_this_thread(LIBRESPOT_PROGRAM, &args).map_err(map_spawn_error)?;
        self.child = Some(child);
        // Remember the *logical* target, not the resolved node: `resync_spotify_sink`
        // compares this against `spotify_target_sink(...)`, and storing the resolved
        // name would make them never compare equal — respawning librespot, hence
        // cutting the audio, on every selection change.
        self.sink = Some(spotify_target_sink(speakers));
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

    /// Hold `child` as if [`Self::spawn_towards`] had spawned it, so a test can drive
    /// the stop paths against a live subprocess without reaching PipeWire.
    #[cfg(test)]
    pub(crate) fn adopt_child_for_test(&mut self, child: Child) {
        self.child = Some(child);
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
mod tests;
