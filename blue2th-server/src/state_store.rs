// SPDX-License-Identifier: MIT OR Apache-2.0

//! Where the backend keeps its small pieces of persistent state.
//!
//! The Spotify refresh token (`spotify_auth.rs`) and the remembered speaker
//! offsets (`targets.rs`) both live under the same app-scoped state directory,
//! so the XDG resolution rules live here once rather than in each store.
//!
//! The XDG resolution is deliberately untested here: asserting the fallback
//! order means mutating `XDG_STATE_HOME`/`HOME`, which is process-wide and
//! would race with any other env-mutating test in the same binary. `targets::
//! tests::test_offsets_store_path_is_app_scoped_and_honours_xdg_state_home` is
//! the single place that does it. The directory's creation and its start-up
//! check (#194) take explicit paths, and are tested in `state_store/tests.rs`.

use std::path::{Path, PathBuf};

/// Directory scoping every blue2th state file, under the user's state home.
const APP_DIR: &str = "blue2th";

/// Path of an app-scoped state file: `$XDG_STATE_HOME/blue2th/<file>`, falling
/// back to `~/.local/state/blue2th/<file>`. `None` when neither variable is set
/// (or both are blank), in which case the caller simply stays in memory only.
pub(crate) fn state_store_path(file: &str) -> Option<PathBuf> {
    let base = std::env::var("XDG_STATE_HOME")
        .ok()
        .filter(|p| !p.trim().is_empty())
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .filter(|h| !h.trim().is_empty())
                .map(|h| format!("{h}/.local/state"))
        })?;
    Some(PathBuf::from(base).join(APP_DIR).join(file))
}

/// Create the state directory `dir`, and any missing parent, `0700` (#194):
/// the one place a store creates it. An existing directory keeps its mode.
pub(crate) fn ensure_state_dir(_dir: &Path) -> std::io::Result<()> {
    // RED stub (#194): the green phase replaces it.
    Err(std::io::Error::other("ensure_state_dir is not implemented"))
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
#[derive(Debug)]
pub(crate) struct StateDirRefusal {
    message: String,
}

impl std::fmt::Display for StateDirRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for StateDirRefusal {}

/// The verdict on what was `found` at `dir`, for a backend running as `euid`.
/// Pure: the file system is read by `check_state_dir`.
pub(crate) fn judge_state_dir(
    _dir: &Path,
    _found: Found,
    _euid: u32,
) -> Result<(), StateDirRefusal> {
    // RED stub (#194): the green phase replaces it.
    Err(StateDirRefusal {
        message: String::new(),
    })
}

/// Inspect `dir` (following a symbolic link) and judge it against the
/// effective uid.
pub(crate) fn check_state_dir(_dir: &Path) -> Result<(), StateDirRefusal> {
    // RED stub (#194): the green phase replaces it.
    Err(StateDirRefusal {
        message: String::new(),
    })
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
pub(crate) fn startup_check(_skip: bool, _state_dir: Option<&Path>) -> StartupCheck<'_> {
    // RED stub (#194): the green phase replaces it.
    StartupCheck::Skip {
        warning: String::new(),
    }
}

#[cfg(test)]
mod tests;
