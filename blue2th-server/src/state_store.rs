// SPDX-License-Identifier: MIT OR Apache-2.0

//! Where the backend keeps its small pieces of persistent state.
//!
//! The API's bearer token (`auth.rs`), the backend's name and settings
//! (`config.rs`), its identity (`identity.rs`), the Spotify refresh token
//! (`spotify_auth.rs`) and the remembered speaker offsets (`targets.rs`) all
//! live under the same app-scoped state directory, so the XDG resolution
//! rules, the directory's creation and its start-up check live here once
//! rather than in each store.
//!
//! The XDG resolution is deliberately untested here: asserting the fallback
//! order means mutating `XDG_STATE_HOME`/`HOME`, which is process-wide and
//! would race with any other env-mutating test in the same binary. `targets::
//! tests::test_offsets_store_path_is_app_scoped_and_honours_xdg_state_home` is
//! the single place that does it. The directory's creation and its start-up
//! check (#194) take explicit paths, and are tested in `state_store/tests.rs`.

use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};

/// Directory scoping every blue2th state file, under the user's state home.
const APP_DIR: &str = "blue2th";

/// The app-scoped state directory: `$XDG_STATE_HOME/blue2th`, falling back to
/// `~/.local/state/blue2th`. `None` when neither variable is set (or both are
/// blank), in which case the backend simply stays in memory only.
pub(crate) fn state_dir() -> Option<PathBuf> {
    let base = std::env::var("XDG_STATE_HOME")
        .ok()
        .filter(|p| !p.trim().is_empty())
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .filter(|h| !h.trim().is_empty())
                .map(|h| format!("{h}/.local/state"))
        })?;
    Some(PathBuf::from(base).join(APP_DIR))
}

/// Path of an app-scoped state file: `<state_dir>/<file>`. `None` when there
/// is no state directory, in which case the caller simply stays in memory only.
pub(crate) fn state_store_path(file: &str) -> Option<PathBuf> {
    state_dir().map(|dir| dir.join(file))
}

/// Create the state directory `dir`, and any missing parent, `0700` (#194):
/// the one place a store creates it. An existing directory keeps its mode.
pub(crate) fn ensure_state_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

/// What the start-up check found at the state directory's path.
#[derive(Debug)]
pub(crate) enum Found {
    /// Nothing there: a first start, which the first store write creates.
    Missing,
    /// Something that is not a directory.
    NotADirectory,
    /// A directory. `mode` is `MetadataExt::mode` as is, file-type bits
    /// included; `uid` is its owner.
    Directory { mode: u32, uid: u32 },
    /// `metadata` failed with anything but `NotFound`.
    Uninspectable(std::io::Error),
}

/// Why the backend refuses to start on its state directory. Its `Display` is
/// the whole message the user reads, the fixing command(s) included.
pub(crate) struct StateDirRefusal {
    message: String,
}

/// The message, as `Display` writes it: `main` returns the refusal through
/// `Result<(), Box<dyn Error>>`, whose error the runtime prints with `Debug` —
/// a derived one would put a `StateDirRefusal { message: … }` dump on stderr.
impl std::fmt::Debug for StateDirRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::fmt::Display for StateDirRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for StateDirRefusal {}

/// The verdict on what was `found` at `dir`, for a backend running as `euid`.
/// Pure: the file system is read by `check_state_dir`.
pub(crate) fn judge_state_dir(dir: &Path, found: Found, euid: u32) -> Result<(), StateDirRefusal> {
    let path = shell_quoted(dir);
    let message = match found {
        Found::Missing => return Ok(()),
        Found::NotADirectory => {
            format!("the state directory {path} is not a directory; refusing to start")
        },
        Found::Uninspectable(error) => {
            format!("cannot inspect the state directory {path}: {error}; refusing to start")
        },
        Found::Directory { mode, uid } => {
            // The type bits dropped, the setuid/setgid/sticky ones kept: the
            // mode as `stat -c %a` prints it.
            let permissions = mode & 0o7777;
            let too_open = permissions & 0o077 != 0;
            let foreign = uid != euid;
            if !too_open && !foreign {
                return Ok(());
            }
            let mut fixes = Vec::new();
            if foreign {
                fixes.push(format!("sudo chown {euid} {path}"));
            }
            if too_open {
                fixes.push(format!("chmod 700 {path}"));
            }
            format!(
                "the state directory {path} (mode {permissions:o}, owner uid {uid}) must grant \
                 nothing to group or others and be owned by uid {euid}; refusing to start. Fix it with: {}",
                fixes.join(" && ")
            )
        },
    };
    Err(StateDirRefusal { message })
}

/// `dir` single-quoted for a shell, a `'` inside it written `'\''`.
fn shell_quoted(dir: &Path) -> String {
    format!("'{}'", dir.to_string_lossy().replace('\'', r"'\''"))
}

/// Inspect `dir` (following a symbolic link) and judge it against the
/// effective uid.
pub(crate) fn check_state_dir(dir: &Path) -> Result<(), StateDirRefusal> {
    let found = match std::fs::metadata(dir) {
        Ok(metadata) if metadata.is_dir() => Found::Directory {
            mode: std::os::unix::fs::MetadataExt::mode(&metadata),
            uid: std::os::unix::fs::MetadataExt::uid(&metadata),
        },
        Ok(_) => Found::NotADirectory,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Found::Missing,
        Err(error) => Found::Uninspectable(error),
    };
    judge_state_dir(dir, found, nix::unistd::geteuid().as_raw())
}

/// What `run` does about the state directory at start.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StartupCheck<'a> {
    /// Check this directory before any store touches it.
    Check(&'a Path),
    /// `--skip-state-permissions-check` is set: no check, and this warning.
    Skip { warning: String },
    /// No state directory at all: the backend stays in memory only.
    Nothing,
}

/// The start-up decision over the flag and the state directory. Pure.
pub(crate) fn startup_check(skip: bool, state_dir: Option<&Path>) -> StartupCheck<'_> {
    match (skip, state_dir) {
        (true, _) => StartupCheck::Skip {
            warning: "--skip-state-permissions-check is set: the state directory's mode and \
                      owner are not checked, and its tokens may be readable by other users"
                .to_string(),
        },
        (false, Some(dir)) => StartupCheck::Check(dir),
        (false, None) => StartupCheck::Nothing,
    }
}

#[cfg(test)]
mod tests;
