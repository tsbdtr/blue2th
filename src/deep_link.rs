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

//! Custom-scheme deep links (phase 5.2).
//!
//! Spotify redirects the OAuth consent to `blue2th://spotify-callback?code=…&state=…`,
//! which Android routes to the app via the `blue2th` scheme declared in
//! `Dioxus.toml` (`[deep_links]`). This module turns that redirect into the
//! authorization code the app hands to the backend:
//!
//! - [`take_pending_deep_link`] reads the activity's current intent URI over JNI
//!   and clears it, so a redirect is consumed exactly once.
//! - [`parse_spotify_callback`] is pure (and unit-tested on every platform): it
//!   maps the URI to a granted code or a denial.
//!
//! On non-Android targets the reader yields `None` — there is no deep link to
//! consume — while the parser stays available so the logic is testable on the PC.

/// The redirect URI registered with Spotify and declared as the app's custom
/// scheme. Must stay in sync with the backend's `DEFAULT_REDIRECT_URI`.
pub const SPOTIFY_CALLBACK_URI: &str = "blue2th://spotify-callback";

/// The outcome of the Spotify consent screen, as carried by the redirect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpotifyCallback {
    /// The user consented: a one-time `code` plus the CSRF `state` to echo back.
    Authorized { code: String, state: String },
    /// The user (or Spotify) refused; carries the raw `error` code, e.g.
    /// `access_denied`.
    Denied(String),
}

/// Parse a Spotify OAuth redirect URI.
///
/// Returns `None` when the URI is not our callback, carries no query, or is
/// missing the `code`/`state` pair — an incomplete redirect is ignored rather
/// than surfaced as a failure, since Android may hand us the launcher intent.
pub fn parse_spotify_callback(uri: &str) -> Option<SpotifyCallback> {
    let rest = uri.strip_prefix(SPOTIFY_CALLBACK_URI)?;
    // Tolerate a trailing slash before the query (`…/spotify-callback/?code=…`).
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    let query = rest.strip_prefix('?')?;
    // A fragment is never part of the query.
    let query = query.split('#').next().unwrap_or(query);

    let mut code = None;
    let mut state = None;
    let mut error = None;
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        match key {
            "code" => code = Some(percent_decode(value)),
            "state" => state = Some(percent_decode(value)),
            "error" => error = Some(percent_decode(value)),
            _ => {},
        }
    }

    // A denial takes precedence: Spotify sends `error` instead of `code`.
    if let Some(error) = error {
        return Some(SpotifyCallback::Denied(error));
    }
    Some(SpotifyCallback::Authorized {
        code: code?,
        state: state?,
    })
}

/// Decode a URL query value: `%XX` escapes and `+` as a space. Invalid escapes
/// are left as-is rather than dropped, so a malformed value stays visible.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            },
            // Decoded from the raw bytes, never by re-slicing the &str: a `%`
            // followed by a multi-byte character would split it and panic.
            b'%' if i + 2 < bytes.len() => match (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2]))
            {
                (Some(high), Some(low)) => {
                    out.push((high << 4) | low);
                    i += 3;
                },
                _ => {
                    out.push(b'%');
                    i += 1;
                },
            },
            byte => {
                out.push(byte);
                i += 1;
            },
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The value of a single ASCII hex digit, or `None` if it is not one.
fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Take the URI the app was opened with, if any, and clear it so the same
/// redirect is never consumed twice.
///
/// Android delivers it either as the launch intent (app was not running) or via
/// `onNewIntent` (app in the background), which the custom `MainActivity.kt`
/// forwards to `setIntent` so `getIntent` below observes it.
#[cfg(target_os = "android")]
pub fn take_pending_deep_link() -> Option<String> {
    use jni::objects::{JObject, JString, JValue};

    let ctx = ndk_context::android_context();
    // SAFETY: ndk-context stores the JavaVM pointer set by the Android runtime before any
    // Rust code runs; it stays valid for the process lifetime.
    let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }.ok()?;
    // Never attach_current_thread(): its guard detaches a Java thread on drop, which
    // aborts the process on the next JNI call (see `bluetooth::android_jni_env`).
    let mut env = vm
        .get_env()
        .or_else(|_| vm.attach_current_thread_permanently())
        .ok()?;
    // SAFETY: the context pointer is the app's Activity object, owned by the runtime.
    let activity = unsafe { JObject::from_raw(ctx.context().cast()) };

    let intent = call_object(
        &mut env,
        &activity,
        "getIntent",
        "()Landroid/content/Intent;",
    )?;
    let data = call_object(&mut env, &intent, "getDataString", "()Ljava/lang/String;")?;
    let uri: String = env
        .get_string(&JString::from(data))
        .map_err(|_| clear_exception(&mut env))
        .ok()?
        .into();

    // Consume it: without this the same code would be replayed on every poll and
    // rejected by the backend as an already-used authorization.
    let null = JObject::null();
    if env
        .call_method(
            &intent,
            "setData",
            "(Landroid/net/Uri;)Landroid/content/Intent;",
            &[JValue::Object(&null)],
        )
        .is_err()
    {
        clear_exception(&mut env);
    }

    Some(uri)
}

/// Hand `url` to the system browser via `Intent.ACTION_VIEW`, so the OAuth
/// consent runs outside the app's WebView (which could not handle the
/// `blue2th://` redirect) and comes back through the deep link.
///
/// Returns whether the intent was actually fired, so the caller can surface a
/// failure instead of leaving the user staring at an unresponsive button.
#[cfg(target_os = "android")]
pub fn open_in_browser(url: &str) -> bool {
    use jni::objects::{JObject, JValue};

    let ctx = ndk_context::android_context();
    // SAFETY: the JavaVM pointer is set by the Android runtime before any Rust
    // code runs and stays valid for the process lifetime.
    let Ok(vm) = (unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }) else {
        return false;
    };
    let Ok(mut env) = vm
        .get_env()
        .or_else(|_| vm.attach_current_thread_permanently())
    else {
        return false;
    };
    // SAFETY: the context pointer is the app's Activity object, owned by the runtime.
    let activity = unsafe { JObject::from_raw(ctx.context().cast()) };

    let fired = (|| {
        let url_string = env.new_string(url).ok()?;
        let uri = env
            .call_static_method(
                "android/net/Uri",
                "parse",
                "(Ljava/lang/String;)Landroid/net/Uri;",
                &[JValue::Object(&url_string)],
            )
            .ok()?
            .l()
            .ok()?;
        let action = env.new_string("android.intent.action.VIEW").ok()?;
        let intent = env
            .new_object(
                "android/content/Intent",
                "(Ljava/lang/String;Landroid/net/Uri;)V",
                &[JValue::Object(&action), JValue::Object(&uri)],
            )
            .ok()?;
        env.call_method(
            &activity,
            "startActivity",
            "(Landroid/content/Intent;)V",
            &[JValue::Object(&intent)],
        )
        .ok()?;
        Some(())
    })()
    .is_some();

    if !fired {
        clear_exception(&mut env);
    }
    fired
}

/// No browser hand-off outside Android; reported as a failure so the caller
/// surfaces it rather than silently doing nothing.
#[cfg(not(target_os = "android"))]
pub fn open_in_browser(_url: &str) -> bool {
    false
}

/// Call a no-argument JNI method returning an object, mapping a null result or a
/// Java exception to `None` (the exception is cleared, never left pending).
#[cfg(target_os = "android")]
fn call_object<'a>(
    env: &mut jni::JNIEnv<'a>,
    receiver: &jni::objects::JObject<'_>,
    name: &str,
    signature: &str,
) -> Option<jni::objects::JObject<'a>> {
    let value = match env.call_method(receiver, name, signature, &[]) {
        Ok(value) => value,
        Err(_) => {
            clear_exception(env);
            return None;
        },
    };
    let object = value.l().ok()?;
    if object.is_null() {
        return None;
    }
    Some(object)
}

/// Clear any pending Java exception. Leaving one pending aborts the process on
/// the next JNI call from the same thread (e.g. the Dioxus WebView handler).
#[cfg(target_os = "android")]
fn clear_exception(env: &mut jni::JNIEnv<'_>) {
    let _ = env.exception_clear();
}

/// No deep link outside Android: the desktop build has no custom-scheme intent.
#[cfg(not(target_os = "android"))]
pub fn take_pending_deep_link() -> Option<String> {
    None
}
