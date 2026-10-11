// SPDX-License-Identifier: MIT OR Apache-2.0
//! `cargo check-conventions [-D warnings]` (#180). The rules live in the
//! library; this is the shell around them: arguments, git, files, output.
//!
//! Exit codes: 0 nothing at `deny`, 1 a finding at `deny`, 2 the check could
//! not run. A check that could not run must never read as a pass.

use std::io;
use std::path::Path;
use std::process::{Command, ExitCode};

use xtask::{check, parse_args, parse_config, SourceFile};

const USAGE: &str = "usage: cargo check-conventions [-D warnings]";

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(message) => {
            eprintln!("check-conventions: {message}");
            ExitCode::from(2)
        },
    }
}

fn run() -> Result<u8, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let escalation = parse_args(&args).map_err(|e| format!("{}\n{USAGE}", e.0))?;

    // The workspace root, wherever the alias is run from.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or("the xtask crate has no parent directory")?;

    let manifest = std::fs::read_to_string(root.join("Cargo.toml"))
        .map_err(|e| format!("cannot read Cargo.toml: {e}"))?;
    let config = parse_config(&manifest).map_err(|e| e.to_string())?;
    let files = tracked_sources(root)?;
    let report =
        check(&files, &config, escalation).map_err(|e| format!("{}: {}", e.path, e.message))?;

    for entry in &report.entries {
        eprintln!("{entry}");
    }
    eprintln!(
        "check-conventions: {} file(s), {} finding(s)",
        files.len(),
        report.entries.len()
    );
    Ok(report.exit_code())
}

/// Every tracked `.rs`, as CI sees it: the index, not the directory walk, so
/// `target/` and an untracked scratch file stay out.
fn tracked_sources(root: &Path) -> Result<Vec<SourceFile>, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z", "--", "*.rs"])
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git ls-files failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let listing =
        String::from_utf8(output.stdout).map_err(|_| "git ls-files printed a non-UTF-8 path")?;

    let mut files = Vec::new();
    // `-z` ends the listing with a NUL, which leaves one empty piece.
    for path in listing.split('\0').filter(|path| !path.is_empty()) {
        match std::fs::read_to_string(root.join(path)) {
            Ok(content) => files.push(SourceFile {
                path: path.to_string(),
                content,
            }),
            // Still in the index, gone from the working tree: on its way out.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {},
            Err(e) => return Err(format!("{path}: {e}")),
        }
    }
    Ok(files)
}
