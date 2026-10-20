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
}

/// Run the built binary with `args`, in an environment isolated under a
/// directory unique to `test_name`, removed before returning — on an error
/// too.
fn run_binary(test_name: &str, args: &[&str]) -> std::io::Result<Run> {
    let dir = std::env::temp_dir().join(format!("blue2th-cli-{test_name}-{}", std::process::id()));
    // A leftover from an interrupted run would be reused otherwise.
    let _ = std::fs::remove_dir_all(&dir);
    let run = std::fs::create_dir_all(&dir).and_then(|()| run_in(&dir, args));
    let _ = std::fs::remove_dir_all(&dir);
    run
}

/// Run the built binary with `args`, with `dir` as its home, state and cache,
/// killing it at the deadline.
///
/// The output goes to files rather than pipes: a grandchild (librespot) that
/// inherits a pipe would keep a reader blocked after the child is gone.
fn run_in(dir: &Path, args: &[&str]) -> std::io::Result<Run> {
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

/// A workspace member's manifest, parsed; empty when unreadable, so the
/// assertions fail rather than the helper.
fn member_manifest(member: &str) -> toml::Table {
    let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(member)
        .join("Cargo.toml");
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(!raw.is_empty(), "{} must be readable", path.display());
    raw.parse::<toml::Table>().unwrap_or_default()
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
        dependency_names(&member_manifest("blue2th-server")).contains("clap"),
        "blue2th-server must depend on clap"
    );
    for member in ["blue2th-frontend", "blue2th-proto", "xtask"] {
        assert!(
            !dependency_names(&member_manifest(member)).contains("clap"),
            "{member} must not depend on clap"
        );
    }
}
