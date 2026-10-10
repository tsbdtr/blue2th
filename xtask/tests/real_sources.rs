// SPDX-License-Identifier: MIT OR Apache-2.0
//! The rules against files as they stand in this repository: the fixtures
//! nobody wrote to agree with the checker.

use std::path::PathBuf;

use xtask::{inline_tests, parse_config, Level, Rule};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn read(relative: &str) -> std::io::Result<String> {
    std::fs::read_to_string(repo_root().join(relative))
}

#[test]
fn test_graph_fake_support_module_is_not_an_inline_test() {
    let path = "blue2th-server/src/graph.rs";
    let content = read(path).unwrap();
    assert!(content.contains("pub mod fake {"), "fixture moved: {path}");
    assert_eq!(inline_tests(path, &content), Ok(Vec::new()));
}

#[test]
fn test_router_actor_testing_support_module_is_not_an_inline_test() {
    let path = "blue2th-server/src/router_actor.rs";
    let content = read(path).unwrap();
    assert!(content.contains("mod testing {"), "fixture moved: {path}");
    assert_eq!(inline_tests(path, &content), Ok(Vec::new()));
}

#[test]
fn test_cfg_test_in_a_doc_comment_of_a_real_file_is_not_an_inline_test() {
    let path = "blue2th-server/src/spotify/tests.rs";
    let content = read(path).unwrap();
    assert!(
        content.contains("first `#[cfg(test)]`"),
        "fixture moved: {path}"
    );
    assert_eq!(inline_tests(path, &content), Ok(Vec::new()));
}

/// The root manifest names both rules rather than leaning on the default, so
/// the policy is read where it is set.
#[test]
fn test_root_manifest_declares_both_rules_at_deny_without_exclusions() {
    let manifest = read("Cargo.toml").unwrap();
    let value: toml::Value = toml::from_str(&manifest).unwrap();
    let conventions = value["workspace"]["metadata"]["conventions"]
        .as_table()
        .unwrap();
    for rule in Rule::ALL {
        assert_eq!(
            conventions.get(rule.name()).and_then(toml::Value::as_str),
            Some("deny"),
            "{}",
            rule.name()
        );
    }
    let config = parse_config(&manifest).unwrap();
    for rule in Rule::ALL {
        assert_eq!(config.rule(rule).level, Level::Deny);
        assert!(config.rule(rule).exclude.is_empty());
    }
}
