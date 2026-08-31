// SPDX-License-Identifier: MIT OR Apache-2.0

//! Guards over `blue2th-frontend/locales/`.
//!
//! `rust-i18n` never fails on a key nothing reads: an orphaned entry simply sits
//! in both files forever, and a key dropped from `fr.yaml` alone falls back to
//! English on a French phone. Neither shows up anywhere else, so they are
//! checked here — by scanning the sources for every key the locales declare.
//!
//! The parsing is done by hand on purpose: the two files are exactly two levels
//! deep and flat, which is far less than a YAML dependency would cost (the
//! `serde_yaml` 0.9 line is archived upstream).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Flatten a locale file into its `namespace.key` entries.
///
/// A line starting at column 0 with `name:` opens a namespace; an indented
/// `key: "value"` line is a key inside it. Blank lines and `#` comments are
/// skipped. Pure on purpose: `clippy`'s `allow-expect-in-tests` does not reach a
/// free helper in an integration-test binary, so this one cannot fail.
fn flatten(content: &str) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    let mut namespace = String::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((name, _)) = trimmed.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if line.starts_with([' ', '\t']) {
            if !namespace.is_empty() {
                keys.insert(format!("{namespace}.{name}"));
            }
        } else {
            // A top-level mapping: everything indented below it belongs to it.
            namespace = name.to_string();
        }
    }

    keys
}

/// The path of a locale file, anchored on the crate root — the whole scan stays
/// inside `blue2th-frontend`, unlike the workspace-wide one in `tests/settings.rs`.
fn locale_path(locale: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("locales/{locale}.yaml"))
}

// Criterion: a test fails when a locale key is referenced nowhere in
// `blue2th-frontend/src/`, and names the offenders. It matches both `"key"` and
// the escaped `\"key\"` rsx form (`main.rs` writes keys inside escaped string
// literals), and it reads `src/` only — never `tests/`, where a test that merely
// enumerates a key would keep a dead one alive. Matching is on the full dotted
// key, so `transport.status_stopped` and `spotify.status_stopped` are told apart.
#[test]
fn test_every_locale_key_is_referenced_in_the_frontend_sources() {
    let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));

    let mut sources = String::new();
    let src = crate_root.join("src");
    // Mapped to a String so the failure names the directory without a `panic!`,
    // which clippy forbids even in tests here.
    let entries = std::fs::read_dir(&src)
        .map_err(|e| format!("read {src:?}: {e}"))
        .expect("the frontend source directory must be readable");
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let source = std::fs::read_to_string(&path)
            .map_err(|e| format!("read {path:?}: {e}"))
            .expect("every frontend source file must be readable");
        sources.push_str(&source);
        sources.push('\n');
    }

    let mut offenders = BTreeSet::new();
    for locale in ["fr", "en"] {
        let path = locale_path(locale);
        let content = std::fs::read_to_string(&path)
            .map_err(|e| format!("read {path:?}: {e}"))
            .expect("both locale files must be readable");
        let keys = flatten(&content);
        assert!(
            !keys.is_empty(),
            "{path:?} parsed to no key at all — the parser, not the locales, is wrong"
        );
        for key in keys {
            let plain = format!("\"{key}\"");
            let escaped = format!("\\\"{key}\\\"");
            if !sources.contains(&plain) && !sources.contains(&escaped) {
                offenders.insert(key);
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "these locale keys are referenced nowhere in blue2th-frontend/src/ \
         and must be deleted from both locale files: {}",
        offenders.into_iter().collect::<Vec<String>>().join(", ")
    );
}

// Criterion: a test fails when `fr.yaml` and `en.yaml` do not carry the same key
// set, naming the difference in each direction. `rust-i18n` falls back to `en`,
// so a key deleted from `fr.yaml` alone shows English on a French phone instead
// of failing anywhere — this is what makes a one-sided deletion fail.
#[test]
fn test_the_two_locale_files_carry_the_same_keys() {
    let fr_path = locale_path("fr");
    let en_path = locale_path("en");
    let fr = std::fs::read_to_string(&fr_path)
        .map_err(|e| format!("read {fr_path:?}: {e}"))
        .expect("the French locale file must be readable");
    let en = std::fs::read_to_string(&en_path)
        .map_err(|e| format!("read {en_path:?}: {e}"))
        .expect("the English locale file must be readable");

    let fr = flatten(&fr);
    let en = flatten(&en);
    assert!(!fr.is_empty(), "fr.yaml parsed to no key at all");

    let missing_in_en: Vec<&String> = fr.difference(&en).collect();
    let missing_in_fr: Vec<&String> = en.difference(&fr).collect();
    assert!(
        missing_in_en.is_empty() && missing_in_fr.is_empty(),
        "the two locale files must carry the same keys; \
         missing from en.yaml: {missing_in_en:?}, missing from fr.yaml: {missing_in_fr:?}"
    );
}
