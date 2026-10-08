// SPDX-License-Identifier: MIT OR Apache-2.0

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
