// SPDX-License-Identifier: MIT OR Apache-2.0

//! The `dx serve --platform web` dev proxy (#160), checked against the routes
//! the backend actually serves.
//!
//! In the browser every API call goes to the page's own origin, and `dx`
//! relays it to the backend through one `[[web.proxy]]` entry per route
//! prefix. A route added to `ROUTES` without its proxy entry works from the
//! phone and fails in the browser with a 404 from `dx` — nothing else would
//! notice, since `cargo test` never runs `dx`. The table lives in this crate,
//! so the check does too.

use std::collections::BTreeSet;
use std::path::Path;

use blue2th_server::ROUTES;

/// Where `dx serve` relays the API: the backend's default port on loopback.
const PROXY_BACKEND: &str = "http://127.0.0.1:4000";

/// The `backend` of every `[[web.proxy]]` entry in the frontend's
/// `Dioxus.toml`, in file order. Empty when the file, the table or the field
/// is missing; the assertions below then fail rather than `panic!`.
fn proxy_backends() -> Vec<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../blue2th-frontend/Dioxus.toml");
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(!raw.is_empty(), "{} must be readable", path.display());
    let parsed = raw.parse::<toml::Table>();
    assert!(parsed.is_ok(), "Dioxus.toml must parse: {parsed:?}");
    parsed
        .unwrap_or_default()
        .get("web")
        .and_then(|web| web.get("proxy"))
        .and_then(|proxy| proxy.as_array())
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.get("backend").and_then(|b| b.as_str()))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// The top-level prefix of every route: `/devices/{addr}/connect` → `devices`.
fn route_prefixes() -> BTreeSet<String> {
    ROUTES
        .iter()
        .filter_map(|route| route.path.trim_start_matches('/').split('/').next())
        .map(str::to_owned)
        .collect()
}

// Criterion: `Dioxus.toml` holds one `[[web.proxy]]` entry per top-level
// prefix of `ROUTES`, each with backend `http://127.0.0.1:4000/<prefix>`.
// Compared as sets both ways, so a missing prefix and a stray entry both fail.
#[test]
fn test_dioxus_proxy_covers_every_route_prefix() {
    let expected: BTreeSet<String> = route_prefixes()
        .iter()
        .map(|prefix| format!("{PROXY_BACKEND}/{prefix}"))
        .collect();
    let actual: BTreeSet<String> = proxy_backends().into_iter().collect();

    assert!(!expected.is_empty(), "ROUTES must yield prefixes to proxy");
    assert_eq!(
        actual, expected,
        "one [[web.proxy]] per ROUTES prefix, at {PROXY_BACKEND}/<prefix>"
    );
}

// Criterion: exactly **one** entry per prefix. Near-miss: a duplicated entry,
// which the set comparison above folds away.
#[test]
fn test_dioxus_proxy_lists_each_prefix_once() {
    let backends = proxy_backends();
    let unique: BTreeSet<&String> = backends.iter().collect();

    assert!(
        !backends.is_empty(),
        "Dioxus.toml must declare [[web.proxy]]"
    );
    assert_eq!(
        unique.len(),
        backends.len(),
        "a prefix proxied twice: {backends:?}"
    );
}

// Criterion (empty value): no entry for `/`. A proxy URL needs a non-empty
// path (observed in `dx` 0.7.10, #160), and a catch-all would also swallow the
// page's own assets. Near-miss: `http://127.0.0.1:4000/` and the bare origin,
// both of which a prefix match would read as "everything".
#[test]
fn test_dioxus_proxy_has_no_entry_for_the_root() {
    let backends = proxy_backends();

    assert!(
        !backends.is_empty(),
        "Dioxus.toml must declare [[web.proxy]]"
    );
    for backend in &backends {
        let path = backend.strip_prefix(PROXY_BACKEND).unwrap_or(backend);
        assert!(
            !path.trim_matches('/').is_empty(),
            "{backend:?} proxies the root; every entry needs a route prefix"
        );
    }
}
