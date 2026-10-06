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
//! `arm`.
//!
//! The browser build (#160) reports from the page's own lifecycle events
//! instead — `load`, `visibilitychange` and `pagehide` — mapped by
//! [`presence_for`], which never yields `Gone`: a reload fires `pagehide`
//! exactly as a close does.

#[cfg(not(target_arch = "wasm32"))]
use std::sync::OnceLock;

use blue2th_proto::ClientPresence;

/// The app's tokio runtime, captured from a task so the JNI callbacks (which run
/// on a Java thread) have somewhere to run their request.
#[cfg(not(target_arch = "wasm32"))]
static RUNTIME: OnceLock<tokio::runtime::Handle> = OnceLock::new();

/// Remember the runtime the presence reports should run on. Called once from a
/// spawned task, where `Handle::current()` is guaranteed to resolve. Further
/// calls are ignored.
#[cfg(not(target_arch = "wasm32"))]
pub fn arm(handle: tokio::runtime::Handle) {
    let _ = RUNTIME.set(handle);
}

/// How long a `Gone` report may block the activity's `onDestroy`: enough for a
/// LAN round-trip, far short of the ANR watchdog.
#[cfg(not(target_arch = "wasm32"))]
const GONE_REPORT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);

/// Report a presence change, if the runtime is armed.
///
/// Best-effort by design: Android may freeze the process right after `onStop`,
/// and a report that does not make it out only costs a wider grace period on the
/// backend — never correctness, and never a message to a user who has left.
#[cfg(not(target_arch = "wasm32"))]
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
#[cfg(not(target_arch = "wasm32"))]
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

/// A page lifecycle event the browser build listens to (#160).
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageEvent {
    /// `load`: the page was opened or reloaded.
    Load,
    /// `visibilitychange` to `hidden`: the tab was switched away from.
    Hidden,
    /// `visibilitychange` to `visible`: the tab is on screen again.
    Visible,
    /// `pagehide`: the page is being left — closed, **or reloaded**.
    PageHide,
}

/// The presence a page event reports. Pure.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub fn presence_for(event: PageEvent) -> ClientPresence {
    match event {
        PageEvent::Load | PageEvent::Visible => ClientPresence::Foreground,
        // `pagehide` is also a reload, and `Gone` pauses Spotify at once: a
        // closed tab is left to the watchdog's background grace instead.
        PageEvent::Hidden | PageEvent::PageHide => ClientPresence::Background,
    }
}

/// Post the presence `event` maps to, best-effort: a failure is ignored, as on
/// Android. Sent with `fetch` rather than `reqwest`, whose wasm client has no
/// `keepalive` — and a post from `pagehide` without it dies with the page.
#[cfg(target_arch = "wasm32")]
fn report_page_event(event: PageEvent) {
    use web_sys::js_sys::Reflect;
    use web_sys::wasm_bindgen::JsValue;

    let settings = crate::settings::current();
    let Ok(post) = crate::backend::browser_presence_post(&settings, presence_for(event)) else {
        return;
    };
    let Some(window) = web_sys::window() else {
        return;
    };
    let Ok(headers) = web_sys::Headers::new() else {
        return;
    };
    if headers.set("authorization", &post.authorization).is_err()
        || headers.set("content-type", "application/json").is_err()
    {
        return;
    }
    let init = web_sys::RequestInit::new();
    init.set_method("POST");
    init.set_headers(&headers);
    init.set_body(&JsValue::from_str(&post.body));
    // web-sys 0.3.99's `RequestInit` has no `keepalive` setter: set the field
    // on the dictionary itself.
    if Reflect::set(
        &init,
        &JsValue::from_str("keepalive"),
        &JsValue::from_bool(post.keepalive),
    )
    .is_err()
    {
        return;
    }
    // The returned promise is dropped: nothing waits for a best-effort report.
    let _ = window.fetch_with_str_and_init(&post.url, &init);
}

/// Install the page listeners that report presence (#160): `load` (or at once
/// when the page has already loaded), `visibilitychange` and `pagehide`. Called
/// once at start; the listeners live as long as the page.
#[cfg(target_arch = "wasm32")]
pub fn install_page_listeners() {
    use web_sys::wasm_bindgen::closure::Closure;
    use web_sys::wasm_bindgen::JsCast as _;

    let Some(window) = web_sys::window() else {
        return;
    };
    let Some(document) = window.document() else {
        return;
    };

    if document.ready_state() == "complete" {
        // The app starts after `load` whenever the wasm arrives late: the
        // event will not fire again, so the page reports it now.
        report_page_event(PageEvent::Load);
    } else {
        let on_load = Closure::<dyn FnMut()>::new(|| report_page_event(PageEvent::Load));
        let _ = window.add_event_listener_with_callback("load", on_load.as_ref().unchecked_ref());
        // Leaked on purpose: the listener lives as long as the page.
        on_load.forget();
    }

    // Cloned handle: the closure owns its own reference to the document it
    // reads the visibility from, for as long as the listener lives.
    let watched = document.clone();
    let on_visibility = Closure::<dyn FnMut()>::new(move || {
        let event = match watched.visibility_state() {
            web_sys::VisibilityState::Hidden => PageEvent::Hidden,
            _ => PageEvent::Visible,
        };
        report_page_event(event);
    });
    let _ = document.add_event_listener_with_callback(
        "visibilitychange",
        on_visibility.as_ref().unchecked_ref(),
    );
    on_visibility.forget();

    let on_pagehide = Closure::<dyn FnMut()>::new(|| report_page_event(PageEvent::PageHide));
    let _ =
        window.add_event_listener_with_callback("pagehide", on_pagehide.as_ref().unchecked_ref());
    on_pagehide.forget();
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
