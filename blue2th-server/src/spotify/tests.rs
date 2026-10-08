// SPDX-License-Identifier: MIT OR Apache-2.0

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

// ─── #147: what a start checks, apart from its routing and its spawn ────

// Criterion (#147, guard, a test never starts the real backend): under
// this crate's tests the program a start spawns resolves to nothing, so
// a spawn reached by mistake is `BackendMissing` — not a `librespot`
// running with the operator's credentials. The near miss is the real
// name, which is on the `PATH` of any machine the app is used from.
#[test]
fn test_the_program_a_start_spawns_under_test_is_not_the_real_librespot() {
    assert_ne!(LIBRESPOT_PROGRAM, "librespot");

    let spawned = spawn_bound_to_this_thread(LIBRESPOT_PROGRAM, &[]).map_err(map_spawn_error);

    assert!(
        matches!(spawned, Err(SpotifyError::BackendMissing)),
        "the stand-in name resolved to a program"
    );
}

// Criterion (#147): "does this start need a spawn" refuses an empty
// selection, as `start` does — whether the backend is stopped or already
// running.
#[test]
fn test_needs_spawn_with_an_empty_selection_is_refused() {
    let mut backend = SpotifyBackend::new();

    assert!(matches!(
        backend.needs_spawn(&[]),
        Err(SpotifyError::NoSpeakerSelected)
    ));

    let child = spawn_bound_to_this_thread("sleep", &["30".to_string()]).expect("spawn sleep");
    backend.adopt_child_for_test(child);
    assert!(matches!(
        backend.needs_spawn(&[]),
        Err(SpotifyError::NoSpeakerSelected)
    ));
    backend.stop().expect("the backend stops");
}

// Criterion (#147): a stopped backend with a selection needs a spawn; one
// holding a live child does not — a start is idempotent while running.
#[test]
fn test_needs_spawn_of_a_stopped_backend_is_true_and_of_a_running_one_is_false() {
    let mut backend = SpotifyBackend::new();

    assert!(matches!(backend.needs_spawn(&[target(A)]), Ok(true)));

    let child = spawn_bound_to_this_thread("sleep", &["30".to_string()]).expect("spawn sleep");
    backend.adopt_child_for_test(child);
    assert!(matches!(
        backend.needs_spawn(&[target(A), target(B)]),
        Ok(false)
    ));
    assert_eq!(backend.status().status, SpotifyStatus::Running);
    backend.stop().expect("the backend stops");
}

// Criterion (#147): liveness is reconciled before the answer — a child
// that exited on its own does not make a respawn be skipped. The child
// here has exited before the backend is asked, and the backend is left
// `Stopped`.
#[test]
fn test_needs_spawn_reconciles_a_child_that_exited_on_its_own() {
    let mut child = spawn_bound_to_this_thread("true", &[]).expect("spawn true");
    child.wait().expect("the child exits");
    let mut backend = SpotifyBackend::new();
    backend.adopt_child_for_test(child);

    assert!(matches!(backend.needs_spawn(&[target(A)]), Ok(true)));
    assert_eq!(backend.status().status, SpotifyStatus::Stopped);
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
// sink, the only routing where the offset exists (as a branch delay).
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

// Criterion: `stop()` on a backend that never started is a no-op reporting
// `Stopped`, not an error. The empty-selection branch of
// `apply_selection_change` runs on every deselection of the last speaker,
// playing or not, so a `stop()` that failed with nothing to kill would turn
// an ordinary deselection into an error path.
#[test]
fn test_stop_on_a_fresh_backend_is_a_no_op_reporting_stopped() {
    let mut backend = SpotifyBackend::new();
    let state = backend
        .stop()
        .expect("stop must not fail with no child held");
    assert_eq!(state.status, SpotifyStatus::Stopped);
    assert_eq!(
        backend.status().status,
        SpotifyStatus::Stopped,
        "the backend itself must report Stopped afterwards, not only the returned state"
    );
}

// Criterion: `stop()` leaves the backend reporting `Stopped` and forgets the
// sink it was pointed at, so nothing later believes a subprocess is still
// targeting it. Asserted through the public `current_sink()`, which is what
// `resync_spotify_sink` reads to decide whether a respawn is needed.
#[test]
fn test_stop_forgets_the_sink_it_was_pointed_at() {
    // A backend that was pointed at the combined sink, built directly rather
    // than through `start`: spawning `librespot` is a hardware/process seam
    // no test may cross, and the field is all `stop` has to clear.
    let mut backend = SpotifyBackend {
        child: None,
        device_name: SPOTIFY_DEVICE_NAME.to_string(),
        sink: Some(COMBINED_SINK_NAME.to_string()),
    };

    let state = backend.stop().expect("stop must not fail");

    assert_eq!(state.status, SpotifyStatus::Stopped);
    assert_eq!(
        backend.current_sink(),
        None,
        "no sink may survive a stop: the combined sink is unloaded right after"
    );
    assert!(
        backend.sink.is_none(),
        "the remembered sink must be cleared, not merely masked by the absent child"
    );
}

// Criterion: calling `stop()` twice is still `Stopped`. The empty branch is
// reached on every deselection of the last speaker, so the guarantee must not
// depend on being reached exactly once.
#[test]
fn test_stop_twice_still_reports_stopped() {
    let mut backend = SpotifyBackend {
        child: None,
        device_name: SPOTIFY_DEVICE_NAME.to_string(),
        sink: Some(COMBINED_SINK_NAME.to_string()),
    };

    let first = backend.stop().expect("first stop must not fail");
    let second = backend.stop().expect("second stop must not fail");

    assert_eq!(first.status, SpotifyStatus::Stopped);
    assert_eq!(second.status, SpotifyStatus::Stopped);
    assert_eq!(backend.current_sink(), None);
}

// Criterion: `stop()` leaves the backend reporting `Stopped` — and nothing
// else. Since the empty-selection branch now stops the subprocess on every
// deselection of the last speaker, a `stop` that also reset the advertised
// name would silently drop the name the app configured (phase 6.2) the first
// time a user switched their speakers off.
#[test]
fn test_stop_keeps_the_configured_device_name() {
    let mut backend = SpotifyBackend::with_name("Salon");
    let state = backend.stop().expect("stop must not fail");
    assert_eq!(state.device_name, "Salon");
    assert_eq!(backend.device_name(), "Salon");
}

/// The whole of `spotify.rs`. Its tests live in this file (#172), so none of
/// them is counted. Cutting at the first `#[cfg(test)]` used to stop at the
/// test-only `adopt_child_for_test`, which hid every function after it.
fn production_source() -> &'static str {
    include_str!("../spotify.rs")
}

/// Poll `child` for up to `budget`, returning its exit status once it has
/// one. `None` means it was still running when the budget ran out.
fn wait_up_to(child: &mut Child, budget: std::time::Duration) -> Option<std::process::ExitStatus> {
    let deadline = std::time::Instant::now() + budget;
    while std::time::Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    None
}

// Criterion (#122): a child spawned through the seam from a thread that then
// exits is gone — terminated by SIGTERM (signal 15) — within 2 s. `sleep 30`
// stands in for `librespot`: it is in coreutils on every CI runner and would
// outlive the test on its own.
#[test]
fn test_spawn_bound_to_this_thread_terminates_the_child_when_the_spawning_thread_exits() {
    use std::os::unix::process::ExitStatusExt;

    let spawner = std::thread::spawn(|| {
        spawn_bound_to_this_thread("sleep", &["30".to_string()]).expect("spawn sleep")
    });
    let mut child = spawner.join().expect("spawning thread joined");

    let status = wait_up_to(&mut child, std::time::Duration::from_secs(2));
    // Never leave a 30 s sleeper behind, whatever the assertion says.
    let _ = child.kill();
    let _ = child.wait();

    assert!(
        status.is_some(),
        "the child outlived the thread that spawned it: no parent-death signal"
    );
    assert_eq!(
        status.and_then(|s| s.signal()),
        Some(15),
        "the child must be terminated by SIGTERM, got {status:?}"
    );
}

// Criterion (#122): the binding is to the thread's *life*, not to the spawn
// call — a child spawned from a thread that keeps living is still running
// 200 ms later.
#[test]
fn test_spawn_bound_to_this_thread_keeps_the_child_while_the_thread_lives() {
    let mut child = spawn_bound_to_this_thread("sleep", &["30".to_string()]).expect("spawn sleep");

    std::thread::sleep(std::time::Duration::from_millis(200));
    let exited = child.try_wait().expect("try_wait");
    let _ = child.kill();
    let _ = child.wait();

    assert!(
        exited.is_none(),
        "the child died while its spawning thread was alive: {exited:?}"
    );
}

// Criterion (#122): `SpotifyBackend::spawn_towards` spawns through the seam and
// nothing else in this module builds a `Command` — the seam is the one place
// the parent-death signal is set, so a second spawn site would be an
// unprotected `librespot`. The real spawn reaches PipeWire, so the rule is
// pinned on the source rather than exercised.
#[test]
fn test_start_spawns_librespot_only_through_the_bound_seam() {
    let source = production_source();

    let command_sites = source.matches("Command::new(").count();
    assert_eq!(
        command_sites, 1,
        "exactly one `Command::new(` — inside `spawn_bound_to_this_thread` — is allowed, found {command_sites}"
    );

    // One definition plus at least one call site (`spawn_towards`).
    let seam_uses = source.matches("spawn_bound_to_this_thread(").count();
    assert!(
        seam_uses >= 2,
        "`spawn_towards` must call `spawn_bound_to_this_thread`, found {seam_uses} mention(s) (the definition alone is 1)"
    );
}
