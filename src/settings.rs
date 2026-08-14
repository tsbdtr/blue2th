// Copyright 2026 Blue2th
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! App settings (phase 6.2): the list of known backends and which one is active.
//!
//! The backend address used to be baked into the APK (`option_env!`), which made
//! the app unusable by anyone who did not build it. It is now a runtime setting,
//! persisted on the phone through `SharedPreferences` (an Android-only JNI seam,
//! mirroring `src/deep_link.rs`) and cached in memory for the HTTP client.
//!
//! Everything in this module except the storage seam is **pure** and unit-tested
//! on any platform: the list operations, the URL normalisation, and the loading
//! of a possibly-missing or corrupt blob.
//!
//! The *name* rule itself lives in `blue2th-proto`, shared with the server, which
//! re-validates rather than trusting the client.

use blue2th_proto::NameError;
use serde::{Deserialize, Serialize};

/// What the status encart shows while no backend is configured.
pub const NO_BACKEND_LABEL: &str = "-";

/// A backend the user configured: the name the app is the source of truth for,
/// and the address every call goes to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendEntry {
    /// Display name, pushed to the backend as its Spotify Connect device name.
    pub name: String,
    /// Base URL, e.g. `http://192.168.1.107:4000` (no trailing slash).
    pub url: String,
}

/// Why a settings change was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsError {
    /// The name broke the shared `blue2th-proto` rule.
    Name(NameError),
    /// Another backend already carries that name — two identical labels in the
    /// status encart would be indistinguishable.
    DuplicateName,
    /// The address has no scheme, holds spaces, or is blank.
    MalformedUrl,
    /// The referenced backend does not exist (stale index).
    UnknownBackend,
}

impl std::fmt::Display for SettingsError {
    fn fmt(&self, _f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        todo!("phase 6.2: message for each settings rejection reason")
    }
}

/// The whole persisted app configuration: the known backends and which one is
/// active. Exactly one backend is active at a time, or none at all.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppSettings {
    /// Every backend the user configured, in the order they were added.
    pub backends: Vec<BackendEntry>,
    /// Index into `backends` of the active one, or `None` when nothing is active.
    pub active: Option<usize>,
}

/// Normalise a backend address: require a scheme, refuse blanks and embedded
/// whitespace, and trim a trailing slash so URL building stays predictable.
pub fn normalise_url(_raw: &str) -> Result<String, SettingsError> {
    todo!("phase 6.2: normalise and validate a backend URL")
}

impl AppSettings {
    /// Add a backend. The name goes through the shared proto validator and must
    /// not collide with an existing one; the URL is normalised.
    pub fn add(&mut self, _name: &str, _url: &str) -> Result<(), SettingsError> {
        todo!("phase 6.2: add a validated backend entry")
    }

    /// Remove the backend at `index`. Removing the active one leaves no active
    /// backend at all (the app then knows no address).
    pub fn remove(&mut self, _index: usize) -> Result<(), SettingsError> {
        todo!("phase 6.2: remove a backend entry, keeping the active index honest")
    }

    /// Make the backend at `index` the active one (exactly one at a time).
    pub fn activate(&mut self, _index: usize) -> Result<(), SettingsError> {
        todo!("phase 6.2: switch the active backend")
    }

    /// The active backend, or `None` when nothing is configured/active.
    pub fn active_backend(&self) -> Option<&BackendEntry> {
        todo!("phase 6.2: resolve the active backend entry")
    }

    /// The active backend's URL, or `None` — there is no fallback address of any
    /// kind, so an unconfigured app sends nothing anywhere.
    pub fn active_url(&self) -> Option<String> {
        todo!("phase 6.2: resolve the active backend URL")
    }

    /// What the status encart shows: the active backend's name, or
    /// [`NO_BACKEND_LABEL`] when none is active.
    pub fn active_label(&self) -> String {
        todo!("phase 6.2: label the status encart from the active backend")
    }
}

/// Parse the persisted blob. A missing, empty, malformed or non-JSON blob yields
/// empty settings: a broken preferences entry must never prevent a start.
pub fn load(_raw: Option<&str>) -> AppSettings {
    todo!("phase 6.2: load settings, tolerating a corrupt blob")
}

/// Serialize the settings for the storage seam.
pub fn save_blob(_settings: &AppSettings) -> String {
    todo!("phase 6.2: serialize settings for SharedPreferences")
}

/// The settings currently in memory, backing the runtime backend lookup.
pub fn current() -> AppSettings {
    todo!("phase 6.2: read the in-memory settings cache")
}

/// Replace the in-memory settings and persist them (Android storage seam).
pub fn set_current(_settings: AppSettings) {
    todo!("phase 6.2: cache and persist the settings")
}
