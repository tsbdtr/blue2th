// SPDX-License-Identifier: MIT OR Apache-2.0

//! Android activity lifecycle → presence reports (phase 5.2).
//!
//! Playback keeps going while the app is backgrounded, which is what the user
//! wants — but Android freezes a backgrounded app within seconds, dropping the
//! now-playing SSE feed the backend uses as a heartbeat. A frozen app and a dead
//! one look identical from the PC.
//!
//! So the app says which one it is: `onStart`/`onStop` report foreground and
//! background (the backend widens its grace period accordingly), and a real exit
//! reports `Gone`, which pauses immediately.
//!
//! That exit arrives through two paths, because Android has no single one:
//! `MainActivity.onDestroy` covers a clean finish (back button), while
//! `Blue2thPresenceService.onTaskRemoved` covers a swipe out of the recents list —
//! there the activity is killed without `onDestroy` ever running.
//!
//! The callbacks arrive on a Java thread with no async context, so the request is
//! handed to the app's tokio runtime through a handle captured at startup by
//! [`arm`].

use std::sync::OnceLock;

use blue2th_proto::ClientPresence;

/// The app's tokio runtime, captured from a task so the JNI callbacks (which run
/// on a Java thread) have somewhere to run their request.
static RUNTIME: OnceLock<tokio::runtime::Handle> = OnceLock::new();

/// Remember the runtime the presence reports should run on. Called once from a
/// spawned task, where `Handle::current()` is guaranteed to resolve. Further
/// calls are ignored.
pub fn arm(handle: tokio::runtime::Handle) {
    let _ = RUNTIME.set(handle);
}

/// How long a `Gone` report may block the activity's `onDestroy`: enough for a
/// LAN round-trip, far short of the ANR watchdog.
const GONE_REPORT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);

/// Report a presence change, if the runtime is armed.
///
/// Best-effort by design: Android may freeze the process right after `onStop`,
/// and a report that does not make it out only costs a wider grace period on the
/// backend — never correctness, and never a message to a user who has left.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub fn report(presence: ClientPresence) {
    let Some(handle) = RUNTIME.get() else {
        return;
    };
    handle.spawn(async move {
        let _ = crate::backend::report_presence(presence).await;
    });
}

/// Report `Gone` and wait for it to actually leave the device.
///
/// Spawning would not do: `onDestroy` is the last thing to run before Android
/// tears the process down, so a detached task is very unlikely to ever be polled
/// — the request would simply die with the app, which is exactly the "closing
/// blue2th does not pause the music" symptom.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub fn report_gone_blocking() {
    let Some(handle) = RUNTIME.get() else {
        return;
    };
    // `block_on` panics inside a runtime worker. The JNI callback runs on a Java
    // thread, so this branch is unreachable in practice — but it keeps the
    // guarantee explicit rather than relying on the caller's thread.
    if tokio::runtime::Handle::try_current().is_ok() {
        report(ClientPresence::Gone);
        return;
    }
    let _ = handle.block_on(async {
        tokio::time::timeout(
            GONE_REPORT_TIMEOUT,
            crate::backend::report_presence(ClientPresence::Gone),
        )
        .await
    });
}

/// Called by `MainActivity.onStart`: the app is on screen.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "C" fn Java_dev_dioxus_main_MainActivity_nativeOnForeground(
    _env: jni::JNIEnv,
    _activity: jni::objects::JObject,
) {
    report(ClientPresence::Foreground);
}

/// Called by `MainActivity.onStop`: backgrounded, but still very much alive.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "C" fn Java_dev_dioxus_main_MainActivity_nativeOnBackground(
    _env: jni::JNIEnv,
    _activity: jni::objects::JObject,
) {
    report(ClientPresence::Background);
}

/// Called by `MainActivity.onDestroy` when the activity is finishing for good.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "C" fn Java_dev_dioxus_main_MainActivity_nativeOnGone(
    _env: jni::JNIEnv,
    _activity: jni::objects::JObject,
) {
    report_gone_blocking();
}

/// Called by `Blue2thPresenceService.onTaskRemoved`: blue2th was swiped out of
/// the recents list. This is the only reliable signal for that exit — the
/// activity's `onDestroy` is not called when Android kills the task.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "C" fn Java_dev_dioxus_main_Blue2thPresenceService_nativeOnGone(
    _env: jni::JNIEnv,
    _service: jni::objects::JObject,
) {
    report_gone_blocking();
}
