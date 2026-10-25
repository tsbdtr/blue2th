// SPDX-License-Identifier: MIT OR Apache-2.0

//! The built `blue2th-server` binary's command line (#170), end to end.
//!
//! The parse itself is tested on `Cli` in the crate's unit tests. What only the
//! binary shows is how `main` hands a refusal back: a `clap::Error` returned
//! through `main() -> Result<(), Box<dyn Error>>` still refuses, but exits 1
//! with its `Debug` dump (`ErrorInner { … }`) on stderr, `--help` included.
//!
//! A binary that does not parse first starts the real server and never exits.
//! So every run has a deadline, past which the child is killed and the test
//! fails, and an environment of its own: `HOME`, `XDG_STATE_HOME` and
//! `XDG_CACHE_HOME` point into a per-test directory, so such a server writes
//! its token and its stores there, never into the developer's state.

use std::collections::BTreeSet;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long a run may take before it is killed. `--help` and a refusal exit
/// in milliseconds; only a server that started anyway reaches this.
const DEADLINE: Duration = Duration::from_secs(10);

/// What a run of the binary left behind.
struct Run {
    /// The exit code, `None` when the child was killed at the deadline (or by
    /// a signal).
    code: Option<i32>,
    /// Whether the child had to be killed at the deadline.
    timed_out: bool,
    stdout: String,
    stderr: String,
    /// The names of the files left in the state directory
    /// (`$XDG_STATE_HOME/blue2th`), read before the per-test directory is
    /// removed. Empty when that directory does not exist.
    state_files: BTreeSet<String>,
    /// The bytes of `tls-key.pem` in the state directory, read before the
    /// per-test directory is removed; `None` when there is none (#195).
    tls_key: Option<Vec<u8>>,
}

/// Run the built binary with `args`, in an environment isolated under a
/// directory unique to `test_name`, removed before returning — on an error
/// too.
fn run_binary(test_name: &str, args: &[&str]) -> std::io::Result<Run> {
    run_binary_prepared(test_name, args, |_| Ok(()))
}

/// `run_binary`, with `prepare` run on the per-test directory between its
/// creation and the spawn — to lay out a state directory the binary finds at
/// start (#194).
fn run_binary_prepared(
    test_name: &str,
    args: &[&str],
    prepare: impl FnOnce(&Path) -> std::io::Result<()>,
) -> std::io::Result<Run> {
    run_binary_with_env(test_name, args, &[], prepare)
}

/// `run_binary_prepared`, with `env` set on the child on top of the isolated
/// environment (#195: `BLUE2TH_PLAIN_HTTP`).
fn run_binary_with_env(
    test_name: &str,
    args: &[&str],
    env: &[(&str, &str)],
    prepare: impl FnOnce(&Path) -> std::io::Result<()>,
) -> std::io::Result<Run> {
    let dir = std::env::temp_dir().join(format!("blue2th-cli-{test_name}-{}", std::process::id()));
    // A leftover from an interrupted run would be reused otherwise.
    let _ = std::fs::remove_dir_all(&dir);
    let run = std::fs::create_dir_all(&dir)
        .and_then(|()| prepare(&dir))
        .and_then(|()| run_in(&dir, args, env));
    let _ = std::fs::remove_dir_all(&dir);
    run
}

/// The names of the entries in `dir`, empty when it does not exist.
fn entry_names(dir: &Path) -> std::io::Result<BTreeSet<String>> {
    match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .map(|entry| entry.map(|e| e.file_name().to_string_lossy().into_owned()))
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeSet::new()),
        Err(e) => Err(e),
    }
}

/// Run the built binary with `args`, with `dir` as its home, state and cache,
/// killing it at the deadline.
///
/// The output goes to files rather than pipes: a grandchild (librespot) that
/// inherits a pipe would keep a reader blocked after the child is gone.
fn run_in(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> std::io::Result<Run> {
    let out_path = dir.join("stdout");
    let err_path = dir.join("stderr");
    let mut child = Command::new(env!("CARGO_BIN_EXE_blue2th-server"))
        .args(args)
        .env("HOME", dir)
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("XDG_CACHE_HOME", dir.join("cache"))
        // A debug build reads it; an inherited one would move a server started
        // by mistake onto the developer's chosen interface.
        .env_remove("BLUE2TH_BIND")
        // #195: an inherited one would turn a debug run into the plain mode.
        .env_remove("BLUE2TH_PLAIN_HTTP")
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(File::create(&out_path)?)
        .stderr(File::create(&err_path)?)
        .spawn()?;

    let started = Instant::now();
    let finished = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(Some(status)),
            Ok(None) if started.elapsed() < DEADLINE => {
                std::thread::sleep(Duration::from_millis(50));
            },
            // Past the deadline, or unable to tell: killed before the test
            // fails, so no stray server outlives it.
            outcome => {
                let _ = child.kill();
                let _ = child.wait();
                break outcome.map(|_| None);
            },
        }
    };
    let status = finished?;

    Ok(Run {
        code: status.and_then(|s| s.code()),
        timed_out: status.is_none(),
        stdout: std::fs::read_to_string(&out_path)?,
        stderr: std::fs::read_to_string(&err_path)?,
        state_files: entry_names(&dir.join("state").join("blue2th"))?,
        tls_key: std::fs::read(dir.join("state").join("blue2th").join("tls-key.pem")).ok(),
    })
}

// Criterion: the built binary run with `--help` exits 0 and its stdout names
// `--pair` and `--bind`. Guard ("`--help` exits 0"): near-miss is a `main`
// returning the `clap::Error`, which exits 1 on `--help`.
#[test]
fn test_binary_help_exits_zero_and_lists_both_options() {
    let run = run_binary("help", &["--help"]).unwrap();
    assert!(
        !run.timed_out,
        "--help must exit before the {DEADLINE:?} deadline, not start the server; stderr: {}",
        run.stderr
    );
    assert_eq!(run.code, Some(0), "stderr: {}", run.stderr);
    for name in ["--pair", "--bind"] {
        assert!(
            run.stdout.contains(name),
            "the help on stdout must name {name}, got {:?}",
            run.stdout
        );
    }
}

// Criterion: the built binary run with `--pair 11` exits 2, its stderr names
// `1..=10` and does not contain `ErrorInner`. Guard ("a refusal is never
// rendered as `Debug`"): near-miss is a `main` returning the `clap::Error`,
// which still refuses but exits 1 with the dump.
#[test]
fn test_binary_pair_out_of_range_exits_two_with_clap_s_message() {
    let run = run_binary("pair-11", &["--pair", "11"]).unwrap();
    assert!(
        !run.timed_out,
        "--pair 11 must be refused before the {DEADLINE:?} deadline; stderr: {}",
        run.stderr
    );
    assert_eq!(run.code, Some(2), "stderr: {}", run.stderr);
    assert!(
        run.stderr.contains("1..=10"),
        "stderr must name the range 1..=10, got {:?}",
        run.stderr
    );
    assert!(
        !run.stderr.contains("ErrorInner"),
        "stderr must be clap's message, not its Debug dump: {:?}",
        run.stderr
    );
}

// Criterion (#194): the binary started on a pre-created `state/blue2th` at
// `755` exits 1 before the deadline, logs the refusal at `ERROR` on stdout
// (tracing's `fmt()` writes there) with `chmod 700 '<path>'`, and leaves no
// file in the state directory. Guard ("before any store"): near-miss is the
// check placed after `AuthStore::with_store`, which still refuses but has
// already minted and written `auth.json`.
#[test]
fn test_binary_refuses_a_755_state_dir_before_any_store_writes() {
    use std::os::unix::fs::PermissionsExt as _;
    let mut fix = String::new();
    let run = run_binary_prepared("state-dir-755", &[], |dir| {
        let state_dir = dir.join("state").join("blue2th");
        fix = format!("chmod 700 '{}'", state_dir.display());
        std::fs::create_dir_all(&state_dir)?;
        std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o755))
    })
    .unwrap();
    assert!(
        !run.timed_out,
        "a 755 state directory must be refused before the {DEADLINE:?} deadline, not start the server; stdout: {}",
        run.stdout
    );
    assert_eq!(
        run.code,
        Some(1),
        "stdout: {}\nstderr: {}",
        run.stdout,
        run.stderr
    );
    assert!(
        run.stdout.contains("ERROR"),
        "the refusal is logged at ERROR, got {:?}",
        run.stdout
    );
    assert!(
        run.stdout.contains(&fix),
        "stdout must give {fix:?}, got {:?}",
        run.stdout
    );
    assert!(
        !run.state_files.contains("auth.json"),
        "no token may be written before the check: {:?}",
        run.state_files
    );
    assert!(
        run.state_files.is_empty(),
        "no store may write before the check: {:?}",
        run.state_files
    );
}

// Requested at manual verification (#194): `main` returns the refusal, and
// prints it on stderr with `Debug`. That line reads as the message itself —
// the fixing command included — and not as a `StateDirRefusal { … }` dump.
#[test]
fn test_binary_refusal_on_stderr_is_the_message_not_a_debug_dump() {
    use std::os::unix::fs::PermissionsExt as _;
    let mut fix = String::new();
    let run = run_binary_prepared("state-dir-755-stderr", &[], |dir| {
        let state_dir = dir.join("state").join("blue2th");
        fix = format!("chmod 700 '{}'", state_dir.display());
        std::fs::create_dir_all(&state_dir)?;
        std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o755))
    })
    .unwrap();
    assert_eq!(run.code, Some(1), "stdout: {}", run.stdout);
    assert!(
        run.stderr.contains(&fix),
        "stderr must give {fix:?}, got {:?}",
        run.stderr
    );
    assert!(
        !run.stderr.contains("StateDirRefusal {"),
        "no Debug dump on stderr: {:?}",
        run.stderr
    );
}

/// Lay out `state/blue2th` at `700` under `dir`, holding `tls-key.pem` with
/// `body`, and return the key's path.
fn state_dir_with_key(dir: &Path, body: &str) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;
    let state_dir = dir.join("state").join("blue2th");
    std::fs::create_dir_all(&state_dir)?;
    std::fs::set_permissions(dir.join("state"), std::fs::Permissions::from_mode(0o700))?;
    std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o700))?;
    let key = state_dir.join("tls-key.pem");
    std::fs::write(&key, body)?;
    Ok(key)
}

// Criterion (#195, key), guard: a malformed `tls-key.pem` refuses the start
// before `AuthStore::with_store`: exit 1 before the deadline, the message
// names the path and says to delete it, no `auth.json` or certificate is
// written, and the key is byte for byte what it was. Near-miss: the key check
// placed after `AuthStore::with_store`, which still refuses but has already
// minted and written `auth.json`.
#[test]
fn test_binary_refuses_a_malformed_tls_key_before_any_store_writes() {
    let mut key_path = String::new();
    let run = run_binary_prepared("tls-key-malformed", &[], |dir| {
        key_path = state_dir_with_key(dir, "not a key")?.display().to_string();
        Ok(())
    })
    .unwrap();
    assert!(
        !run.timed_out,
        "a malformed key must be refused before the {DEADLINE:?} deadline, not start the server; stdout: {}",
        run.stdout
    );
    assert_eq!(
        run.code,
        Some(1),
        "stdout: {}\nstderr: {}",
        run.stdout,
        run.stderr
    );
    let output = format!("{}{}", run.stdout, run.stderr);
    assert!(
        output.contains(&key_path) && output.contains("delete"),
        "the refusal names {key_path} and says to delete it, got {output:?}"
    );
    assert_eq!(
        run.state_files,
        BTreeSet::from(["tls-key.pem".to_string()]),
        "nothing but the untouched key may be in the state directory"
    );
    assert_eq!(
        run.tls_key.as_deref(),
        Some(b"not a key".as_slice()),
        "the key is byte for byte unchanged"
    );
}

// Criterion (#195, plain mode): `BLUE2TH_PLAIN_HTTP=1` in a debug build with a
// non-loopback `--bind` refuses to start, before anything is written: exit 1
// before the deadline, the message names the address and the loopback rule,
// and the state directory holds no file. `192.0.2.1` (TEST-NET-1) is never a
// local address, so a server that starts anyway cannot reach a real LAN.
#[test]
fn test_binary_in_plain_mode_refuses_a_non_loopback_bind_before_any_store_writes() {
    let run = run_binary_with_env(
        "plain-non-loopback",
        &["--bind", "192.0.2.1:4000"],
        &[("BLUE2TH_PLAIN_HTTP", "1")],
        |_| Ok(()),
    )
    .unwrap();
    assert!(
        !run.timed_out,
        "the refusal must come before the {DEADLINE:?} deadline; stdout: {}",
        run.stdout
    );
    assert_eq!(
        run.code,
        Some(1),
        "stdout: {}\nstderr: {}",
        run.stdout,
        run.stderr
    );
    let output = format!("{}{}", run.stdout, run.stderr);
    assert!(
        output.contains("192.0.2.1:4000") && output.contains("loopback"),
        "the refusal names the address and the loopback rule, got {output:?}"
    );
    assert!(
        run.state_files.is_empty(),
        "no store may write before the refusal: {:?}",
        run.state_files
    );
}

// Criterion (#195, key; plain mode): the two refusals #195 adds reach stderr
// as their message, as the state directory's does (#194), and not as a
// `KeyRefusal { … }` or `StartRefusal("…")` dump. Stderr alone: the log line
// on stdout carries the message whatever `Debug` prints.
#[test]
fn test_binary_tls_refusals_on_stderr_are_the_message_not_a_debug_dump() {
    let mut key_path = String::new();
    let key = run_binary_prepared("tls-key-malformed-stderr", &[], |dir| {
        key_path = state_dir_with_key(dir, "not a key")?.display().to_string();
        Ok(())
    })
    .unwrap();
    assert_eq!(key.code, Some(1), "stdout: {}", key.stdout);
    assert!(
        key.stderr.contains(&key_path) && key.stderr.contains("delete"),
        "stderr must name {key_path} and say to delete it, got {:?}",
        key.stderr
    );
    assert!(
        !key.stderr.contains("KeyRefusal"),
        "no Debug dump on stderr: {:?}",
        key.stderr
    );

    let bind = run_binary_with_env(
        "plain-non-loopback-stderr",
        &["--bind", "192.0.2.1:4000"],
        &[("BLUE2TH_PLAIN_HTTP", "1")],
        |_| Ok(()),
    )
    .unwrap();
    assert_eq!(bind.code, Some(1), "stdout: {}", bind.stdout);
    assert!(
        bind.stderr.contains("192.0.2.1:4000 is not one"),
        "stderr must carry the refusal itself, got {:?}",
        bind.stderr
    );
    assert!(
        !bind.stderr.contains("StartRefusal"),
        "no Debug dump on stderr: {:?}",
        bind.stderr
    );
}

/// The names of every crate `manifest` depends on, in any dependency table
/// (`dependencies`, `dev-dependencies`, `build-dependencies`, and their
/// `[target.'cfg(…)'.…]` forms), under the package's real name when an entry
/// renames it (`cli = { package = "clap" }`).
fn dependency_names(manifest: &toml::Table) -> BTreeSet<String> {
    const KINDS: [&str; 3] = ["dependencies", "dev-dependencies", "build-dependencies"];
    let targets = manifest
        .get("target")
        .and_then(|t| t.as_table())
        .into_iter()
        .flat_map(|t| t.values())
        .filter_map(|v| v.as_table());
    std::iter::once(manifest)
        .chain(targets)
        .flat_map(|scope| KINDS.iter().filter_map(|kind| scope.get(*kind)))
        .filter_map(|table| table.as_table())
        .flat_map(|table| table.iter())
        .map(|(key, value)| {
            value
                .get("package")
                .and_then(|p| p.as_str())
                .unwrap_or(key)
                .to_owned()
        })
        .collect()
}

/// A workspace member's manifest, parsed. An unreadable or unparsable one is
/// an error rather than an empty table: an empty table depends on nothing, so
/// "`xtask` does not depend on clap" would pass without reading `xtask`.
fn member_manifest(member: &str) -> Result<toml::Table, String> {
    let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(member)
        .join("Cargo.toml");
    std::fs::read_to_string(&path)
        .map_err(|e| e.to_string())
        .and_then(|raw| raw.parse::<toml::Table>().map_err(|e| e.to_string()))
        .map_err(|e| format!("{}: {e}", path.display()))
}

// Criterion: `dependency_names` sees a dependency hidden in a target table or
// behind a rename — the two places a check of `[dependencies]` alone misses.
#[test]
fn test_dependency_names_reads_target_tables_and_renamed_packages() {
    let manifest = r#"
        [dependencies]
        serde = "1"
        [target.'cfg(target_os = "android")'.dependencies]
        jni = "0.21"
        [dev-dependencies]
        cli = { package = "clap", version = "4" }
    "#
    .parse::<toml::Table>()
    .unwrap_or_default();
    let names = dependency_names(&manifest);
    let expected: BTreeSet<String> = ["serde", "jni", "clap"].map(str::to_owned).into();
    assert_eq!(names, expected);
}

// Criterion: `clap` is a dependency of `blue2th-server` only;
// `blue2th-frontend`, `blue2th-proto` and `xtask` do not get it.
#[test]
fn test_clap_is_a_dependency_of_the_server_only() {
    assert!(
        dependency_names(&member_manifest("blue2th-server").unwrap()).contains("clap"),
        "blue2th-server must depend on clap"
    );
    for member in ["blue2th-frontend", "blue2th-proto", "xtask"] {
        assert!(
            !dependency_names(&member_manifest(member).unwrap()).contains("clap"),
            "{member} must not depend on clap"
        );
    }
}
