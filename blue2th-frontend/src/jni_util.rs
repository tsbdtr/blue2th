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

//! The shared JNI seam (phase 6.6).
//!
//! The now-deleted on-phone Bluetooth stack grew its own attach and
//! exception-clearing helpers; the multicast lock needs exactly the same two
//! disciplines, so they live here with a generic error type. This module is the
//! only JNI seam left in the crate:
//!
//! - **never** the plain `attach_current_thread` call: its `AttachGuard` detaches
//!   on drop and the next `FindClass` on that thread aborts the process. Attach
//!   permanently instead;
//! - **always** capture a pending Java exception's `toString()` and clear it
//!   before returning — an uncleared exception is an ART abort at the next JNI
//!   call, which no `Result` can catch.
//!
//! It also holds the class/method preflight probe, cached in a `OnceLock`: a
//! stripped OEM ROM without `WifiManager.MulticastLock` must disable the Search
//! button, not fail at the moment the user taps it.

/// A JNI failure, carrying the Java exception's own message when there was one.
///
/// Only ever built on Android, where the JNI calls live; the desktop build keeps
/// the type so the error path compiles and stays testable.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub struct JniError(String);

impl std::fmt::Display for JniError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for JniError {}

impl JniError {
    /// Wrap a message (a Java exception's `toString()`, or a Rust-side detail).
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

/// The `JNIEnv` for the current thread, attaching it permanently if needed.
///
/// Never the plain `attach_current_thread` call: its `AttachGuard` detaches a
/// Java thread from the JVM on drop, and the next `FindClass` on that thread
/// aborts the process.
#[cfg(target_os = "android")]
pub fn env(vm: &jni::JavaVM) -> Result<jni::JNIEnv<'_>, JniError> {
    vm.get_env()
        .or_else(|_| vm.attach_current_thread_permanently())
        .map_err(|e| JniError::new(e.to_string()))
}

/// Capture a pending Java exception's `toString()` **before** clearing it, and
/// return it as a [`JniError`].
///
/// The order is the whole point: an uncleared exception aborts the process at the
/// next JNI call, and reading it after the clear yields nothing, so the error
/// card would show the jni crate's generic "Java exception was thrown".
#[cfg(target_os = "android")]
pub fn err_clear(env: &mut jni::JNIEnv<'_>, e: jni::errors::Error) -> JniError {
    let detail = match env.exception_occurred() {
        Ok(throwable) if !throwable.is_null() => {
            // Must clear before making any further JNI call on this thread.
            let _ = env.exception_clear();
            let detail = env
                .call_method(&throwable, "toString", "()Ljava/lang/String;", &[])
                .ok()
                .and_then(|v| v.l().ok())
                .and_then(|s| {
                    env.get_string(&jni::objects::JString::from(s))
                        .ok()
                        .map(Into::<String>::into)
                });
            // Defensive: if the toString/get_string path itself raised (an OOM,
            // say), clear that too rather than return with one pending.
            let _ = env.exception_clear();
            detail
        },
        _ => {
            // No retrievable throwable; still clear anything pending.
            let _ = env.exception_clear();
            None
        },
    };
    JniError::new(detail.unwrap_or_else(|| e.to_string()))
}

/// Whether Android's `WifiManager.MulticastLock` could be resolved, computed
/// once and cached.
///
/// The probe runs on first read and its verdict is what the Search button's
/// enabled state derives from: a ROM where `FindClass` cannot resolve the class
/// renders the button disabled, with no JNI call ever attempted on tap.
///
/// Off Android there is nothing to lock and the browse runs anyway, so the
/// verdict is `true` — the desktop test build keeps the button live.
#[cfg(target_os = "android")]
pub fn multicast_supported() -> bool {
    static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SUPPORTED.get_or_init(probe_multicast)
}

/// Resolve the two classes the multicast lock needs. Runs once, behind the
/// `OnceLock` in [`multicast_supported`].
#[cfg(target_os = "android")]
fn probe_multicast() -> bool {
    let ctx = ndk_context::android_context();
    // SAFETY: `ndk_context` hands back the process-wide `JavaVM` the Android
    // runtime installed; it outlives every call made through it.
    let Ok(vm) = (unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }) else {
        return false;
    };
    let Ok(mut env) = env(&vm) else {
        return false;
    };
    let resolved = env.find_class("android/net/wifi/WifiManager").is_ok()
        && env
            .find_class("android/net/wifi/WifiManager$MulticastLock")
            .is_ok();
    if !resolved {
        // A failed `FindClass` leaves a `NoClassDefFoundError` pending, and the
        // verdict alone is all this probe reports — clear it so the next JNI
        // call on this thread does not abort.
        let _ = env.exception_clear();
    }
    resolved
}

/// No JVM off Android, so nothing to probe: the browse runs unlocked and the
/// button stays live.
#[cfg(not(target_os = "android"))]
pub fn multicast_supported() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    // Criterion: the JNI preflight result is **cached** — the probe runs once and
    // every later read returns the same verdict, so no JNI call is attempted per
    // button render.
    #[test]
    fn test_multicast_supported_is_stable_across_calls() {
        let first = multicast_supported();
        for _ in 0..4 {
            assert_eq!(
                multicast_supported(),
                first,
                "the preflight verdict is cached, so it cannot change mid-run"
            );
        }
    }

    // Criterion (non-nominal): off Android the multicast lock is a no-op stub and
    // the browse still runs, so the verdict is positive on a desktop build.
    #[test]
    #[cfg(not(target_os = "android"))]
    fn test_multicast_supported_is_true_off_android() {
        assert!(
            multicast_supported(),
            "off Android there is nothing to lock: the browse runs anyway"
        );
    }

    // Criterion: a shared JNI helper surfaces the Java exception's own detail,
    // so the settings page error card shows the cause rather than the jni
    // crate's generic "Java exception was thrown".
    #[test]
    fn test_jni_error_displays_the_captured_detail() {
        let err = JniError::new("java.lang.SecurityException: no multicast");
        assert_eq!(
            err.to_string(),
            "java.lang.SecurityException: no multicast",
            "the captured Java detail must reach the user unchanged"
        );
    }
}
