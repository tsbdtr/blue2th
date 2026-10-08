// SPDX-License-Identifier: MIT OR Apache-2.0

//! The backend's stable identity (phase 6.6).
//!
//! A backend used to be identified by its URL alone, so a DHCP lease change made
//! the app create a *second* entry for the same machine. It now mints a stable id
//! once, publishes it over mDNS, and the app matches on that instead.
//!
//! The id lives in its own store, **separate from the token** (`auth.rs`): wiping
//! `auth.json` must re-mint the token without changing who the machine is, or
//! re-pairing would still duplicate the entry. Following the `state_store`
//! pattern, [`BackendIdentity::new`] is disk-free so tests never read or write
//! the real `~/.local/state/blue2th/`.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use rand::Rng as _;

/// Bytes of entropy behind a minted backend id. It only has to be unique on a
/// LAN, but it is cheap to make a collision impossible outright.
const ID_ENTROPY_BYTES: usize = 16;

/// File holding the persisted backend id under the app-scoped state directory.
const IDENTITY_STORE_FILE: &str = "identity.json";

/// Path of the identity store, or `None` when no state home can be resolved (the
/// id then stays in memory, which re-mints it on every restart).
pub fn identity_store_path() -> Option<PathBuf> {
    crate::state_store::state_store_path(IDENTITY_STORE_FILE)
}

/// The stable id this backend advertises, optionally persisted.
///
/// The store path is not kept: the id never changes once minted, so there is
/// nothing left to write after construction.
pub struct BackendIdentity {
    /// The current id: reloaded from the store, or freshly minted.
    id: String,
    /// Whether the id was minted rather than reloaded.
    minted: bool,
}

/// On-disk shape of the identity store.
#[derive(serde::Serialize, serde::Deserialize)]
struct StoredId {
    id: String,
}

/// Mint a URL-safe stable id, different on every call. It rides in a TXT record
/// and in the app's settings blob, so it stays in the URL-safe alphabet.
pub fn generate_id() -> String {
    let mut raw = [0u8; ID_ENTROPY_BYTES];
    rand::thread_rng().fill(&mut raw[..]);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw)
}

/// Read the persisted id. `None` for a missing, unreadable, malformed or blank
/// store — all four mean "no usable id", and the caller mints a new one.
fn load_id(path: Option<&Path>) -> Option<String> {
    let raw = std::fs::read_to_string(path?).ok()?;
    let stored: StoredId = serde_json::from_str(&raw).ok()?;
    (!stored.id.trim().is_empty()).then_some(stored.id)
}

/// Persist the id. A failure is logged, never propagated: the backend still
/// runs, it simply looks like a new machine after a restart.
fn persist_id(path: &Path, id: &str) {
    if let Err(e) = write_id(path, id) {
        tracing::warn!("could not persist the backend id: {e}");
    }
}

fn write_id(path: &Path, id: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string(&StoredId {
        // Owned copy: the on-disk shape is serialized from its own value.
        id: id.to_string(),
    })
    .map_err(std::io::Error::other)?;
    std::fs::write(path, body)
}

impl BackendIdentity {
    /// A fresh id with no store: performs no I/O.
    pub fn new() -> Self {
        Self {
            id: generate_id(),
            minted: true,
        }
    }

    /// An id backed by a store, reloaded on construction so a restart keeps the
    /// same identity. A missing, unreadable or malformed store mints (and
    /// persists) a new one.
    pub fn with_store(store: Option<PathBuf>) -> Self {
        let reloaded = load_id(store.as_deref());
        let minted = reloaded.is_none();
        let id = reloaded.unwrap_or_else(|| {
            let fresh = generate_id();
            if let Some(path) = store.as_deref() {
                persist_id(path, &fresh);
            }
            fresh
        });
        Self { id, minted }
    }

    /// The current stable id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Whether the id was minted rather than reloaded from the store. A minted
    /// id makes this machine look brand new to every app that knew it, so it is
    /// worth a log line — unlike a reloaded one.
    pub fn minted_a_new_id(&self) -> bool {
        self.minted
    }
}

impl Default for BackendIdentity {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
