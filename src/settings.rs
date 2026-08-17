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

use blue2th_proto::{NameError, PairLink};
use serde::{Deserialize, Serialize};

/// What the status encart shows while no backend is configured.
pub const NO_BACKEND_LABEL: &str = "-";

/// How the user pairs with a given backend (phase 6.4).
///
/// One mechanism, two transports: the server mints one short-lived code and
/// prints it as text *and* as a QR of `blue2th://pair?…`. This per-backend
/// setting only selects what the settings page offers for an entry that already
/// exists. Defaults to `Code`, which needs nothing but the terminal.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PairingMethod {
    /// Type the six characters shown in the server's terminal.
    #[default]
    Code,
    /// Scan the terminal's QR with the phone's own camera app, which routes the
    /// `blue2th://pair?…` deep link to the app.
    Qr,
}

/// A backend the user configured: the name the app is the source of truth for,
/// and the address every call goes to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendEntry {
    /// Display name, pushed to the backend as its Spotify Connect device name.
    pub name: String,
    /// Base URL, e.g. `http://192.168.1.107:4000` (no trailing slash).
    pub url: String,
    /// Whether that backend may re-select a returning speaker while playback
    /// runs (phase 6.3). Pushed with the name over `POST /config`; defaults to
    /// on, and `serde(default)` keeps a phase 6.2-era blob loadable.
    #[serde(default = "restore_during_playback_default")]
    pub restore_during_playback: bool,
    /// The API token obtained by pairing (phase 6.4), carried as
    /// `Authorization: Bearer <token>` on every call to that backend. `None`
    /// until the user pairs — the calls then fail fast as "not paired" rather
    /// than each screen failing on its own.
    #[serde(default)]
    pub token: Option<String>,
    /// How the settings page offers to pair with this backend.
    #[serde(default)]
    pub pairing: PairingMethod,
    /// The backend's stable id, learnt from its mDNS record or its pairing
    /// (phase 6.6). It is what identifies the *machine*, so a DHCP lease change
    /// repairs this entry instead of creating a second one. `None` for a pre-6.6
    /// entry, which keeps matching on its URL until an id is adopted.
    #[serde(default)]
    pub id: Option<String>,
}

/// The default for [`BackendEntry::restore_during_playback`]: on, so a speaker
/// that comes back rejoins without the user doing anything — the point of the
/// feature. A phase 6.2-era blob, which has no such field, therefore loads with
/// restoration enabled rather than silently disabled.
fn restore_during_playback_default() -> bool {
    true
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
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // The proto rule already words itself; forwarding keeps one wording
            // for a rule that lives in one place.
            SettingsError::Name(err) => write!(f, "{err}"),
            SettingsError::DuplicateName => write!(f, "another backend already uses that name"),
            SettingsError::MalformedUrl => write!(
                f,
                "the address must look like http://host:port, with no space"
            ),
            SettingsError::UnknownBackend => write!(f, "that backend no longer exists"),
        }
    }
}

impl std::error::Error for SettingsError {}

/// The whole persisted app configuration: the known backends and which one is
/// active. Exactly one backend is active at a time, or none at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppSettings {
    /// Every backend the user configured, in the order they were added.
    pub backends: Vec<BackendEntry>,
    /// Index into `backends` of the active one, or `None` when nothing is active.
    pub active: Option<usize>,
    /// Whether a discovered backend whose id matches a known entry has its
    /// address repaired in place, with no question asked (phase 6.6).
    ///
    /// The default fn is explicit rather than a bare `serde(default)`: that
    /// would yield `false` and silently opt every existing install out — the
    /// lesson already recorded in `restore_during_playback_default`.
    #[serde(default = "discovery_setting_default")]
    pub auto_repair_url: bool,
    /// Whether a discovered backend the app does not know can be added from the
    /// discovery list (phase 6.6). Adding never pairs: the six-character code is
    /// still required.
    #[serde(default = "discovery_setting_default")]
    pub discovery_adds_backends: bool,
}

/// The default for both phase 6.6 discovery settings: **on**. Finding the
/// backend by itself is the point of the feature, so a phase 6.4-era blob — which
/// carries neither field — must load with them enabled.
fn discovery_setting_default() -> bool {
    true
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            backends: Vec::new(),
            active: None,
            auto_repair_url: discovery_setting_default(),
            discovery_adds_backends: discovery_setting_default(),
        }
    }
}

/// What a discovered service means for the settings the app already holds.
///
/// Pure classification, computed by [`reconcile`] with no network and no JNI: the
/// UI only renders it and applies what the user (or the auto-repair setting)
/// decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryAction {
    /// A known backend answering at the address the app already has: nothing to
    /// repair, no write, no toast.
    UpToDate,
    /// A known backend that moved, with auto-repair on: update the URL in place.
    Repair {
        /// Index of the known entry in `backends`.
        index: usize,
        /// The normalised address it now answers at.
        url: String,
    },
    /// A known backend that moved, with auto-repair off: ask before writing.
    ConfirmRepair {
        /// Index of the known entry in `backends`.
        index: usize,
        /// The normalised address it now answers at.
        url: String,
    },
    /// An unknown backend the user may add (then pair with, as usual).
    Addable,
    /// Found and listed, but not actionable — an unknown backend while
    /// `discovery_adds_backends` is off, or an address that is not usable.
    Ignored,
}

/// Classify a discovered service against the settings the app holds. Pure.
///
/// Matching is by **id** first — that is the whole point of phase 6.6 — and falls
/// back to the URL when the service (or the entry) carries none, exactly as the
/// app behaved before. A missing id must never, on its own, make a known machine
/// look new.
pub fn reconcile(
    settings: &AppSettings,
    found: &blue2th_proto::DiscoveredBackend,
) -> DiscoveryAction {
    // An address that cannot be used is worse than no find at all: storing it
    // would swap a reachable entry for one every later call fails on.
    let Ok(url) = normalise_url(&found.url) else {
        return DiscoveryAction::Ignored;
    };

    // The id identifies the *machine*, so it wins over the address — that is the
    // whole point of the phase. The URL is only a fallback, for a service that
    // advertises no id or an entry that has not learnt one yet; a missing id must
    // never, on its own, make a known machine look new.
    let known = found
        .id
        .as_deref()
        .and_then(|id| {
            settings
                .backends
                .iter()
                .position(|b| b.id.as_deref() == Some(id))
        })
        .or_else(|| settings.backends.iter().position(|b| b.url == url));

    let Some(index) = known else {
        return if settings.discovery_adds_backends {
            DiscoveryAction::Addable
        } else {
            DiscoveryAction::Ignored
        };
    };

    // Matched by URL, or by id at the address already stored: nothing moved, so
    // nothing is written and nothing is said.
    if settings.backends.get(index).map(|b| b.url.as_str()) == Some(url.as_str()) {
        return DiscoveryAction::UpToDate;
    }
    if settings.auto_repair_url {
        DiscoveryAction::Repair { index, url }
    } else {
        DiscoveryAction::ConfirmRepair { index, url }
    }
}

/// Normalise a backend address: require a scheme, refuse blanks and embedded
/// whitespace, and trim a trailing slash so URL building stays predictable.
pub fn normalise_url(raw: &str) -> Result<String, SettingsError> {
    let trimmed = raw.trim();
    // An embedded space would silently produce an unreachable address; reject it
    // rather than percent-encode something the user did not mean.
    if trimmed.is_empty() || trimmed.chars().any(char::is_whitespace) {
        return Err(SettingsError::MalformedUrl);
    }
    let Some((scheme, rest)) = trimmed.split_once("://") else {
        return Err(SettingsError::MalformedUrl);
    };
    // `http://` alone carries a scheme but no host: it would build URLs that go
    // nowhere, so the host part must exist.
    let host = rest.trim_end_matches('/');
    // …and so must the hostname *inside* it: a typo like `http://:4000` has a
    // port and no host. reqwest refuses such a URL only when a call is built,
    // with an opaque "URL scheme is not allowed" — catching it here is what lets
    // the user read what they actually got wrong.
    let authority = host.split(['/', '?', '#']).next().unwrap_or(host);
    let hostname = authority.split(':').next().unwrap_or(authority);
    if scheme.is_empty() || host.is_empty() || hostname.is_empty() {
        return Err(SettingsError::MalformedUrl);
    }
    Ok(format!("{scheme}://{host}"))
}

impl AppSettings {
    /// Add a backend. The name goes through the shared proto validator and must
    /// not collide with an existing one; the URL is normalised.
    pub fn add(&mut self, name: &str, url: &str) -> Result<(), SettingsError> {
        let name = blue2th_proto::validate_backend_name(name).map_err(SettingsError::Name)?;
        if self.backends.iter().any(|b| b.name == name) {
            return Err(SettingsError::DuplicateName);
        }
        // Normalise before storing anything: a rejected entry must leave the list
        // untouched.
        let url = normalise_url(url)?;
        self.backends.push(BackendEntry {
            name,
            url,
            restore_during_playback: restore_during_playback_default(),
            // A backend is unpaired until the user runs the exchange, and the
            // typed code is the transport that needs nothing but the terminal.
            token: None,
            pairing: PairingMethod::default(),
            // Typing an address says nothing about which machine answers it; the
            // id is adopted the first time that backend is discovered or paired.
            id: None,
        });
        Ok(())
    }

    /// Repair the address of the backend at `index` (phase 6.6).
    ///
    /// The URL goes through [`normalise_url`], and **everything else is kept**:
    /// the token, the locally chosen name, the pairing method and the
    /// restore-during-playback flag. That is what makes a DHCP lease change a
    /// non-event rather than a re-pairing.
    pub fn set_url(&mut self, index: usize, url: &str) -> Result<(), SettingsError> {
        // Normalise before touching anything, exactly as `add` does: a rejected
        // repair must leave the whole list as it was.
        let url = normalise_url(url)?;
        let Some(entry) = self.backends.get_mut(index) else {
            return Err(SettingsError::UnknownBackend);
        };
        entry.url = url;
        Ok(())
    }

    /// Adopt the stable id of the backend at `index`, learnt from its mDNS record
    /// or its pairing. Never clears the token: the machine is the same one.
    pub fn set_backend_id(
        &mut self,
        index: usize,
        id: Option<String>,
    ) -> Result<(), SettingsError> {
        let Some(entry) = self.backends.get_mut(index) else {
            return Err(SettingsError::UnknownBackend);
        };
        entry.id = id;
        Ok(())
    }

    /// Adopt the ids of discovered services that match a known entry by URL while
    /// that entry carries none. Returns whether anything changed.
    ///
    /// This is what lets a **pre-6.6 entry** survive its next address change: it
    /// is matched on its URL alone until it learns an id, so without this the
    /// first lease change would offer the machine the app already knows as a
    /// brand new one — the very duplication phase 6.6 exists to stop.
    ///
    /// An id another entry already claims is never re-assigned: two entries
    /// answering to the same id would then both match every later find.
    pub fn adopt_discovered_ids(&mut self, found: &[blue2th_proto::DiscoveredBackend]) -> bool {
        let mut adopted = false;
        for service in found {
            let (Some(id), Ok(url)) = (service.id.as_deref(), normalise_url(&service.url)) else {
                continue;
            };
            if self.backends.iter().any(|b| b.id.as_deref() == Some(id)) {
                continue;
            }
            if let Some(entry) = self
                .backends
                .iter_mut()
                .find(|b| b.url == url && b.id.is_none())
            {
                entry.id = Some(id.to_string());
                adopted = true;
            }
        }
        adopted
    }

    /// Create an entry from a discovered service and return its index.
    ///
    /// Goes through the same rules as [`AppSettings::add`] — a service
    /// announcement cannot smuggle in a name typing would refuse — and leaves
    /// `token: None`: discovery is **not** authentication, so the app still
    /// reports "not paired" until the six-character code is exchanged.
    pub fn add_discovered(
        &mut self,
        found: &blue2th_proto::DiscoveredBackend,
    ) -> Result<usize, SettingsError> {
        // Reusing `add` is what makes the rules identical to typing: one
        // validator, one duplicate check, one normalisation. It pushes at the
        // end, so the new entry is the last one.
        self.add(&found.name, &found.url)?;
        let index = self.backends.len().saturating_sub(1);
        // Clone: the DTO is borrowed from the discovery list, which outlives this
        // call and may still be redrawn, while the entry needs its own copy.
        self.set_backend_id(index, found.id.clone())?;
        Ok(index)
    }

    /// Whether a discovered backend that moved is repaired without asking.
    pub fn set_auto_repair_url(&mut self, enabled: bool) {
        self.auto_repair_url = enabled;
    }

    /// Whether the discovery list may create entries for unknown backends.
    pub fn set_discovery_adds_backends(&mut self, enabled: bool) {
        self.discovery_adds_backends = enabled;
    }

    /// Store the API token obtained by pairing with the backend at `index`.
    pub fn set_token(&mut self, index: usize, token: Option<String>) -> Result<(), SettingsError> {
        let Some(entry) = self.backends.get_mut(index) else {
            return Err(SettingsError::UnknownBackend);
        };
        entry.token = token;
        Ok(())
    }

    /// Choose how the user pairs with the backend at `index`.
    pub fn set_pairing_method(
        &mut self,
        index: usize,
        method: PairingMethod,
    ) -> Result<(), SettingsError> {
        let Some(entry) = self.backends.get_mut(index) else {
            return Err(SettingsError::UnknownBackend);
        };
        entry.pairing = method;
        Ok(())
    }

    /// Apply a scanned `blue2th://pair?…` link: create the whole backend entry
    /// (address, name, token) or update the one that already carries that URL,
    /// and make it active. Returns its index.
    ///
    /// A URL the app already knows is **updated, never duplicated**, and its
    /// locally chosen name survives: the user may have renamed it deliberately,
    /// and the QR's name must not undo that.
    pub fn upsert_from_pair_link(
        &mut self,
        link: &PairLink,
        token: &str,
    ) -> Result<usize, SettingsError> {
        let url = normalise_url(&link.url)?;
        if let Some(index) = self.backends.iter().position(|b| b.url == url) {
            // Known address: only the token moves. The local name is kept — the
            // user may have renamed this backend deliberately, and a QR must not
            // undo that.
            self.set_token(index, Some(token.to_string()))?;
            self.activate(index)?;
            return Ok(index);
        }

        // Creating needs a name, and it must not collide: both are the same
        // rules `add` applies, so a link cannot smuggle in what typing cannot.
        let name = link
            .name
            .as_deref()
            .ok_or(SettingsError::Name(NameError::Empty))?;
        self.add(name, &url)?;
        let index = self.backends.len().saturating_sub(1);
        self.set_token(index, Some(token.to_string()))?;
        // The method is chosen when a backend is added, and this one was added by
        // scanning: offering the QR again is what re-pairing it will most likely
        // mean. Only on creation — for a known entry the user's own choice wins,
        // exactly as their chosen name does.
        self.set_pairing_method(index, PairingMethod::Qr)?;
        self.activate(index)?;
        Ok(index)
    }

    /// The active backend's API token, or `None` when nothing is active or the
    /// active backend has not been paired yet.
    pub fn active_token(&self) -> Option<String> {
        self.active_backend().and_then(|b| b.token.clone())
    }

    /// Toggle the restore-during-playback setting of the backend at `index`.
    /// The app is the source of truth for it, exactly as for the name, and
    /// pushes it over `POST /config`.
    pub fn set_restore_during_playback(
        &mut self,
        index: usize,
        enabled: bool,
    ) -> Result<(), SettingsError> {
        let Some(entry) = self.backends.get_mut(index) else {
            return Err(SettingsError::UnknownBackend);
        };
        entry.restore_during_playback = enabled;
        Ok(())
    }

    /// Remove the backend at `index`. Removing the active one leaves no active
    /// backend at all (the app then knows no address).
    pub fn remove(&mut self, index: usize) -> Result<(), SettingsError> {
        if index >= self.backends.len() {
            return Err(SettingsError::UnknownBackend);
        }
        self.backends.remove(index);
        // The active index refers to a position, so removing an earlier entry
        // shifts it; removing the active one leaves no active backend at all.
        self.active = match self.active {
            Some(active) if active == index => None,
            Some(active) if active > index => Some(active - 1),
            other => other,
        };
        Ok(())
    }

    /// Make the backend at `index` the active one (exactly one at a time).
    pub fn activate(&mut self, index: usize) -> Result<(), SettingsError> {
        if index >= self.backends.len() {
            return Err(SettingsError::UnknownBackend);
        }
        self.active = Some(index);
        Ok(())
    }

    /// The active backend, or `None` when nothing is configured/active.
    pub fn active_backend(&self) -> Option<&BackendEntry> {
        self.backends.get(self.active?)
    }

    /// The active backend's URL, or `None` — there is no fallback address of any
    /// kind, so an unconfigured app sends nothing anywhere.
    pub fn active_url(&self) -> Option<String> {
        self.active_backend().map(|b| b.url.clone())
    }

    /// What the status encart shows: the active backend's name, or
    /// [`NO_BACKEND_LABEL`] when none is active.
    pub fn active_label(&self) -> String {
        self.active_backend()
            .map(|b| b.name.clone())
            .unwrap_or_else(|| NO_BACKEND_LABEL.to_string())
    }
}

/// Parse the persisted blob. A missing, empty, malformed or non-JSON blob yields
/// empty settings: a broken preferences entry must never prevent a start.
pub fn load(raw: Option<&str>) -> AppSettings {
    let mut settings: AppSettings = raw
        .map(str::trim)
        .filter(|blob| !blob.is_empty())
        .and_then(|blob| serde_json::from_str(blob).ok())
        .unwrap_or_default();
    // A stored index pointing outside the list would panic nothing but would show
    // a backend that is not there; repair it rather than trust the blob.
    if settings
        .active
        .is_some_and(|i| i >= settings.backends.len())
    {
        settings.active = None;
    }
    settings
}

/// Serialize the settings for the storage seam.
pub fn save_blob(settings: &AppSettings) -> String {
    // Serialization of a plain struct cannot realistically fail; an empty blob
    // would simply load back as empty settings, which is the safe direction.
    serde_json::to_string(settings).unwrap_or_default()
}

/// The settings currently in memory, backing the runtime backend lookup.
pub fn current() -> AppSettings {
    // Owned copy: the lock must never be held across an await in the HTTP paths.
    cache().read().map(|s| s.clone()).unwrap_or_default()
}

/// Replace the in-memory settings and persist them (Android storage seam).
pub fn set_current(settings: AppSettings) {
    write_stored(&save_blob(&settings));
    if let Ok(mut guard) = cache().write() {
        *guard = settings;
    }
}

/// The process-wide settings cache, seeded on first use from the phone's storage
/// (there is nothing to read off Android, so it starts empty there).
static CACHE: std::sync::OnceLock<std::sync::RwLock<AppSettings>> = std::sync::OnceLock::new();

fn cache() -> &'static std::sync::RwLock<AppSettings> {
    CACHE.get_or_init(|| std::sync::RwLock::new(load(read_stored().as_deref())))
}

/// Preferences file and key holding the serialized settings.
#[cfg(target_os = "android")]
const PREFS_NAME: &str = "blue2th";
#[cfg(target_os = "android")]
const PREFS_KEY: &str = "settings";

/// Read the persisted blob from `SharedPreferences`.
///
/// Android storage seam (JNI), mirroring `src/deep_link.rs`: not exercised by CI,
/// validated on a device. Any failure reads as "nothing stored", which loads as
/// empty settings rather than blocking the app.
#[cfg(target_os = "android")]
fn read_stored() -> Option<String> {
    use jni::objects::{JObject, JString, JValue};

    let ctx = ndk_context::android_context();
    // SAFETY: ndk-context holds the JavaVM pointer set by the Android runtime
    // before any Rust code runs; it is valid for the process lifetime.
    let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }.ok()?;
    let mut env = vm
        .get_env()
        .or_else(|_| vm.attach_current_thread_permanently())
        .ok()?;
    // SAFETY: the context pointer is the app's Activity object, owned by the runtime.
    let activity = unsafe { JObject::from_raw(ctx.context().cast()) };

    let stored = (|| {
        let name = env.new_string(PREFS_NAME).ok()?;
        let prefs = env
            .call_method(
                &activity,
                "getSharedPreferences",
                "(Ljava/lang/String;I)Landroid/content/SharedPreferences;",
                &[JValue::Object(&name), JValue::Int(0)],
            )
            .ok()?
            .l()
            .ok()?;
        let key = env.new_string(PREFS_KEY).ok()?;
        let null = JObject::null();
        let stored = env
            .call_method(
                &prefs,
                "getString",
                "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
                &[JValue::Object(&key), JValue::Object(&null)],
            )
            .ok()?
            .l()
            .ok()?;
        if stored.is_null() {
            return None;
        }
        env.get_string(&JString::from(stored))
            .ok()
            .map(Into::<String>::into)
    })();
    if stored.is_none() {
        // Mirrors `write_stored` and `deep_link.rs`: leaving an exception pending
        // aborts the process on the next JNI call. Clearing when nothing was
        // thrown (the "never stored anything" case) is a no-op.
        let _ = env.exception_clear();
    }
    stored
}

/// Persist the blob to `SharedPreferences` (Android storage seam).
#[cfg(target_os = "android")]
fn write_stored(blob: &str) {
    use jni::objects::{JObject, JValue};

    let ctx = ndk_context::android_context();
    // SAFETY: see `read_stored`.
    let Ok(vm) = (unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }) else {
        return;
    };
    let Ok(mut env) = vm
        .get_env()
        .or_else(|_| vm.attach_current_thread_permanently())
    else {
        return;
    };
    // SAFETY: see `read_stored`.
    let activity = unsafe { JObject::from_raw(ctx.context().cast()) };

    let written = (|| {
        let name = env.new_string(PREFS_NAME).ok()?;
        let prefs = env
            .call_method(
                &activity,
                "getSharedPreferences",
                "(Ljava/lang/String;I)Landroid/content/SharedPreferences;",
                &[JValue::Object(&name), JValue::Int(0)],
            )
            .ok()?
            .l()
            .ok()?;
        let editor = env
            .call_method(
                &prefs,
                "edit",
                "()Landroid/content/SharedPreferences$Editor;",
                &[],
            )
            .ok()?
            .l()
            .ok()?;
        let key = env.new_string(PREFS_KEY).ok()?;
        let value = env.new_string(blob).ok()?;
        env.call_method(
            &editor,
            "putString",
            "(Ljava/lang/String;Ljava/lang/String;)Landroid/content/SharedPreferences$Editor;",
            &[JValue::Object(&key), JValue::Object(&value)],
        )
        .ok()?;
        env.call_method(&editor, "apply", "()V", &[]).ok()?;
        Some(())
    })();
    if written.is_none() {
        // Leaving an exception pending aborts the process on the next JNI call.
        let _ = env.exception_clear();
    }
}

/// No phone storage off Android: the settings live for the process only.
#[cfg(not(target_os = "android"))]
fn read_stored() -> Option<String> {
    None
}

/// No phone storage off Android; the in-memory cache still applies.
#[cfg(not(target_os = "android"))]
fn write_stored(_blob: &str) {}
