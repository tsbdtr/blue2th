//! Where the backend keeps its small pieces of persistent state.
//!
//! The Spotify refresh token (`spotify_auth.rs`) and the remembered speaker
//! offsets (`targets.rs`) both live under the same app-scoped state directory,
//! so the XDG resolution rules live here once rather than in each store.
//!
//! Deliberately untested here: asserting the fallback order means mutating
//! `XDG_STATE_HOME`/`HOME`, which is process-wide and would race with any other
//! env-mutating test in the same binary. `targets::tests::
//! test_offsets_store_path_is_app_scoped_and_honours_xdg_state_home` is the
//! single place that does it.

use std::path::PathBuf;

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
