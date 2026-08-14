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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppSettings {
    /// Every backend the user configured, in the order they were added.
    pub backends: Vec<BackendEntry>,
    /// Index into `backends` of the active one, or `None` when nothing is active.
    pub active: Option<usize>,
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
    if scheme.is_empty() || host.is_empty() {
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
        self.backends.push(BackendEntry { name, url });
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
    if stored.is_none() {
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
