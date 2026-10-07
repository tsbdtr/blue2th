// SPDX-License-Identifier: MIT OR Apache-2.0

//! The browser build (#159), pinned on the files that make it possible.
//!
//! `cargo test` runs on the host and never compiles the wasm target, so nothing
//! in it would notice `tokio` creeping back into the browser tree, the `web`
//! feature disappearing, or the CI job that does build for the browser being
//! dropped. These assert on the manifest, the CI workflow and the shared
//! sources instead. They cannot replace the wasm build itself — that is what the
//! `web` CI job is for.
//!
//! Since #175 they also pin where the renderer comes from: the Android target,
//! never a default feature, so no host build compiles the desktop renderer.

use std::path::Path;

/// The target-table key that keeps a dependency off the browser build. Compared
/// with whitespace removed, so the spacing of the key is free.
const NATIVE_ONLY: &str = "cfg(not(target_arch=\"wasm32\"))";

/// The target-table key that keeps a dependency on the browser build only.
const WASM_ONLY: &str = "cfg(target_arch=\"wasm32\")";

/// The target-table key that keeps a dependency on the Android build only.
const ANDROID_ONLY: &str = "cfg(target_os=\"android\")";

/// The dioxus features that select a renderer.
const RENDERERS: [&str; 3] = ["mobile", "desktop", "web"];

/// The dependency tables of a manifest, or of one of its `[target.<key>]`.
const DEPENDENCY_KINDS: [&str; 3] = ["dependencies", "dev-dependencies", "build-dependencies"];

/// The crates only the desktop renderer brings into a host build (#175).
const HOST_FORBIDDEN: [&str; 9] = [
    "wry",
    "dioxus-desktop",
    "webkit2gtk",
    "gtk",
    "soup3",
    "javascriptcore-rs",
    "muda",
    "libxdo",
    "openssl-sys",
];

/// The native packages the server build needs, and all CI installs (#175):
/// D-Bus for bluer, PipeWire for pipewire-rs, clang and libclang for its
/// bindgen step.
const SERVER_PACKAGES: [&str; 5] = [
    "pkg-config",
    "libdbus-1-dev",
    "libpipewire-0.3-dev",
    "libclang-dev",
    "clang",
];

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
    parse("Cargo.toml")
}

/// The TOML file at `relative` to the crate manifest, parsed.
fn parse(relative: &str) -> toml::Table {
    let parsed = read(relative).parse::<toml::Table>();
    assert!(parsed.is_ok(), "{relative} must parse: {parsed:?}");
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

// ── #175: the renderer is a property of the target ──────────────────────────
//
// These replace #159's `test_mobile_stays_the_default_feature`. Its guarantee,
// "Android gets its renderer", no longer comes from a default feature: the
// Android-only dioxus dependency carries `mobile`, and no host build sees it.

/// The renderer a `[features]` entry enables directly — `dioxus/mobile`,
/// `dioxus?/web` — or `None`. Compared on whole names, never on a substring:
/// a feature called `web-storage` enables no renderer.
fn renderer_of(entry: &str) -> Option<&str> {
    let (dependency, feature) = entry.split_once('/')?;
    (dependency.trim_end_matches('?') == "dioxus" && RENDERERS.contains(&feature))
        .then_some(feature)
}

/// Every renderer `default` enables, walking `[features]` through every local
/// feature it names, as `"<feature> -> dioxus/<renderer>"`. Empty for an empty
/// or absent `default`: there, the empty list is the intended value.
fn default_renderers(manifest: &toml::Table) -> Vec<String> {
    let mut pending = vec!["default".to_owned()];
    let mut seen: Vec<String> = Vec::new();
    let mut found = Vec::new();
    while let Some(name) = pending.pop() {
        if seen.contains(&name) {
            continue;
        }
        for entry in feature(manifest, &name).unwrap_or_default() {
            if let Some(renderer) = renderer_of(&entry) {
                found.push(format!("{name} -> dioxus/{renderer}"));
            } else if feature(manifest, &entry).is_some() {
                pending.push(entry);
            }
        }
        seen.push(name);
    }
    found
}

/// Every dependency table of `manifest` — shared, dev and build, then each
/// `[target.<key>]`'s — named by its path, with the target key's whitespace
/// removed.
fn dependency_tables(manifest: &toml::Table) -> Vec<(String, toml::Table)> {
    let mut tables = Vec::new();
    for kind in DEPENDENCY_KINDS {
        if let Some(table) = manifest.get(kind).and_then(|t| t.as_table()) {
            tables.push((kind.to_owned(), table.clone()));
        }
    }
    let targets = manifest.get("target").and_then(|t| t.as_table());
    for (key, target) in targets.into_iter().flatten() {
        let key: String = key.chars().filter(|c| !c.is_whitespace()).collect();
        for kind in DEPENDENCY_KINDS {
            if let Some(table) = target.get(kind).and_then(|t| t.as_table()) {
                tables.push((format!("target.{key}.{kind}"), table.clone()));
            }
        }
    }
    tables
}

/// Every dioxus entry of `manifest`, renamed ones included, with the path of
/// the table that holds it.
fn dioxus_entries(manifest: &toml::Table) -> Vec<(String, toml::Value)> {
    dependency_tables(manifest)
        .into_iter()
        .flat_map(|(site, table)| {
            table
                .into_iter()
                .filter(|(name, dependency)| {
                    name == "dioxus"
                        || dependency.get("package").and_then(|p| p.as_str()) == Some("dioxus")
                })
                .map(move |(_, dependency)| (site.clone(), dependency))
        })
        .collect()
}

/// Every place in `manifest` that enables `dioxus/mobile`: a dependency table
/// whose dioxus entry lists `mobile`, and a `[features]` entry that enables it
/// — or that is itself called `mobile`, whatever it enables.
fn mobile_renderer_sites(manifest: &toml::Table) -> Vec<String> {
    let mut sites: Vec<String> = dioxus_entries(manifest)
        .into_iter()
        .filter(|(_, dependency)| {
            strings(Some(dependency), "features")
                .iter()
                .any(|f| f == "mobile")
        })
        .map(|(site, _)| site)
        .collect();
    let features = manifest.get("features").and_then(|f| f.as_table());
    for name in features.into_iter().flat_map(|f| f.keys()) {
        let enables = feature(manifest, name)
            .unwrap_or_default()
            .iter()
            .any(|entry| renderer_of(entry) == Some("mobile"));
        if name == "mobile" || enables {
            sites.push(format!("features.{name}"));
        }
    }
    sites
}

/// The path `mobile_renderer_sites` gives the one table allowed to enable the
/// mobile renderer.
fn android_site() -> String {
    format!("target.{ANDROID_ONLY}.dependencies")
}

// Criterion: `[target.'cfg(target_os = "android")'.dependencies]` declares
// `dioxus` with features `["mobile"]` — exactly, so the Android build gets its
// renderer and nothing more.
#[test]
fn test_android_target_dependency_enables_the_mobile_renderer() {
    let manifest = manifest();
    let android = target_dependencies(&manifest, ANDROID_ONLY);
    let dioxus = android.as_ref().and_then(|t| t.get("dioxus"));

    assert!(
        dioxus.is_some(),
        "dioxus must be a dependency of [target.'{ANDROID_ONLY}'.dependencies]"
    );
    assert_eq!(
        strings(dioxus, "features"),
        vec!["mobile".to_owned()],
        "the Android dioxus entry carries the mobile renderer, and only it"
    );
}

// Criterion: `[features]` keeps no default: `default` is empty or absent. The
// strictest reading of "empty (or absent)": a default carrying a non-renderer
// feature fails here too, though the transitive walk below would accept it.
#[test]
fn test_default_feature_set_is_empty() {
    let manifest = manifest();

    assert_eq!(
        feature(&manifest, "default").unwrap_or_default(),
        Vec::<String>::new(),
        "`default` must be empty or absent: the renderer comes from the target"
    );
}

// Criterion: no feature listed in `default`, directly or through another
// feature, enables `dioxus/mobile`, `dioxus/desktop` or `dioxus/web`. The
// checker's own near-misses are pinned in
// `test_default_renderer_walk_refuses_indirect_and_non_mobile_renderers`.
#[test]
fn test_no_default_feature_enables_a_renderer() {
    let manifest = manifest();

    assert_eq!(
        default_renderers(&manifest),
        Vec::<String>::new(),
        "a renderer in `default` comes back into every host build"
    );
}

// Criterion: the `[features]` table keeps `web = ["dioxus/web"]` and loses
// `mobile`; the only dependency entry that enables `dioxus/mobile` sits under
// exactly `cfg(target_os = "android")`. The checker's own near-misses are
// pinned in `test_mobile_site_check_accepts_only_the_exact_android_key`.
#[test]
fn test_only_the_android_target_enables_the_mobile_renderer() {
    let manifest = manifest();

    assert_eq!(
        mobile_renderer_sites(&manifest),
        vec![android_site()],
        "only [target.'{ANDROID_ONLY}'.dependencies] may enable dioxus/mobile, \
         and no `mobile` feature may remain"
    );
}

// Criterion: the dioxus version is declared once, in the workspace
// `[workspace.dependencies]` (`dioxus = "0.7.10"`, no features there), and both
// frontend entries are `workspace = true` and add only their features.
// Near-miss: an Android entry with its own `version = "0.7.10"`, which builds
// identically today and drifts on the next bump.
#[test]
fn test_dioxus_version_is_declared_once_in_the_workspace() {
    let workspace = parse("../Cargo.toml");
    let declared = workspace
        .get("workspace")
        .and_then(|w| w.get("dependencies"))
        .and_then(|d| d.get("dioxus"));
    let version = declared.and_then(|d| {
        d.as_str()
            .or_else(|| d.get("version").and_then(|v| v.as_str()))
    });
    let extra: Vec<String> = declared
        .and_then(|d| d.as_table())
        .map(|t| t.keys().filter(|k| *k != "version").cloned().collect())
        .unwrap_or_default();

    assert_eq!(
        version,
        Some("0.7.10"),
        "[workspace.dependencies] must declare dioxus 0.7.10, the version dx is pinned to"
    );
    assert!(
        extra.is_empty(),
        "the workspace dioxus carries its version only; each frontend entry adds its features, got {extra:?}"
    );

    let manifest = manifest();
    let entries = dioxus_entries(&manifest);
    let mut sites: Vec<String> = entries.iter().map(|(site, _)| site.clone()).collect();
    sites.sort();
    let mut expected = vec!["dependencies".to_owned(), android_site()];
    expected.sort();
    assert_eq!(
        sites, expected,
        "the frontend declares dioxus twice: shared (router) and Android (mobile)"
    );

    for (site, dependency) in &entries {
        assert_eq!(
            dependency.get("workspace").and_then(|w| w.as_bool()),
            Some(true),
            "the dioxus entry of [{site}] must be `workspace = true`"
        );
        let keys: Vec<&String> = dependency
            .as_table()
            .map(|t| t.keys().collect())
            .unwrap_or_default();
        assert!(
            keys.iter().all(|k| *k == "workspace" || *k == "features"),
            "the dioxus entry of [{site}] adds only its features to the workspace one, got keys {keys:?}"
        );
    }
}

// Pins the checker of `test_no_default_feature_enables_a_renderer` on the
// spec's guards, since the real manifest exercises none of them once green.
// Near-misses: `default = ["app"]` with `app = ["dioxus/mobile"]` (a check of
// `default`'s direct entries accepts it); `default = ["web"]` (a check for
// `mobile` alone accepts it). Accepted: an empty or absent `default`, and a
// default naming `web-storage`, which a substring match on `web` would refuse.
#[test]
fn test_default_renderer_walk_refuses_indirect_and_non_mobile_renderers() {
    let refused = [
        "[features]\ndefault = [\"app\"]\napp = [\"dioxus/mobile\"]\n",
        "[features]\ndefault = [\"web\"]\nweb = [\"dioxus/web\"]\n",
        "[features]\ndefault = [\"a\"]\na = [\"b\"]\nb = [\"dioxus?/desktop\"]\n",
        "[features]\ndefault = [\"a\"]\na = [\"a\", \"dioxus/mobile\"]\n",
    ];
    for fixture in refused {
        let manifest = fixture.parse::<toml::Table>().unwrap_or_default();
        assert!(
            !default_renderers(&manifest).is_empty(),
            "a renderer reached from `default` must be refused:\n{fixture}"
        );
    }

    let accepted = [
        "[features]\ndefault = []\nweb = [\"dioxus/web\"]\n",
        "[features]\nweb = [\"dioxus/web\"]\n",
        "[package]\nname = \"x\"\n",
        "[features]\ndefault = [\"web-storage\"]\nweb-storage = [\"dep:web-sys\"]\nweb = [\"dioxus/web\"]\n",
    ];
    for fixture in accepted {
        let manifest = fixture.parse::<toml::Table>().unwrap_or_default();
        assert_eq!(
            default_renderers(&manifest),
            Vec::<String>::new(),
            "no renderer is reachable from `default` here:\n{fixture}"
        );
    }
}

// Pins the checker of `test_only_the_android_target_enables_the_mobile_renderer`
// on the spec's "Android exactly" guard. Near-misses: `mobile` under
// `cfg(not(target_arch = "wasm32"))`, `cfg(unix)` or `cfg(target_os = "linux")`
// — each still builds Android correctly, and each puts the renderer back into
// the host build. Only the exact Android key passes, whatever its spacing.
#[test]
fn test_mobile_site_check_accepts_only_the_exact_android_key() {
    let dioxus = "dioxus = { workspace = true, features = [\"mobile\"] }";
    let refused = [
        "cfg(not(target_arch = \"wasm32\"))",
        "cfg(unix)",
        "cfg(target_os = \"linux\")",
        "cfg(any(target_os = \"android\", unix))",
    ];
    for key in refused {
        let fixture = format!("[target.'{key}'.dependencies]\n{dioxus}\n");
        let manifest = fixture.parse::<toml::Table>().unwrap_or_default();
        assert_ne!(
            mobile_renderer_sites(&manifest),
            vec![android_site()],
            "mobile under a key that includes the host must be refused:\n{fixture}"
        );
    }

    let renamed = format!(
        "[target.'cfg(unix)'.dependencies]\ndx = {{ package = \"dioxus\", features = [\"mobile\"] }}\n\
         [target.'cfg(target_os = \"android\")'.dependencies]\n{dioxus}\n"
    );
    let feature_too = format!(
        "[target.'cfg(target_os = \"android\")'.dependencies]\n{dioxus}\n\
         [features]\nmobile = []\n"
    );
    for fixture in [renamed, feature_too] {
        let manifest = fixture.parse::<toml::Table>().unwrap_or_default();
        assert_ne!(
            mobile_renderer_sites(&manifest),
            vec![android_site()],
            "a second site enabling mobile must be refused:\n{fixture}"
        );
    }

    for key in ["cfg(target_os = \"android\")", "cfg(target_os=\"android\")"] {
        let fixture = format!("[target.'{key}'.dependencies]\n{dioxus}\n");
        let manifest = fixture.parse::<toml::Table>().unwrap_or_default();
        assert_eq!(
            mobile_renderer_sites(&manifest),
            vec![android_site()],
            "the exact Android key is the one accepted site:\n{fixture}"
        );
    }
}

/// The crate names of `cargo tree -p blue2th-frontend -e normal` for the host,
/// offline. A command that fails, or prints nothing, fails the assertion here
/// rather than reading as "no renderer".
fn host_tree_crates() -> Vec<String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let output = std::process::Command::new(cargo)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args([
            "tree",
            "--offline",
            "-p",
            "blue2th-frontend",
            "-e",
            "normal",
            "--prefix",
            "none",
            "--format",
            "{p}",
        ])
        .output();
    assert!(
        output.as_ref().is_ok_and(|o| o.status.success()),
        "cargo tree must run offline on the host tree: {output:?}"
    );
    let stdout = output.map(|o| o.stdout).unwrap_or_default();
    String::from_utf8_lossy(&stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().next().map(str::to_owned))
        .collect()
}

// Criterion: the host tree contains none of `wry`, `dioxus-desktop`,
// `webkit2gtk`, `gtk`, `soup3`, `javascriptcore-rs`, `muda`, `libxdo`,
// `openssl-sys`. The behavioural proof behind the manifest checks: it also
// catches a renderer or a GTK crate arriving through another dependency. Names
// compare whole, so `gtk` does not match `gtk-sys`. Host only: the host crates
// are the ones `cargo test` has just compiled, so `--offline` always finds
// them, whereas the Android-only crates may not be in a fresh clone's cache.
#[test]
fn test_host_tree_carries_no_desktop_renderer() {
    let crates = host_tree_crates();

    assert!(
        crates.iter().any(|c| c == "dioxus"),
        "the host tree must be read, and must contain dioxus: {crates:?}"
    );
    let found: Vec<&String> = crates
        .iter()
        .filter(|c| HOST_FORBIDDEN.contains(&c.as_str()))
        .collect();
    assert!(
        found.is_empty(),
        "the host build must compile no desktop renderer, found {found:?}"
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

/// The text of `code` from the first `from` to the next `to` after it, or an
/// empty string when either is missing — which no `contains` below accepts.
fn between<'a>(code: &'a str, from: &str, to: &str) -> &'a str {
    code.find(from)
        .and_then(|start| {
            let rest = code.get(start..)?;
            rest.find(to).and_then(|end| rest.get(..end))
        })
        .unwrap_or_default()
}

// Criterion: every client syncs its backend's config through the one rule —
// push only what is pending, otherwise read (#160) — and an unpaired browser
// opens on `/settings`. Both rules are tested at runtime (`backend::sync_config`,
// `tests/browser.rs`); what no runtime test sees is where the app asks them and
// what it does with the answer. A call merely present somewhere is already
// enforced by the dead-code lint, so each check is scoped to its site.
// Near-miss: the health loop's usable transition syncing nothing (or still
// calling `push_active_name`), and the start page computed but never followed.
#[test]
fn test_app_consults_the_config_sync_and_start_page_policies() {
    let shipped = shipped_code("src/main.rs");

    let usable_again = between(
        &shipped,
        "!was_usable",
        "timer::sleep(BACKEND_HEALTH_INTERVAL)",
    );
    assert!(
        usable_again.contains("sync_backend_config("),
        "the backend becoming usable (start, reconnection) must sync its config, got {usable_again:?}"
    );
    assert!(
        !shipped.contains("push_active_name("),
        "no client re-pushes its stored config on reconnection any more"
    );

    let start = between(&shipped, "start_page(", ";");
    assert!(
        start.contains("== settings::StartPage::Settings")
            && start.contains("navigator.push(Route::AppSettingsPage"),
        "the Settings start page must navigate to the settings page, got {start:?}"
    );
}

// ── CI ───────────────────────────────────────────────────────────────────────

/// The `web` job of `.github/workflows/ci.yml`, as `job` reads it.
fn web_job() -> Vec<String> {
    job("../.github/workflows/ci.yml", "web")
}

/// The job `name` of the workflow at `relative`, comment lines dropped and
/// shell line continuations joined, one logical line per entry. Empty when the
/// job does not exist.
fn job(relative: &str, name: &str) -> Vec<String> {
    let workflow = read(relative);
    let key = format!("{name}:");
    let mut in_jobs = false;
    let mut in_job = false;
    let mut lines = Vec::new();
    for line in workflow.lines() {
        if line.trim_start().starts_with('#') {
            continue;
        }
        let top_level = !line.is_empty() && !line.starts_with(' ');
        if top_level {
            in_jobs = line.starts_with("jobs:");
            in_job = false;
            continue;
        }
        let job_key = line.starts_with("  ") && !line.starts_with("   ") && line.ends_with(':');
        if in_jobs && job_key {
            in_job = line.trim() == key;
            continue;
        }
        if in_job {
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
// toolchain (1.98.1) with the wasm target. Its clippy moved to the `quality`
// job's scripts/clippy.sh (#167), so the job no longer needs the component.
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

// Criterion: the job runs `dx build --platform web`, and no clippy of its own:
// the wasm lint runs in the `quality` job through scripts/clippy.sh (#167).
// Any mention counts — `cargo clippy`, a step running scripts/clippy.sh again,
// a `clippy (wasm)` step name, the `clippy` component nothing here uses.
// Near-miss: a web job that calls scripts/clippy.sh, which a match on
// `cargo clippy` alone accepts.
#[test]
fn test_ci_web_job_runs_the_web_build_and_no_clippy() {
    let job = web_job();
    let clippy = position(&job, |l| l.contains("clippy"));
    let build = position(&job, |l| {
        l.contains("dx build")
            && l.contains("--platform web")
            && l.contains("--package blue2th-frontend")
    });

    assert!(
        clippy.is_none(),
        "the wasm clippy runs in the quality job through scripts/clippy.sh, not here"
    );
    assert!(
        build.is_some(),
        "the web job runs dx build --platform web --package blue2th-frontend"
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

// ── #175: the native packages CI installs ────────────────────────────────────

/// Every package the `apt-get install` lines of `job` name, options dropped,
/// sorted and deduplicated. Every such line counts, so a second install step
/// cannot slip a package past the first.
fn apt_packages(job: &[String]) -> Vec<String> {
    let mut packages: Vec<String> = job
        .iter()
        .filter_map(|line| line.split_once("apt-get install").map(|(_, rest)| rest))
        .flat_map(|rest| {
            rest.split_whitespace()
                .filter(|token| !token.starts_with('-'))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect();
    packages.sort();
    packages.dedup();
    packages
}

fn server_packages() -> Vec<String> {
    let mut packages: Vec<String> = SERVER_PACKAGES.iter().map(|p| (*p).to_owned()).collect();
    packages.sort();
    packages
}

// Criterion: the `quality` job's apt step installs exactly `pkg-config`,
// `libdbus-1-dev`, `libpipewire-0.3-dev`, `libclang-dev` and `clang`. Set
// equality: near-miss, the list with `libssl-dev` still in it, which a
// "contains the five server packages" check accepts.
#[test]
fn test_ci_quality_job_installs_only_the_server_native_packages() {
    let quality = job("../.github/workflows/ci.yml", "quality");

    assert!(!quality.is_empty(), "ci.yml must have a `quality` job");
    assert_eq!(
        apt_packages(&quality),
        server_packages(),
        "the quality job installs the server's native packages, and no renderer's"
    );
}

// Criterion: the release `server` job installs the same five packages, and its
// list stays identical to ci.yml's. Near-miss: release.yml with one extra
// package, which builds the server just as well.
#[test]
fn test_release_server_job_installs_the_same_packages_as_ci() {
    let server = job("../.github/workflows/release.yml", "server");
    let quality = job("../.github/workflows/ci.yml", "quality");

    assert!(!server.is_empty(), "release.yml must have a `server` job");
    assert_eq!(
        apt_packages(&server),
        server_packages(),
        "the release server job installs the server's native packages only"
    );
    assert_eq!(
        apt_packages(&server),
        apt_packages(&quality),
        "release.yml and ci.yml install the same list"
    );
}

// Criterion: the stale mentions of the `mobile` feature and of the GTK/WebKit
// stack are updated — the frontend manifest's dioxus comment, ci.yml's apt and
// `web` job comments, release.yml's claim that the server build resolves the
// frontend's host dependencies, pr-title.yml, and the two agent definitions.
// Each phrase is the stale claim as it stood on develop @ f4f012c; a rewording
// that keeps the claim passes, so the review still reads these comments.
#[test]
fn test_no_stale_mention_of_the_mobile_feature_or_the_gtk_stack_remains() {
    let stale = [
        ("Cargo.toml", "the crate's `mobile`"),
        (
            "../.github/workflows/ci.yml",
            "dioxus-desktop -> wry on a host build",
        ),
        (
            "../.github/workflows/ci.yml",
            "dx drops the `mobile` default",
        ),
        (
            "../.github/workflows/release.yml",
            "resolves the mobile crate's host",
        ),
        ("../.github/workflows/pr-title.yml", "GTK/WebKit"),
        (
            "../.claude/agents/tdd-test-writer.md",
            "Dioxus 0.7 `mobile`",
        ),
        (
            "../.claude/agents/tdd-implementer.md",
            "Dioxus 0.7 `mobile`",
        ),
    ];
    let found: Vec<(&str, &str)> = stale
        .into_iter()
        .filter(|(file, phrase)| read(file).contains(phrase))
        .collect();

    assert!(
        found.is_empty(),
        "these still describe the `mobile` feature or the GTK/WebKit stack: {found:?}"
    );
}
