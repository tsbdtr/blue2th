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
    let _ = multicast_supported;
    todo!("phase 6.6: derive the button state from the cached verdict")
}

/// Whether the browse runs, given whether the multicast lock was acquired. Pure.
///
/// A failed lock must **not** fail the scan: some devices do not filter
/// multicast, and in hotspot mode the phone is the access point. Only the browse
/// result decides what the user is told.
pub fn browse_proceeds(lock_acquired: bool) -> bool {
    let _ = lock_acquired;
    todo!("phase 6.6: a failed multicast lock never stops the browse")
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
    let _ = timeout;
    todo!("phase 6.6: acquire the lock, browse with mdns-sd, release the lock")
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
