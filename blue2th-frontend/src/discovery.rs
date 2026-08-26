// SPDX-License-Identifier: MIT OR Apache-2.0

//! Finding the backend on the LAN (phase 6.6).
//!
//! Browsing is pure Rust (`mdns-sd`); JNI is used **only** to hold Android's
//! `WifiManager.MulticastLock` while the browse runs — three synchronous calls,
//! no callback, no `.dex`, no `RegisterNatives`. `NsdManager` is deliberately not
//! used: its `DiscoveryListener` is a Java interface Rust cannot implement.
//!
//! The network itself is a manual-test boundary, exactly as BlueZ and PipeWire
//! are on the backend. Everything the app *decides* — whether the Search button
//! is live, whether a failed lock stops the browse, and what a find means for the
//! settings ([`crate::settings::reconcile`]) — is pure and tested here.

use std::time::Duration;

use blue2th_proto::DiscoveredBackend;

/// Why a browse could not be run or completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryError {
    /// The JNI preflight failed on this ROM: `WifiManager.MulticastLock` could
    /// not be resolved, so the Search button is disabled and nothing is tried.
    Unsupported,
    /// The mDNS browse itself failed (socket, interface, timeout plumbing).
    Browse(String),
}

impl std::fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DiscoveryError::Unsupported => {
                write!(f, "this device cannot search the network")
            },
            DiscoveryError::Browse(detail) => write!(f, "{detail}"),
        }
    }
}

impl std::error::Error for DiscoveryError {}

/// How long a browse listens before reporting what it found.
pub const BROWSE_TIMEOUT: Duration = Duration::from_secs(5);

/// Whether the Search button is live, derived from the cached preflight verdict.
/// Pure, so the disabled case is testable with no JNI at all.
pub fn search_enabled(multicast_supported: bool) -> bool {
    multicast_supported
}

/// Whether the browse runs, given whether the multicast lock was acquired. Pure.
///
/// A failed lock must **not** fail the scan: some devices do not filter
/// multicast, and in hotspot mode the phone is the access point. Only the browse
/// result decides what the user is told.
pub fn browse_proceeds(lock_acquired: bool) -> bool {
    // Deliberately ignores its input: the lock is an optimisation, not a
    // precondition. Kept as a named function so the rule is pinned by a test
    // rather than living as an implicit `if` inside `browse`.
    let _ = lock_acquired;
    true
}

/// Browse `_blue2th._tcp.local.` for `timeout`, holding the multicast lock for
/// the duration, and return what answered — in the order found.
///
/// Finding nothing is `Ok(Vec::new())`, a neutral state: a guest Wi-Fi with
/// client isolation, a filtered multicast or a backend that is simply down are
/// not errors, and manual entry plus the QR stay the way out.
///
/// The network seam itself is validated by hand on a device.
pub async fn browse(timeout: Duration) -> Result<Vec<DiscoveredBackend>, DiscoveryError> {
    if !crate::jni_util::multicast_supported() {
        return Err(DiscoveryError::Unsupported);
    }
    // Best effort, and held for the whole browse: released when this guard drops.
    // `browse_proceeds` states the rule a failed lock must obey.
    let lock = MulticastGuard::acquire();
    if !browse_proceeds(lock.is_some()) {
        return Ok(Vec::new());
    }

    let daemon =
        mdns_sd::ServiceDaemon::new().map_err(|e| DiscoveryError::Browse(e.to_string()))?;
    let events = daemon
        .browse(blue2th_proto::SERVICE_TYPE)
        .map_err(|e| DiscoveryError::Browse(e.to_string()))?;

    let mut found: Vec<DiscoveredBackend> = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;
    // Listening until the deadline rather than until the first answer: several
    // backends may reply, and the caller lists them all.
    while let Ok(Ok(event)) = tokio::time::timeout_at(deadline, events.recv_async()).await {
        let mdns_sd::ServiceEvent::ServiceResolved(service) = event else {
            continue;
        };
        let Some(candidate) = resolved_to_backend(&service) else {
            continue;
        };
        // The same instance resolves more than once on a busy network; a repeat
        // is the same machine, not a second one.
        if is_new_find(&found, &candidate) {
            found.push(candidate);
        }
    }
    // Best effort: the browse already produced its result, and a daemon that
    // refuses to stop must not turn a successful scan into an error.
    let _ = daemon.shutdown();
    Ok(found)
}

/// Whether `candidate` is a machine the browse has not already listed. Pure.
///
/// A repeat is anything answering at an address already listed, **or** carrying
/// an id already listed: a multi-homed backend resolves once per interface, and
/// listing it twice would show the same machine as two entries and make the scan
/// repair it twice.
pub fn is_new_find(found: &[DiscoveredBackend], candidate: &DiscoveredBackend) -> bool {
    !found
        .iter()
        .any(|f| f.url == candidate.url || (candidate.id.is_some() && f.id == candidate.id))
}

/// Turn a resolved mDNS service into the shared DTO, or `None` when it carries no
/// usable IPv4 address. Address selection is `min` rather than "first" so a
/// multi-homed backend resolves to the same URL on every scan — a `HashSet` has
/// no order, and an unstable URL would look like a move on each browse.
fn resolved_to_backend(service: &mdns_sd::ResolvedService) -> Option<DiscoveredBackend> {
    let addr = service.get_addresses_v4().into_iter().min()?;
    let url = format!("http://{addr}:{}", service.get_port());
    // The DTO is built by proto from the TXT pairs, so the app reads exactly what
    // the server writes.
    let txt: Vec<(&str, &str)> = [blue2th_proto::TXT_KEY_ID, blue2th_proto::TXT_KEY_NAME]
        .into_iter()
        .filter_map(|key| service.get_property_val_str(key).map(|value| (key, value)))
        .collect();
    Some(blue2th_proto::discovered_from_txt(&url, &txt))
}

/// Holds Android's `WifiManager.MulticastLock` for as long as it lives.
///
/// Without it the Wi-Fi driver filters multicast frames to save power and the
/// browse sees nothing. Acquiring it can still fail — and that is not fatal, see
/// [`browse_proceeds`].
#[cfg(target_os = "android")]
struct MulticastGuard(jni::objects::GlobalRef);

#[cfg(target_os = "android")]
impl MulticastGuard {
    /// Acquire the lock, or `None` if anything on the way refused.
    fn acquire() -> Option<Self> {
        use jni::objects::{JObject, JValue};

        let ctx = ndk_context::android_context();
        // SAFETY: `ndk_context` hands back the process-wide `JavaVM` and the
        // current `Activity`, both installed by the Android runtime.
        let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }.ok()?;
        let mut env = crate::jni_util::env(&vm).ok()?;
        let activity = unsafe { JObject::from_raw(ctx.context().cast()) };

        let lock = (|| -> Result<jni::objects::GlobalRef, jni::errors::Error> {
            // The application context, not the activity: a `WifiManager` bound to
            // an activity leaks it, which Android has warned about since N.
            let app_ctx = env
                .call_method(
                    &activity,
                    "getApplicationContext",
                    "()Landroid/content/Context;",
                    &[],
                )?
                .l()?;
            let service = env.new_string("wifi")?;
            let wifi = env
                .call_method(
                    &app_ctx,
                    "getSystemService",
                    "(Ljava/lang/String;)Ljava/lang/Object;",
                    &[JValue::Object(&service)],
                )?
                .l()?;
            if wifi.is_null() {
                return Err(jni::errors::Error::NullPtr("no WifiManager on this device"));
            }
            let tag = env.new_string("blue2th-discovery")?;
            let lock = env
                .call_method(
                    &wifi,
                    "createMulticastLock",
                    "(Ljava/lang/String;)Landroid/net/wifi/WifiManager$MulticastLock;",
                    &[JValue::Object(&tag)],
                )?
                .l()?;
            // A global ref, because the lock must outlive this frame: it is
            // released when the guard drops, at the end of the browse. Taken
            // *before* `acquire`, so a failure on the way out cannot leave the
            // radio filter lifted with no guard left to release it.
            let lock = env.new_global_ref(&lock)?;
            env.call_method(lock.as_obj(), "acquire", "()V", &[])?;
            Ok(lock)
        })();

        match lock {
            Ok(lock) => Some(Self(lock)),
            Err(e) => {
                // Through the shared helper, never a bare `exception_clear`: it
                // captures the Java cause before clearing, and clearing is what
                // keeps the next JNI call from aborting the process. The cause is
                // dropped rather than surfaced on purpose — a refused lock is not
                // a failed scan (see `browse_proceeds`), so there is nothing to
                // tell the user yet.
                let _ = crate::jni_util::err_clear(&mut env, e);
                None
            },
        }
    }
}

#[cfg(target_os = "android")]
impl Drop for MulticastGuard {
    fn drop(&mut self) {
        let ctx = ndk_context::android_context();
        // SAFETY: see `acquire`.
        let Ok(vm) = (unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }) else {
            return;
        };
        let Ok(mut env) = crate::jni_util::env(&vm) else {
            return;
        };
        if let Err(e) = env.call_method(self.0.as_obj(), "release", "()V", &[]) {
            // Releasing an already-released lock throws; the radio filter is back
            // on either way. Cleared through the shared helper so a pending
            // exception cannot abort the next JNI call on this thread.
            let _ = crate::jni_util::err_clear(&mut env, e);
        }
    }
}

/// No multicast filtering to lift off Android: the browse runs as-is.
#[cfg(not(target_os = "android"))]
struct MulticastGuard;

#[cfg(not(target_os = "android"))]
impl MulticastGuard {
    fn acquire() -> Option<Self> {
        Some(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Criterion: the Search button's enabled state is a pure function over the
    // cached preflight verdict — a ROM that cannot resolve `MulticastLock`
    // renders it disabled, and no JNI call is ever attempted.
    #[test]
    fn test_search_enabled_follows_the_preflight_verdict() {
        assert!(search_enabled(true), "a capable ROM keeps the button live");
        assert!(
            !search_enabled(false),
            "a failed preflight disables the button rather than failing on tap"
        );
    }

    // Criterion (non-nominal): the multicast lock could not be acquired — browse
    // anyway. Only the browse result decides what the user is told.
    #[test]
    fn test_browse_proceeds_even_without_the_multicast_lock() {
        assert!(browse_proceeds(true));
        assert!(
            browse_proceeds(false),
            "a failed lock is not a failed scan: hotspot mode has the phone as AP"
        );
    }

    /// A discovered service, for the deduplication cases.
    fn service(id: Option<&str>, url: &str) -> DiscoveredBackend {
        DiscoveredBackend {
            id: id.map(str::to_string),
            name: "blue2th-PC".to_string(),
            url: url.to_string(),
        }
    }

    // Criterion (non-nominal): the same instance resolves more than once on a
    // busy network — a repeat at the same address is the same machine.
    #[test]
    fn test_is_new_find_rejects_a_repeat_at_the_same_address() {
        let found = vec![service(Some("salon-id"), "http://192.168.1.107:4000")];
        assert!(!is_new_find(
            &found,
            &service(Some("salon-id"), "http://192.168.1.107:4000")
        ));
        // Even when the repeat lost its TXT id on the second resolution.
        assert!(!is_new_find(
            &found,
            &service(None, "http://192.168.1.107:4000")
        ));
    }

    // Criterion (non-nominal): a multi-homed backend resolves once per interface,
    // at two different addresses — the id says it is one machine, and listing it
    // twice would have the scan repair the same entry twice.
    #[test]
    fn test_is_new_find_rejects_the_same_id_at_another_address() {
        let found = vec![service(Some("salon-id"), "http://192.168.1.107:4000")];
        assert!(!is_new_find(
            &found,
            &service(Some("salon-id"), "http://10.0.0.5:4000")
        ));
    }

    // Criterion: two genuinely different backends are both listed — including two
    // that advertise no id at all, which then only differ by address.
    #[test]
    fn test_is_new_find_accepts_a_second_backend() {
        let found = vec![service(Some("salon-id"), "http://192.168.1.107:4000")];
        assert!(is_new_find(
            &found,
            &service(Some("bureau-id"), "http://192.168.1.42:4000")
        ));
        assert!(
            is_new_find(&found, &service(None, "http://192.168.1.42:4000")),
            "a service with no id is matched on its address alone"
        );
        let idless = vec![service(None, "http://192.168.1.107:4000")];
        assert!(
            is_new_find(&idless, &service(None, "http://192.168.1.42:4000")),
            "two id-less backends are told apart by their addresses"
        );
        assert!(is_new_find(&[], &service(None, "http://192.168.1.42:4000")));
    }

    // Criterion: the scan is bounded (~5 s) so it cannot hold the settings page.
    #[test]
    fn test_browse_timeout_is_bounded_and_short() {
        assert!(
            BROWSE_TIMEOUT >= Duration::from_secs(1) && BROWSE_TIMEOUT <= Duration::from_secs(10),
            "the browse must be bounded and short, got {BROWSE_TIMEOUT:?}"
        );
    }

    // Criterion (non-nominal): "no backend found" is a neutral state, never an
    // error — the error type has no variant for it, so an empty scan can only be
    // reported as `Ok(vec![])`.
    #[test]
    fn test_discovery_error_has_no_variant_for_an_empty_scan() {
        let unsupported = DiscoveryError::Unsupported.to_string();
        assert!(
            !unsupported.is_empty(),
            "the disabled case must explain itself in the error card"
        );
        assert_eq!(
            DiscoveryError::Browse("socket bind failed".to_string()).to_string(),
            "socket bind failed",
            "a browse failure must surface its own detail"
        );
    }
}
