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
//! `src/bluetooth.rs` grew its own `android_jni_env` / `bt_err_clear`; the
//! multicast lock needs exactly the same two disciplines, so they are lifted here
//! with a generic error type rather than copied:
//!
//! - **never** `attach_current_thread()`: its `AttachGuard` detaches on drop and
//!   the next `FindClass` on that thread aborts the process;
//! - **always** capture a pending Java exception's `toString()` and clear it
//!   before returning — an uncleared exception is an ART abort at the next JNI
//!   call, which no `Result` can catch.
//!
//! It also holds the class/method preflight probe, cached in a `OnceLock`: a
//! stripped OEM ROM without `WifiManager.MulticastLock` must disable the Search
//! button, not fail at the moment the user taps it.

/// A JNI failure, carrying the Java exception's own message when there was one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JniError(String);

impl std::fmt::Display for JniError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for JniError {}

impl JniError {
    /// Wrap a message (a Java exception's `toString()`, or a Rust-side detail).
    pub fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

/// Whether Android's `WifiManager.MulticastLock` could be resolved, computed
/// once and cached.
///
/// The probe runs at startup and its verdict is what the Search button's enabled
/// state derives from: a ROM where `FindClass`/`GetMethodID` cannot resolve the
/// class must render the button disabled, with no JNI call ever attempted.
///
/// Off Android there is nothing to lock and the browse runs anyway, so the
/// verdict is `true` — the desktop test build keeps the button live.
pub fn multicast_supported() -> bool {
    todo!("phase 6.6: cache the JNI preflight verdict in a OnceLock")
}

/// The `JNIEnv` for the current thread, attaching it permanently if needed.
///
/// Never `attach_current_thread()`: see the module docs.
#[cfg(target_os = "android")]
pub fn env(vm: &jni::JavaVM) -> Result<jni::JNIEnv<'_>, JniError> {
    let _ = vm;
    todo!("phase 6.6: get_env() or attach_current_thread_permanently()")
}

/// Capture a pending Java exception's `toString()` **before** clearing it, and
/// return it as a [`JniError`]. Mirrors `bt_err_clear` in `src/bluetooth.rs`.
#[cfg(target_os = "android")]
pub fn err_clear(env: &mut jni::JNIEnv<'_>, e: jni::errors::Error) -> JniError {
    let _ = (env, e);
    todo!("phase 6.6: exception_occurred -> toString -> exception_clear")
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
