// SPDX-License-Identifier: MIT OR Apache-2.0

//! The backend's own name (phase 6.2).
//!
//! The clients — the phone and the browser — read it over `GET /config` and
//! push a change over `POST /config`, the last action winning (#160); the server
//! persists it so a restart keeps advertising the same Spotify Connect device —
//! and so the Web API device lookup keeps matching the running `librespot` even
//! before a client talks to it again.
//!
//! The name comes from a client, so the server re-validates it with the
//! *shared* `blue2th_proto::validate_backend_name` rule rather than trusting
//! the client. Following the `state_store` pattern, `new()` is disk-free so tests
//! never read or write the real `~/.local/state/blue2th/`.

use blue2th_proto::{NameError, ServerConfig, DEFAULT_BACKEND_NAME};

/// File holding the configured name under the app-scoped state directory.
const NAME_STORE_FILE: &str = "name.json";

/// Path of the name store, or `None` when no state home can be resolved (the
/// name then simply stays in memory).
pub fn name_store_path() -> Option<std::path::PathBuf> {
    crate::state_store::state_store_path(NAME_STORE_FILE)
}

/// The name this backend answers to, optionally persisted.
pub struct ServerName {
    /// The current name; the default until the app configures another one.
    name: String,
    /// Whether a remembered speaker coming back mid-playback is re-selected
    /// straight away (phase 6.3). Persisted next to the name; defaults to on.
    restore_during_playback: bool,
    /// Whether the backend dials a remembered-but-disconnected speaker back by
    /// itself (phase 6.5). Persisted next to the name; defaults to on.
    auto_reconnect: bool,
    /// Whether the Spotify Connect level is pinned to 100 (#58). Persisted next
    /// to the name; defaults to off, since pinning is an opt-in.
    spotify_volume_lock: bool,
    /// Where the name is persisted, or `None` to stay in memory only.
    store: Option<std::path::PathBuf>,
}

/// Read the persisted config, or `None` when there is nothing usable to read.
/// The flag rides along, defaulted by the DTO — so a phase 6.2 store, which only
/// carries a name, comes back with restoration on rather than silently off.
fn load_config(path: Option<&std::path::Path>) -> Option<ServerConfig> {
    let raw = std::fs::read_to_string(path?).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Persist the configured name. Failures are reported to the caller, which logs
/// them: losing persistence must never turn a rename into an error.
fn save_config(
    path: &std::path::Path,
    name: &str,
    restore_during_playback: bool,
    auto_reconnect: bool,
    spotify_volume_lock: bool,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string(&ServerConfig {
        // Owned copy: `ServerConfig` is a plain DTO built for serialization.
        name: name.to_string(),
        restore_during_playback,
        auto_reconnect,
        spotify_volume_lock,
    })
    .map_err(std::io::Error::other)?;
    std::fs::write(path, body)
}

impl ServerName {
    /// The default name, with no store: performs no I/O (used by tests and by
    /// the store-free router constructors).
    pub fn new() -> Self {
        Self {
            name: DEFAULT_BACKEND_NAME.to_string(),
            restore_during_playback: true,
            auto_reconnect: true,
            spotify_volume_lock: false,
            store: None,
        }
    }

    /// A name backed by a store, reloaded on construction so a restart keeps the
    /// configured name. A missing or malformed store yields the default.
    pub fn with_store(store: Option<std::path::PathBuf>) -> Self {
        let stored = load_config(store.as_deref());
        Self {
            name: stored
                .as_ref()
                .and_then(|c| blue2th_proto::validate_backend_name(&c.name).ok())
                .unwrap_or_else(|| DEFAULT_BACKEND_NAME.to_string()),
            // Absent from the store (phase 6.2 file) or unreadable: on, the default.
            restore_during_playback: stored
                .as_ref()
                .map(|c| c.restore_during_playback)
                .unwrap_or(true),
            // Absent from the store (a pre-6.5 file) or unreadable: on, the default.
            auto_reconnect: stored.as_ref().map(|c| c.auto_reconnect).unwrap_or(true),
            // Absent from the store (a pre-#58 file) or unreadable: off, the
            // default — a guard is never turned on by omission.
            spotify_volume_lock: stored
                .as_ref()
                .map(|c| c.spotify_volume_lock)
                .unwrap_or(false),
            store,
        }
    }

    /// The current name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether a returning speaker may be restored while playback runs.
    pub fn restore_during_playback(&self) -> bool {
        self.restore_during_playback
    }

    /// Store and persist the restore-during-playback setting.
    pub fn set_restore_during_playback(&mut self, enabled: bool) {
        self.restore_during_playback = enabled;
        self.persist();
    }

    /// Whether the backend may dial a remembered speaker back by itself.
    pub fn auto_reconnect(&self) -> bool {
        self.auto_reconnect
    }

    /// Store and persist the auto-reconnect setting.
    pub fn set_auto_reconnect(&mut self, enabled: bool) {
        self.auto_reconnect = enabled;
        self.persist();
    }

    /// Whether the Spotify Connect level is pinned to 100 (#58).
    pub fn spotify_volume_lock(&self) -> bool {
        self.spotify_volume_lock
    }

    /// Store and persist the Spotify volume lock.
    pub fn set_spotify_volume_lock(&mut self, enabled: bool) {
        self.spotify_volume_lock = enabled;
        self.persist();
    }

    /// Write name and flag together: they share one file, so a partial write
    /// would drop whichever half it left out.
    fn persist(&self) {
        let Some(path) = self.store.as_deref() else {
            return;
        };
        if let Err(e) = save_config(
            path,
            &self.name,
            self.restore_during_playback,
            self.auto_reconnect,
            self.spotify_volume_lock,
        ) {
            tracing::warn!("could not persist the backend config: {e}");
        }
    }

    /// Validate (shared proto rule), trim, store and persist a new name,
    /// returning what was actually stored.
    pub fn set_name(&mut self, raw: &str) -> Result<String, NameError> {
        let name = blue2th_proto::validate_backend_name(raw)?;
        // Owned copy: the validated value is both stored and handed back.
        self.name = name.clone();
        self.persist();
        Ok(name)
    }
}

impl Default for ServerName {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
