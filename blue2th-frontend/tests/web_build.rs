// SPDX-License-Identifier: MIT OR Apache-2.0

//! The browser build (#159), pinned on the files that make it possible.
//!
//! `cargo test` runs on the host and never compiles the wasm target, so nothing
//! in it would notice `tokio` creeping back into the browser tree, the `web`
//! feature disappearing, or the CI job that does build for the browser being
//! dropped. These assert on the manifest, the CI workflow and the shared
//! sources instead. They cannot replace the wasm build itself — that is what the
//! `web` CI job is for.

use std::path::Path;

/// The target-table key that keeps a dependency off the browser build. Compared
/// with whitespace removed, so the spacing of the key is free.
const NATIVE_ONLY: &str = "cfg(not(target_arch=\"wasm32\"))";

/// The target-table key that keeps a dependency on the browser build only.
const WASM_ONLY: &str = "cfg(target_arch=\"wasm32\")";

/// Reads a file relative to the crate manifest. An unreadable file yields an
/// empty string and fails the assertion here, rather than `panic!`.
fn read(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        !content.is_empty(),
        "{} must exist and be readable",
        path.display()
    );
    content
}

/// The crate manifest, parsed. A parse error fails the assertion here rather
/// than `expect`: clippy's test allowance does not reach a free function.
fn manifest() -> toml::Table {
    let parsed = read("Cargo.toml").parse::<toml::Table>();
    assert!(
        parsed.is_ok(),
        "blue2th-frontend/Cargo.toml must parse: {parsed:?}"
    );
    parsed.unwrap_or_default()
}

/// The `[target.<key>.dependencies]` table whose key equals `key` once
/// whitespace is removed.
fn target_dependencies(manifest: &toml::Table, key: &str) -> Option<toml::Table> {
    let targets = manifest.get("target")?.as_table()?;
    targets
        .iter()
        .find(|(k, _)| k.chars().filter(|c| !c.is_whitespace()).collect::<String>() == key)
        .and_then(|(_, v)| v.get("dependencies"))
        .and_then(|d| d.as_table())
        .cloned()
}

/// The string array at `value[field]`, empty when absent.
fn strings(value: Option<&toml::Value>, field: &str) -> Vec<String> {
    value
        .and_then(|v| v.get(field))
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn feature(manifest: &toml::Table, name: &str) -> Option<Vec<String>> {
    manifest
        .get("features")
        .and_then(|f| f.get(name))
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(str::to_owned))
                .collect()
        })
}

// ── Cargo.toml ───────────────────────────────────────────────────────────────

// Criterion: `dioxus = { features = ["router"] }` — no platform feature on the
// dependency line, so `--no-default-features --features web` builds no mobile
// renderer.
#[test]
fn test_dioxus_dependency_names_no_platform_feature() {
    let manifest = manifest();
    let dioxus = manifest.get("dependencies").and_then(|d| d.get("dioxus"));

    assert!(dioxus.is_some(), "dioxus must stay a dependency");
    assert_eq!(
        strings(dioxus, "features"),
        vec!["router".to_owned()],
        "the dioxus line carries `router` only; the platform comes from a crate feature"
    );
}

// Criterion: `mobile = ["dioxus/mobile"]` stays the default, so the Android
// build and every host command keep their meaning.
#[test]
fn test_mobile_stays_the_default_feature() {
    let manifest = manifest();

    assert_eq!(
        feature(&manifest, "default"),
        Some(vec!["mobile".to_owned()])
    );
    assert_eq!(
        feature(&manifest, "mobile"),
        Some(vec!["dioxus/mobile".to_owned()])
    );
}

// Criterion: a new `web = ["dioxus/web"]` feature.
#[test]
fn test_web_feature_enables_the_dioxus_web_renderer() {
    let manifest = manifest();

    assert_eq!(
        feature(&manifest, "web"),
        Some(vec!["dioxus/web".to_owned()]),
        "`web` must enable dioxus/web, and nothing else"
    );
}

// Criterion: `tokio` and `mdns-sd` are dependencies of
// `cfg(not(target_arch = "wasm32"))` only. Near-miss: a gate on
// `target_os = "android"` would also keep them off wasm, but would take them
// off the host build, where clippy and the tests check the native code — only
// the exact `not(wasm32)` key passes.
#[test]
fn test_tokio_and_mdns_sd_are_native_only_dependencies() {
    let manifest = manifest();
    let shared = manifest
        .get("dependencies")
        .and_then(|d| d.as_table())
        .cloned()
        .unwrap_or_default();
    let native = target_dependencies(&manifest, NATIVE_ONLY);

    for name in ["tokio", "mdns-sd"] {
        assert!(
            !shared.contains_key(name),
            "{name} must not be a dependency of every target: it does not build on wasm"
        );
        assert!(
            native.as_ref().is_some_and(|t| t.contains_key(name)),
            "{name} must be a dependency of [target.'{NATIVE_ONLY}'.dependencies]"
        );
    }
}

// Criterion: no behaviour change on Android — the native tokio keeps the
// features the app runs on (`time` for the timers, `rt` for the runtime handle
// `lifecycle` captures).
#[test]
fn test_native_tokio_keeps_its_time_and_rt_features() {
    let manifest = manifest();
    let native = target_dependencies(&manifest, NATIVE_ONLY);
    let tokio = native.as_ref().and_then(|t| t.get("tokio"));
    let features = strings(tokio, "features");

    for wanted in ["time", "rt"] {
        assert!(
            features.iter().any(|f| f == wanted),
            "the native tokio must keep feature `{wanted}`, got {features:?}"
        );
    }
}

// Criterion: `gloo-timers` (0.3, feature `futures`) is a dependency of
// `cfg(target_arch = "wasm32")` only. Near-miss: gloo-timers in the shared
// `[dependencies]` compiles on the host too, and would then be one import away
// from replacing tokio's timer on Android.
#[test]
fn test_gloo_timers_is_a_wasm_only_dependency_with_futures() {
    let manifest = manifest();
    let shared = manifest
        .get("dependencies")
        .and_then(|d| d.as_table())
        .cloned()
        .unwrap_or_default();
    let native = target_dependencies(&manifest, NATIVE_ONLY).unwrap_or_default();
    let wasm = target_dependencies(&manifest, WASM_ONLY);
    let gloo = wasm.as_ref().and_then(|t| t.get("gloo-timers"));

    assert!(
        !shared.contains_key("gloo-timers"),
        "gloo-timers is wasm-only"
    );
    assert!(
        !native.contains_key("gloo-timers"),
        "gloo-timers is wasm-only"
    );
    assert!(
        gloo.is_some(),
        "gloo-timers must be a dependency of [target.'{WASM_ONLY}'.dependencies]"
    );
    assert_eq!(
        gloo.and_then(|g| g.get("version")).and_then(|v| v.as_str()),
        Some("0.3")
    );
    assert!(
        strings(gloo, "features").iter().any(|f| f == "futures"),
        "gloo-timers needs its `futures` feature for `gloo_timers::future::sleep`"
    );
}

// ── Shared sources ───────────────────────────────────────────────────────────

/// The part of a source file before its `#[cfg(test)]` module: the code that
/// ships.
fn shipped_part(relative: &str) -> String {
    let source = read(relative);
    let end = source.find("#[cfg(test)]").unwrap_or(source.len());
    source.get(..end).unwrap_or_default().to_owned()
}

// Criterion: every non-test `tokio::time` use in shared code goes through the
// helper — the eleven sleeps in `main.rs` and the `scan` timeout in
// `backend.rs`. Near-miss: a `use tokio::time::sleep;` that leaves the call
// sites reading `sleep(...)` — caught by the same `tokio::time` match.
#[test]
fn test_shared_code_names_no_tokio_timer() {
    for file in ["src/main.rs", "src/backend.rs"] {
        let shipped = shipped_part(file);
        let offending: Vec<(usize, &str)> = shipped
            .lines()
            .enumerate()
            .filter(|(_, line)| !line.trim_start().starts_with("//"))
            .filter(|(_, line)| line.contains("tokio::time"))
            .map(|(i, line)| (i + 1, line.trim()))
            .collect();

        assert!(
            offending.is_empty(),
            "{file} must go through the timer helper, not tokio::time: {offending:?}"
        );
    }
}

/// The argument list of the first `name(` call in `source`, whitespace
/// removed, up to its matching parenthesis. Empty when there is no such call.
fn call_arguments(source: &str, name: &str) -> String {
    let Some(start) = source.find(name).map(|at| at + name.len()) else {
        return String::new();
    };
    let mut depth = 1usize;
    let mut arguments = String::new();
    for c in source.get(start..).unwrap_or_default().chars() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {},
        }
        if depth == 0 {
            break;
        }
        if !c.is_whitespace() {
            arguments.push(c);
        }
    }
    arguments
}

// Criterion: `browse_available` is `cfg!(not(target_arch = "wasm32"))` at the
// call site. `cfg!` is true on every target `cargo test` builds for, so no
// runtime test sees a call site that passes `true` and leaves Search live in
// the browser — only the source does. Near-miss: `search_enabled(multicast,
// true)`, which compiles everywhere and passes every other test.
#[test]
fn test_settings_page_derives_browse_availability_from_the_target() {
    let shipped = shipped_part("src/main.rs");
    let arguments = call_arguments(&shipped, "discovery::search_enabled(");

    assert!(
        !arguments.is_empty(),
        "main.rs must call discovery::search_enabled"
    );
    assert!(
        arguments.ends_with(",cfg!(not(target_arch=\"wasm32\")),")
            || arguments.ends_with(",cfg!(not(target_arch=\"wasm32\"))"),
        "Search must be live only where the target can browse, got ({arguments})"
    );
}

// ── #160: the browser glue ───────────────────────────────────────────────────

// Criterion: `web-sys` is a wasm-only direct dependency, with `Storage` for the
// `localStorage` seam. Near-miss: `web-sys` under `[dependencies]`, which
// builds for the browser just as well but drags it into the Android build.
#[test]
fn test_web_sys_is_a_wasm_only_dependency_with_storage() {
    let manifest = manifest();
    let shared = manifest
        .get("dependencies")
        .and_then(|d| d.as_table())
        .cloned()
        .unwrap_or_default();
    let native = target_dependencies(&manifest, NATIVE_ONLY).unwrap_or_default();
    let wasm = target_dependencies(&manifest, WASM_ONLY);
    let web_sys = wasm.as_ref().and_then(|t| t.get("web-sys"));

    assert!(!shared.contains_key("web-sys"), "web-sys is wasm-only");
    assert!(!native.contains_key("web-sys"), "web-sys is wasm-only");
    assert!(
        web_sys.is_some(),
        "web-sys must be a dependency of [target.'{WASM_ONLY}'.dependencies]"
    );
    assert!(
        strings(web_sys, "features").iter().any(|f| f == "Storage"),
        "the settings persist to localStorage, which needs web-sys's `Storage`"
    );
}

/// The non-comment lines of the shipped part of `relative`, joined.
fn shipped_code(relative: &str) -> String {
    shipped_part(relative)
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

// Criterion: every client syncs its backend's config through the one rule —
// push only what is pending, otherwise read (#160) — and an unpaired browser
// opens on `/settings`. Both are tested at runtime (`backend::sync_config`,
// `tests/browser.rs`); what no runtime test sees is whether the app asks them.
// Near-miss: the rule defined and green while the health loop still calls
// `push_active_name` and the router always opens on `/`.
#[test]
fn test_app_consults_the_config_sync_and_start_page_policies() {
    let shipped = shipped_code("src/main.rs");

    for call in ["backend::sync_config(", "start_page("] {
        assert!(
            shipped.contains(call),
            "main.rs must decide through {call}…)"
        );
    }
    assert!(
        !shipped.contains("push_active_name("),
        "no client re-pushes its stored config on reconnection any more"
    );
}

// ── CI ───────────────────────────────────────────────────────────────────────

/// The `web` job of `.github/workflows/ci.yml`, comment lines dropped and
/// shell line continuations joined, one logical line per entry. Empty when the
/// job does not exist.
fn web_job() -> Vec<String> {
    let workflow = read("../.github/workflows/ci.yml");
    let mut in_jobs = false;
    let mut in_web = false;
    let mut lines = Vec::new();
    for line in workflow.lines() {
        if line.trim_start().starts_with('#') {
            continue;
        }
        let top_level = !line.is_empty() && !line.starts_with(' ');
        if top_level {
            in_jobs = line.starts_with("jobs:");
            in_web = false;
            continue;
        }
        let job_key = line.starts_with("  ") && !line.starts_with("   ") && line.ends_with(':');
        if in_jobs && job_key {
            in_web = line.trim() == "web:";
            continue;
        }
        if in_web {
            lines.push(line.to_owned());
        }
    }
    lines
        .join("\n")
        .replace("\\\n", " ")
        .lines()
        .map(str::to_owned)
        .collect()
}

fn position(job: &[String], predicate: impl Fn(&str) -> bool) -> Option<usize> {
    job.iter().position(|line| predicate(line))
}

// Criterion: `.github/workflows/ci.yml` has a `web` job on the pinned
// toolchain (1.98.1) with the wasm target and clippy.
#[test]
fn test_ci_web_job_uses_the_pinned_toolchain_with_the_wasm_target() {
    let job = web_job();

    assert!(!job.is_empty(), "ci.yml must have a `web` job");
    assert!(
        position(&job, |l| l.contains("dtolnay/rust-toolchain")).is_some(),
        "the web job installs its toolchain with dtolnay/rust-toolchain"
    );
    assert!(
        position(&job, |l| l.trim() == "toolchain: 1.98.1").is_some(),
        "the web job runs the toolchain the rest of CI is pinned to"
    );
    assert!(
        position(&job, |l| l.trim_start().starts_with("targets:")
            && l.contains("wasm32-unknown-unknown"))
        .is_some(),
        "the web job installs the wasm32-unknown-unknown target"
    );
    assert!(
        position(&job, |l| l.trim_start().starts_with("components:")
            && l.contains("clippy"))
        .is_some(),
        "the web job installs clippy"
    );
}

// Criterion: the job installs `dioxus-cli@0.7.10` through `cargo-binstall`, as
// `release.yml` does.
#[test]
fn test_ci_web_job_installs_the_pinned_dioxus_cli() {
    let job = web_job();

    assert!(
        position(&job, |l| l.contains("cargo-bins/cargo-binstall@")).is_some(),
        "the web job installs cargo-binstall"
    );
    assert!(
        position(&job, |l| l.contains("cargo binstall")
            && l.contains("dioxus-cli@0.7.10"))
        .is_some(),
        "the web job installs dioxus-cli 0.7.10 with cargo binstall"
    );
}

// Criterion: the job runs the wasm clippy — `-p blue2th-frontend --target
// wasm32-unknown-unknown --no-default-features --features web` with
// `-D warnings` — then `dx build --platform web`.
#[test]
fn test_ci_web_job_runs_the_wasm_clippy_then_the_web_build() {
    let job = web_job();
    let clippy = position(&job, |l| {
        l.contains("cargo clippy")
            && l.contains("-p blue2th-frontend")
            && l.contains("--target wasm32-unknown-unknown")
            && l.contains("--no-default-features")
            && l.contains("--features web")
            && l.contains("-D warnings")
    });
    let build = position(&job, |l| {
        l.contains("dx build")
            && l.contains("--platform web")
            && l.contains("--package blue2th-frontend")
    });

    assert!(
        clippy.is_some(),
        "the web job runs the wasm clippy with -D warnings"
    );
    assert!(
        build.is_some(),
        "the web job runs dx build --platform web --package blue2th-frontend"
    );
    assert!(
        clippy < build,
        "the wasm clippy runs before the web build, which is the slower of the two"
    );
}

// Criterion: the job holds to the workflow's top-level `permissions`.
// Near-miss: a job-level `permissions:` block — even `contents: write` — keeps
// `scripts/check-workflow-permissions.sh` green, so only this test sees it.
#[test]
fn test_ci_web_job_declares_no_permissions_of_its_own() {
    let job = web_job();

    assert!(!job.is_empty(), "ci.yml must have a `web` job");
    assert!(
        position(&job, |l| l.starts_with("    permissions:")).is_none(),
        "the web job must inherit the top-level `contents: read`, not widen it"
    );
}
