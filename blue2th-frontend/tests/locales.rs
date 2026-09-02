// SPDX-License-Identifier: MIT OR Apache-2.0

//! Guards over `blue2th-frontend/locales/`.
//!
//! `rust-i18n` never fails on a key nothing reads: an orphaned entry simply sits
//! in both files forever, a key dropped from `fr.yaml` alone falls back to
//! English on a French phone, and a `t!` left behind after a deletion prints its
//! own dotted key on screen. None of the three shows up anywhere else, so the
//! locales and `src/` are checked against each other here, in both directions.
//!
//! The parsing is done by hand on purpose: the two files are exactly two levels
//! deep and flat, which is far less than a YAML dependency would cost (the
//! `serde_yaml` 0.9 line is archived upstream). The price of that shortcut is
//! that the parser must **refuse** what it does not understand: a parser that
//! quietly returns fewer keys than the file holds turns this whole file into a
//! test that passes while orphans accumulate. Every unexpected shape is an
//! `Err`, never a skipped line.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The two locale files, in the order their failures should read.
const LOCALES: [&str; 2] = ["fr", "en"];

/// Flatten a locale file into its `namespace.key` entries.
///
/// The grammar accepted is the one the files actually use: a column-0 `name:`
/// with no value opens a namespace, an indented `key: value` line is a key
/// inside it, and a column-0 `name: value` is a namespace-less top-level key.
/// Blank lines and `#` comments are skipped. Anything else — a tab indent, a
/// third level of nesting, a line without a colon, a block scalar, an
/// inconsistent indent, a duplicated key or namespace — is an error rather than
/// a silent skip, because silence here is indistinguishable from a clean file.
///
/// Returns a `Result` rather than panicking: `clippy`'s `allow-expect-in-tests`
/// does not reach a free helper in an integration-test binary, so the `expect`
/// belongs to the `#[test]` function that calls this.
fn flatten(content: &str) -> Result<BTreeSet<String>, String> {
    let mut keys = BTreeSet::new();
    let mut namespaces = BTreeSet::new();
    let mut namespace = String::new();
    // The indent of the first key under the current namespace: every sibling
    // must match it, or the file is nested deeper than this parser reads.
    let mut key_indent: Option<usize> = None;

    for (number, line) in content.lines().enumerate() {
        let number = number + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if line.starts_with('\t') {
            return Err(format!("line {number}: YAML forbids a tab indent"));
        }
        let indent = line.len() - line.trim_start().len();
        let Some((name, value)) = trimmed.split_once(':') else {
            return Err(format!(
                "line {number}: {trimmed:?} is neither a comment nor a `key: value` pair"
            ));
        };
        let name = name.trim();
        // Only the first `:` splits, so a value may hold as many as it likes —
        // `confirm_repair` does. A trailing `#` is part of the value too.
        let value = value.trim();

        if indent == 0 {
            if value.is_empty() {
                if !namespaces.insert(name.to_string()) {
                    return Err(format!(
                        "line {number}: `{name}:` is declared twice; YAML keeps one \
                         block and drops the other in silence"
                    ));
                }
                name.clone_into(&mut namespace);
                key_indent = None;
            } else {
                // A namespace-less key, addressed as `t!("name")`.
                if !keys.insert(name.to_string()) {
                    return Err(format!("line {number}: `{name}` is declared twice"));
                }
                namespace.clear();
                key_indent = None;
            }
            continue;
        }

        if namespace.is_empty() {
            return Err(format!(
                "line {number}: `{name}` is indented under no namespace"
            ));
        }
        if value.is_empty() {
            return Err(format!(
                "line {number}: `{name}:` opens a third level of nesting, which this \
                 parser does not read — flatten it or teach the parser"
            ));
        }
        match key_indent {
            None => key_indent = Some(indent),
            Some(expected) if expected == indent => {},
            Some(expected) => {
                return Err(format!(
                    "line {number}: `{name}` is indented by {indent}, its siblings by \
                     {expected} — a block scalar or a deeper level, either way unread"
                ))
            },
        }
        if !keys.insert(format!("{namespace}.{name}")) {
            return Err(format!(
                "line {number}: `{namespace}.{name}` is declared twice"
            ));
        }
    }

    Ok(keys)
}

/// The path of a locale file, anchored on the crate root — the whole scan stays
/// inside `blue2th-frontend`, unlike the workspace-wide one in `tests/settings.rs`.
fn locale_path(locale: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("locales/{locale}.yaml"))
}

/// Every `namespace.key` a locale file declares.
///
/// An empty result is an error: it is what a broken parser returns, and it would
/// make every check below pass on an empty set.
fn locale_keys(locale: &str) -> Result<BTreeSet<String>, String> {
    let path = locale_path(locale);
    let content = std::fs::read_to_string(&path).map_err(|e| format!("read {path:?}: {e}"))?;
    let keys = flatten(&content).map_err(|e| format!("{path:?}: {e}"))?;
    if keys.is_empty() {
        return Err(format!(
            "{path:?} parsed to no key at all — the parser, not the locales, is wrong"
        ));
    }
    Ok(keys)
}

/// Every `.rs` file under `blue2th-frontend/src/`, concatenated.
///
/// Walked recursively: a key used from a future `src/components/` subdirectory
/// must not read as an orphan. `tests/` is deliberately left out — a test that
/// merely enumerates a key would keep a dead one alive.
fn frontend_sources() -> Result<String, String> {
    let mut sources = String::new();
    let mut pending = vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("src")];

    while let Some(dir) = pending.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|e| format!("read {dir:?}: {e}"))?;
        for entry in entries {
            let path = entry.map_err(|e| format!("read {dir:?}: {e}"))?.path();
            if path.is_dir() {
                pending.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let source =
                std::fs::read_to_string(&path).map_err(|e| format!("read {path:?}: {e}"))?;
            sources.push_str(&source);
            sources.push('\n');
        }
    }

    if sources.is_empty() {
        return Err("no frontend source file was read at all".to_string());
    }
    Ok(sources)
}

/// Whether the sources address `key` through a string literal.
///
/// Both quotes are part of the needle, so a key is never counted as used
/// because a longer key contains it: `"spotify.status_stopped"` does not occur
/// inside `"spotify.status_stopped_v2"`. The escaped form is the one `main.rs`
/// writes inside `rsx!` string literals.
fn is_referenced(sources: &str, key: &str) -> bool {
    sources.contains(&format!("\"{key}\"")) || sources.contains(&format!("\\\"{key}\\\""))
}

/// Every literal key the sources hand to `t!`.
///
/// A call site is `t!(` preceded by a delimiter or a path separator — never by
/// an identifier byte, which is what tells it from `assert!(`, `format!(` and
/// friends. A key built at runtime has no literal to read and is skipped; there
/// is none today, and `t!(key_variable)` staying unchecked is the honest
/// outcome rather than a false alarm.
fn referenced_keys(sources: &str) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    let bytes = sources.as_bytes();

    for (start, _) in sources.match_indices("t!(") {
        if start > 0 {
            let previous = bytes[start - 1];
            if previous.is_ascii_alphanumeric() || previous == b'_' {
                continue;
            }
        }
        let rest = &sources[start + "t!(".len()..];
        // `\"key\"` in an rsx literal, `"key"` everywhere else.
        let rest = rest.strip_prefix('\\').unwrap_or(rest);
        let Some(rest) = rest.strip_prefix('"') else {
            continue;
        };
        let Some(end) = rest.find(['"', '\\']) else {
            continue;
        };
        let key = &rest[..end];
        let readable = !key.is_empty()
            && key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.');
        if readable {
            keys.insert(key.to_string());
        }
    }

    keys
}

// Criterion: a test fails when a locale key is referenced nowhere in
// `blue2th-frontend/src/`, and names the offenders. It matches both `"key"` and
// the escaped `\"key\"` rsx form, and it reads `src/` only. Matching is on the
// full dotted key, so `transport.status_stopped` and `spotify.status_stopped`
// are told apart.
#[test]
fn test_every_locale_key_is_referenced_in_the_frontend_sources() {
    let sources = frontend_sources().expect("the frontend sources must be readable");

    let mut offenders = BTreeSet::new();
    for locale in LOCALES {
        let keys = locale_keys(locale).expect("both locale files must parse");
        offenders.extend(keys.into_iter().filter(|key| !is_referenced(&sources, key)));
    }

    assert!(
        offenders.is_empty(),
        "these locale keys are referenced nowhere in blue2th-frontend/src/ \
         and must be deleted from both locale files: {}",
        offenders.into_iter().collect::<Vec<String>>().join(", ")
    );
}

// Criterion: the reverse direction — a key the sources ask `t!` for must exist
// in the locales. `rust-i18n` answers an unknown key with the key itself, so a
// deletion that forgets its call site ships `spotify.hint` as the on-screen
// label instead of failing anywhere.
#[test]
fn test_every_key_the_sources_use_exists_in_both_locale_files() {
    let sources = frontend_sources().expect("the frontend sources must be readable");
    let used = referenced_keys(&sources);
    assert!(
        !used.is_empty(),
        "no `t!` call site was found at all — the scanner, not the sources, is wrong"
    );

    let mut offenders = Vec::new();
    for locale in LOCALES {
        let keys = locale_keys(locale).expect("both locale files must parse");
        for key in used.difference(&keys) {
            offenders.push(format!("{key} (missing from {locale}.yaml)"));
        }
    }

    assert!(
        offenders.is_empty(),
        "these keys are used in blue2th-frontend/src/ but declared in no locale file, \
         and would render as their own dotted key: {}",
        offenders.join(", ")
    );
}

// Criterion: a test fails when `fr.yaml` and `en.yaml` do not carry the same key
// set, naming the difference in each direction. `rust-i18n` falls back to `en`,
// so a key deleted from `fr.yaml` alone shows English on a French phone instead
// of failing anywhere — this is what makes a one-sided deletion fail.
#[test]
fn test_the_two_locale_files_carry_the_same_keys() {
    let fr = locale_keys("fr").expect("the French locale file must parse");
    let en = locale_keys("en").expect("the English locale file must parse");

    let missing_in_en: Vec<&String> = fr.difference(&en).collect();
    let missing_in_fr: Vec<&String> = en.difference(&fr).collect();
    assert!(
        missing_in_en.is_empty() && missing_in_fr.is_empty(),
        "the two locale files must carry the same keys; \
         missing from en.yaml: {missing_in_en:?}, missing from fr.yaml: {missing_in_fr:?}"
    );
}

// Criterion: the parser under-reports on nothing. Each case below is a shape the
// locale files could grow into, and each one that parsed to "no key here" would
// let an orphan through unnoticed.
#[test]
fn test_the_parser_reads_every_shape_the_locale_files_use() {
    let keys = flatten(
        "# a comment\n\
         \n\
         scan:\n\
         \x20 # an indented comment\n\
         \x20 button: \"Load\"\n\
         \x20 bare: unquoted\n\
         \x20 hint: \"%{name} moved to %{url}: update?\" # a trailing note\n\
         \x20 hash: \"a # b\"\n\
         top_level: \"no namespace\"\n\
         device:\n\
         \x20 empty: \"\"\n",
    )
    .expect("every shape here is one the locale files already use");

    assert_eq!(
        keys.into_iter().collect::<Vec<String>>(),
        vec![
            "device.empty",
            "scan.bare",
            "scan.button",
            "scan.hash",
            "scan.hint",
            "top_level",
        ]
    );
}

// Criterion: the parser refuses what it cannot read. A silently skipped line is
// the failure that matters here — it makes this file pass while the locales rot.
#[test]
fn test_the_parser_refuses_what_it_cannot_read() {
    for (label, content) in [
        (
            "a third level of nesting",
            "app_settings:\n  section:\n    title: \"Réglages\"\n",
        ),
        ("a list item", "scan:\n  button: \"Load\"\n  - orphan\n"),
        (
            "a block scalar continuation",
            "scan:\n  button: >\n    a folded: line\n",
        ),
        ("a tab indent", "scan:\n\tbutton: \"Load\"\n"),
        ("a key under no namespace", "  button: \"Load\"\n"),
        (
            "a duplicated namespace",
            "scan:\n  button: \"Load\"\nscan:\n  other: \"Other\"\n",
        ),
        (
            "a duplicated key",
            "scan:\n  button: \"Load\"\n  button: \"Reload\"\n",
        ),
    ] {
        let parsed = flatten(content);
        assert!(
            parsed.is_err(),
            "{label} must be refused, not skipped; got {parsed:?}"
        );
    }
}

// Criterion: the "used" matcher is honest in both directions — a longer key that
// merely contains a shorter one does not mark the shorter one as used, and the
// escaped rsx form still counts.
#[test]
fn test_the_matcher_tells_a_key_from_a_longer_one() {
    let sources = "t!(\"spotify.status_stopped_v2\")\n";
    assert!(!is_referenced(sources, "spotify.status_stopped"));
    assert!(is_referenced(sources, "spotify.status_stopped_v2"));
    assert!(is_referenced(
        "rsx! { t!(\\\"scan.button\\\") }",
        "scan.button"
    ));
}

// Criterion: the `t!` scanner reads the call sites and nothing else — neither
// `assert!(`/`format!(`, which end in the same three bytes, nor a key built at
// runtime, which has no literal to read.
#[test]
fn test_the_scanner_reads_t_call_sites_only() {
    let keys = referenced_keys(
        "assert!(\"not.a.key\");\n\
         let msg = format!(\"{x}\");\n\
         t!(\"scan.button\")\n\
         rust_i18n::t!(\"device.empty\")\n\
         t!(\\\"transport.play\\\")\n\
         t!(built_at_runtime)\n",
    );
    assert_eq!(
        keys.into_iter().collect::<Vec<String>>(),
        vec!["device.empty", "scan.button", "transport.play"]
    );
}

// Criterion (#52): the pairing-failure toast uses a localised key, present in
// both `en.yaml` and `fr.yaml`. The parity test above only checks the two files
// agree — it stays green while a key is missing from both, which is exactly the
// case here, so the key is named explicitly.
#[test]
fn test_the_bluetooth_pairing_failure_key_exists_in_both_locales() {
    const KEY: &str = "device.pairing_failed";

    for locale in LOCALES {
        let keys = locale_keys(locale).expect("the locale file must parse");
        assert!(
            keys.contains(KEY),
            "{locale}.yaml must carry {KEY:?}, the message shown when a speaker refuses the bond"
        );
    }
}

/// The value of one `namespace.key` entry, with its surrounding quotes removed.
///
/// Addressed by its **full path**: `not_paired` is declared twice — under
/// `server:` (this app and its backend) and under `app_settings:` — so a lookup
/// on the bare name would silently read whichever line comes first, and would
/// start reading the other one the day the blocks are reordered.
///
/// Lenient where [`flatten`] is strict: the grammar is already guarded by the
/// tests above, and a line this helper cannot read is simply not the one asked
/// for. Returns `None` when the path is absent, which the caller asserts on.
fn locale_value(content: &str, path: &str) -> Option<String> {
    let mut namespace = String::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((name, value)) = trimmed.split_once(':') else {
            continue;
        };
        let (name, value) = (name.trim(), value.trim());
        let indented = line.starts_with(char::is_whitespace);

        if indented {
            if format!("{namespace}.{name}") == path {
                return Some(value.trim_matches('"').to_string());
            }
        } else if value.is_empty() {
            name.clone_into(&mut namespace);
        } else if name == path {
            return Some(value.trim_matches('"').to_string());
        }
    }
    None
}

// Criterion (#52): the wording must not be confused with app-to-backend pairing.
// `server.not_paired` already owns that sentence, so the two must not read the
// same, and the Bluetooth one has to name the speaker.
#[test]
fn test_the_bluetooth_pairing_failure_message_is_not_the_backend_pairing_one() {
    for locale in LOCALES {
        let path = locale_path(locale);
        let content = std::fs::read_to_string(&path).expect("read the locale file");

        let bluetooth = locale_value(&content, "device.pairing_failed");
        assert!(
            bluetooth.is_some(),
            "{locale}.yaml must carry a `device.pairing_failed` message"
        );
        let backend = locale_value(&content, "server.not_paired");
        assert!(
            backend.is_some(),
            "{locale}.yaml must still carry the backend `server.not_paired` message"
        );
        assert_ne!(
            bluetooth, backend,
            "{locale}.yaml must word the speaker pairing failure differently \
             from the unpaired-backend message"
        );
    }
}

// The helper above is what the test before it leans on, and a lookup that always
// answered `None` would make its `assert_ne!` compare nothing at all.
#[test]
fn test_locale_value_reads_the_named_namespace_only() {
    let content =
        "server:\n  not_paired: \"backend\"\n\napp_settings:\n  not_paired: \"settings\"\n";

    assert_eq!(
        locale_value(content, "server.not_paired").as_deref(),
        Some("backend")
    );
    assert_eq!(
        locale_value(content, "app_settings.not_paired").as_deref(),
        Some("settings"),
        "the second block must be reachable, not shadowed by the first"
    );
    assert_eq!(locale_value(content, "device.not_paired"), None);
}
