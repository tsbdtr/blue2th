// SPDX-License-Identifier: MIT OR Apache-2.0

use super::*;

fn file(path: &str, content: &str) -> SourceFile {
    SourceFile {
        path: path.to_string(),
        content: content.to_string(),
    }
}

/// A source carrying the licence line, so `spdx-header` stays quiet and a test
/// sees only the rule it is about.
fn with_header(body: &str) -> String {
    format!("{SPDX_HEADER}\n{body}")
}

fn manifest(conventions: &str) -> String {
    format!("[workspace]\nmembers = [\"a\"]\n\n[workspace.metadata.conventions]\n{conventions}\n")
}

fn config(conventions: &str) -> Config {
    parse_config(&manifest(conventions)).unwrap()
}

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|a| a.to_string()).collect()
}

const INLINE_TESTS: &str =
    "pub fn f() {}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {}\n}\n";

fn inline_lines(path: &str, content: &str) -> Vec<usize> {
    inline_tests(path, content)
        .unwrap()
        .into_iter()
        .map(|f| match f {
            Finding::Violation {
                rule: Rule::InlineTests,
                line,
                ..
            } => Some(line),
            _ => None,
        })
        .collect::<Option<Vec<usize>>>()
        .unwrap()
}

// ── spdx-header ─────────────────────────────────────────────────────────────

#[test]
fn test_spdx_header_on_the_first_line_is_not_a_finding() {
    assert_eq!(spdx_header("a/src/x.rs", &with_header("fn f() {}\n")), None);
}

#[test]
fn test_spdx_header_alone_without_a_newline_is_not_a_finding() {
    assert_eq!(spdx_header("a/src/x.rs", SPDX_HEADER), None);
}

#[test]
fn test_spdx_header_missing_is_a_finding_at_line_one() {
    let finding = spdx_header("a/src/x.rs", "fn f() {}\n").unwrap();
    assert!(matches!(
        finding,
        Finding::Violation { rule: Rule::SpdxHeader, ref path, line: 1, .. } if path == "a/src/x.rs"
    ));
}

#[test]
fn test_spdx_header_prefix_only_is_a_finding() {
    assert!(spdx_header("a/src/x.rs", "// SPDX-License-Identifier: MIT\n").is_some());
}

#[test]
fn test_spdx_header_with_a_trailing_space_is_a_finding() {
    let content = format!("{SPDX_HEADER} \nfn f() {{}}\n");
    assert!(spdx_header("a/src/x.rs", &content).is_some());
}

#[test]
fn test_spdx_header_on_line_two_is_a_finding() {
    let content = format!("\n{SPDX_HEADER}\nfn f() {{}}\n");
    assert!(spdx_header("a/src/x.rs", &content).is_some());
}

#[test]
fn test_spdx_header_with_a_crlf_is_a_finding() {
    let content = format!("{SPDX_HEADER}\r\nfn f() {{}}\r\n");
    assert!(spdx_header("a/src/x.rs", &content).is_some());
}

#[test]
fn test_spdx_header_after_a_bom_is_a_finding() {
    let content = format!("\u{feff}{SPDX_HEADER}\n");
    assert!(spdx_header("a/src/x.rs", &content).is_some());
}

#[test]
fn test_spdx_header_on_an_empty_file_is_a_finding() {
    assert!(spdx_header("a/src/x.rs", "").is_some());
}

// ── inline-tests ────────────────────────────────────────────────────────────

#[test]
fn test_inline_tests_module_is_a_finding_at_the_mod_line() {
    assert_eq!(inline_lines("a/src/x.rs", INLINE_TESTS), vec![4]);
}

#[test]
fn test_inline_tests_finding_names_the_file() {
    let findings = inline_tests("a/src/x.rs", INLINE_TESTS).unwrap();
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].path(), "a/src/x.rs");
}

#[test]
fn test_inline_tests_declaration_without_body_is_not_a_finding() {
    let content = "pub fn f() {}\n\n#[cfg(test)]\nmod tests;\n";
    assert_eq!(inline_lines("a/src/x.rs", content), Vec::<usize>::new());
}

#[test]
fn test_inline_tests_tokio_test_is_a_finding() {
    let content = "#[cfg(test)]\nmod tests {\n    #[tokio::test]\n    async fn t() {}\n}\n";
    assert_eq!(inline_lines("a/src/x.rs", content), vec![2]);
}

#[test]
fn test_inline_tests_tokio_test_with_arguments_is_a_finding() {
    let content = "#[cfg(test)]\nmod tests {\n    #[tokio::test(flavor = \"multi_thread\")]\n    async fn t() {}\n}\n";
    assert_eq!(inline_lines("a/src/x.rs", content), vec![2]);
}

#[test]
fn test_inline_tests_cfg_all_test_is_a_finding() {
    let content =
        "#[cfg(all(test, target_os = \"linux\"))]\nmod tests {\n    #[test]\n    fn t() {}\n}\n";
    assert_eq!(inline_lines("a/src/x.rs", content), vec![2]);
}

#[test]
fn test_inline_tests_cfg_any_test_is_a_finding() {
    let content =
        "#[cfg(any(test, feature = \"x\"))]\nmod tests {\n    #[test]\n    fn t() {}\n}\n";
    assert_eq!(inline_lines("a/src/x.rs", content), vec![2]);
}

#[test]
fn test_inline_tests_cfg_not_test_is_not_a_finding() {
    let content = "#[cfg(not(test))]\nmod m {\n    #[test]\n    fn t() {}\n}\n";
    assert_eq!(inline_lines("a/src/x.rs", content), Vec::<usize>::new());
}

#[test]
fn test_inline_tests_other_attribute_between_cfg_and_mod_is_a_finding() {
    let content = "#[cfg(test)]\n#[allow(clippy::module_inception)]\nmod tests {\n    #[test]\n    fn t() {}\n}\n";
    assert_eq!(inline_lines("a/src/x.rs", content), vec![3]);
}

#[test]
fn test_inline_tests_support_module_without_test_fn_is_not_a_finding() {
    let content = "#[cfg(test)]\npub mod fake {\n    pub struct Fake;\n    pub fn new() -> Fake { Fake }\n}\n";
    assert_eq!(inline_lines("a/src/x.rs", content), Vec::<usize>::new());
}

#[test]
fn test_inline_tests_support_module_with_one_test_fn_is_a_finding() {
    let content = "#[cfg(test)]\npub mod fake {\n    pub struct Fake;\n    #[tokio::test]\n    async fn t() {}\n}\n";
    assert_eq!(inline_lines("a/src/x.rs", content), vec![2]);
}

#[test]
fn test_inline_tests_test_fn_in_a_nested_module_is_a_finding() {
    let content = "#[cfg(test)]\nmod outer {\n    mod inner {\n        #[test]\n        fn t() {}\n    }\n}\n";
    assert_eq!(inline_lines("a/src/x.rs", content), vec![2]);
}

#[test]
fn test_inline_tests_module_nested_in_another_module_is_a_finding() {
    let content = "mod outer {\n    #[cfg(test)]\n    mod tests {\n        #[test]\n        fn t() {}\n    }\n}\n";
    assert_eq!(inline_lines("a/src/x.rs", content), vec![3]);
}

#[test]
fn test_inline_tests_two_modules_are_two_findings() {
    let content = "#[cfg(test)]\nmod a {\n    #[test]\n    fn t() {}\n}\n\n#[cfg(test)]\nmod b {\n    #[test]\n    fn t() {}\n}\n";
    assert_eq!(inline_lines("a/src/x.rs", content), vec![2, 8]);
}

#[test]
fn test_inline_tests_cfg_test_in_a_comment_or_a_string_is_not_a_finding() {
    let content = "/// Cutting at the first `#[cfg(test)]` used to stop early.\n// #[cfg(test)] mod tests { #[test] fn t() {} }\nconst S: &str = \"#[cfg(test)] mod tests { #[test] fn t() {} }\";\n";
    assert_eq!(inline_lines("a/src/x.rs", content), Vec::<usize>::new());
}

#[test]
fn test_inline_tests_outside_src_is_not_a_finding() {
    assert_eq!(
        inline_lines("a/tests/x.rs", INLINE_TESTS),
        Vec::<usize>::new()
    );
}

#[test]
fn test_inline_tests_directory_merely_containing_src_is_not_a_finding() {
    assert_eq!(
        inline_lines("a/my_src/x.rs", INLINE_TESTS),
        Vec::<usize>::new()
    );
}

#[test]
fn test_inline_tests_nested_under_src_is_a_finding() {
    assert_eq!(inline_lines("a/src/views/x.rs", INLINE_TESTS), vec![4]);
}

#[test]
fn test_inline_tests_unparsable_src_file_is_an_error() {
    let err = inline_tests("a/src/x.rs", "fn {").unwrap_err();
    assert_eq!(err.path, "a/src/x.rs");
}

#[test]
fn test_inline_tests_unparsable_file_outside_src_is_not_an_error() {
    assert_eq!(inline_tests("a/tests/x.rs", "fn {"), Ok(Vec::new()));
}

// ── config ──────────────────────────────────────────────────────────────────

#[test]
fn test_parse_config_level_strings_are_read() {
    let config = config("inline-tests = \"warn\"\nspdx-header = \"allow\"");
    assert_eq!(config.rule(Rule::InlineTests).level, Level::Warn);
    assert_eq!(config.rule(Rule::SpdxHeader).level, Level::Allow);
}

#[test]
fn test_parse_config_table_reads_level_and_exclude() {
    let config = config("inline-tests = { level = \"warn\", exclude = [\"a/src/x.rs\"] }");
    assert_eq!(
        config.rule(Rule::InlineTests),
        &RuleConfig {
            level: Level::Warn,
            exclude: vec!["a/src/x.rs".to_string()],
        }
    );
}

#[test]
fn test_parse_config_table_without_level_defaults_to_deny() {
    let config = config("inline-tests = { exclude = [\"a/src/x.rs\"] }\nspdx-header = \"warn\"");
    assert_eq!(config.rule(Rule::InlineTests).level, Level::Deny);
}

#[test]
fn test_parse_config_absent_rule_defaults_to_deny() {
    let config = config("inline-tests = \"warn\"");
    assert_eq!(config.rule(Rule::SpdxHeader), &RuleConfig::default());
    assert_eq!(RuleConfig::default().level, Level::Deny);
}

#[test]
fn test_parse_config_without_conventions_table_defaults_every_rule_to_deny() {
    let config =
        parse_config("[workspace]\nmembers = [\"a\"]\n\n[workspace.metadata.other]\nx = 1\n")
            .unwrap();
    for rule in Rule::ALL {
        assert_eq!(config.rule(rule).level, Level::Deny, "{}", rule.name());
        assert!(config.rule(rule).exclude.is_empty());
    }
}

#[test]
fn test_parse_config_unknown_rule_is_an_error() {
    assert_eq!(
        parse_config(&manifest("inline-test = \"allow\"")),
        Err(ConfigError::UnknownRule("inline-test".to_string()))
    );
}

#[test]
fn test_parse_config_unknown_level_is_an_error() {
    for value in ["error", "Deny", ""] {
        assert_eq!(
            parse_config(&manifest(&format!("inline-tests = \"{value}\""))),
            Err(ConfigError::UnknownLevel {
                rule: "inline-tests".to_string(),
                value: value.to_string(),
            }),
            "{value:?}"
        );
    }
}

#[test]
fn test_parse_config_unknown_level_in_a_table_is_an_error() {
    assert!(matches!(
        parse_config(&manifest("inline-tests = { level = \"off\" }")),
        Err(ConfigError::UnknownLevel { .. })
    ));
}

#[test]
fn test_parse_config_unknown_key_in_a_rule_table_is_an_error() {
    assert_eq!(
        parse_config(&manifest(
            "inline-tests = { level = \"deny\", excludes = [\"a/src/x.rs\"] }"
        )),
        Err(ConfigError::UnknownKey {
            rule: "inline-tests".to_string(),
            key: "excludes".to_string(),
        })
    );
}

#[test]
fn test_parse_config_rule_value_neither_string_nor_table_is_an_error() {
    assert_eq!(
        parse_config(&manifest("inline-tests = 1")),
        Err(ConfigError::InvalidRule {
            rule: "inline-tests".to_string(),
        })
    );
}

#[test]
fn test_parse_config_exclude_not_an_array_of_strings_is_an_error() {
    for exclude in ["\"a/src/x.rs\"", "[1]"] {
        assert_eq!(
            parse_config(&manifest(&format!(
                "inline-tests = {{ exclude = {exclude} }}"
            ))),
            Err(ConfigError::InvalidExclude {
                rule: "inline-tests".to_string(),
            }),
            "{exclude}"
        );
    }
}

#[test]
fn test_parse_config_empty_exclude_entry_is_an_error() {
    assert_eq!(
        parse_config(&manifest(
            "inline-tests = { exclude = [\"a/src/x.rs\", \"\"] }"
        )),
        Err(ConfigError::EmptyExclude {
            rule: "inline-tests".to_string(),
        })
    );
}

#[test]
fn test_parse_config_conventions_not_a_table_is_an_error() {
    let manifest =
        "[workspace]\nmembers = [\"a\"]\n\n[workspace.metadata]\nconventions = \"deny\"\n";
    assert_eq!(parse_config(manifest), Err(ConfigError::NotATable));
}

#[test]
fn test_parse_config_invalid_toml_is_an_error() {
    assert!(matches!(
        parse_config("[workspace\n"),
        Err(ConfigError::Toml(_))
    ));
}

// ── arguments ───────────────────────────────────────────────────────────────

#[test]
fn test_parse_args_none_is_no_escalation() {
    assert_eq!(parse_args(&[]), Ok(Escalation::None));
}

#[test]
fn test_parse_args_deny_warnings_escalates() {
    assert_eq!(
        parse_args(&args(&["-D", "warnings"])),
        Ok(Escalation::DenyWarnings)
    );
}

#[test]
fn test_parse_args_deny_alone_is_an_error() {
    assert!(parse_args(&args(&["-D"])).is_err());
}

#[test]
fn test_parse_args_deny_something_else_is_an_error() {
    assert!(parse_args(&args(&["-D", "clippy"])).is_err());
}

#[test]
fn test_parse_args_unknown_argument_is_an_error() {
    assert!(parse_args(&args(&["--fix"])).is_err());
}

// ── check: levels ───────────────────────────────────────────────────────────

fn severities(report: &Report) -> Vec<(Severity, Rule)> {
    report
        .entries
        .iter()
        .map(|e| (e.severity, e.finding.rule()))
        .collect()
}

#[test]
fn test_check_clean_tree_exits_zero() {
    let files = [file("a/src/x.rs", &with_header("fn f() {}\n"))];
    let report = check(&files, &Config::default(), Escalation::None).unwrap();
    assert!(report.entries.is_empty());
    assert_eq!(report.exit_code(), 0);
}

#[test]
fn test_check_deny_finding_is_an_error_and_exits_one() {
    let files = [file("a/src/x.rs", &with_header(INLINE_TESTS))];
    let report = check(&files, &Config::default(), Escalation::None).unwrap();
    assert_eq!(
        severities(&report),
        vec![(Severity::Error, Rule::InlineTests)]
    );
    assert_eq!(report.exit_code(), 1);
}

#[test]
fn test_check_warn_finding_is_a_warning_and_exits_zero() {
    let files = [file("a/src/x.rs", &with_header(INLINE_TESTS))];
    let report = check(&files, &config("inline-tests = \"warn\""), Escalation::None).unwrap();
    assert_eq!(
        severities(&report),
        vec![(Severity::Warning, Rule::InlineTests)]
    );
    assert_eq!(report.exit_code(), 0);
}

#[test]
fn test_check_warn_finding_under_deny_warnings_is_an_error_and_exits_one() {
    let files = [file("a/src/x.rs", &with_header(INLINE_TESTS))];
    let report = check(
        &files,
        &config("inline-tests = \"warn\""),
        Escalation::DenyWarnings,
    )
    .unwrap();
    assert_eq!(
        severities(&report),
        vec![(Severity::Error, Rule::InlineTests)]
    );
    assert_eq!(report.exit_code(), 1);
}

#[test]
fn test_check_allow_rule_does_not_run() {
    let files = [file("a/src/x.rs", INLINE_TESTS)];
    let report = check(
        &files,
        &config("inline-tests = \"allow\"\nspdx-header = \"allow\""),
        Escalation::DenyWarnings,
    )
    .unwrap();
    assert!(report.entries.is_empty());
    assert_eq!(report.exit_code(), 0);
}

#[test]
fn test_check_allow_rule_does_not_parse_its_files() {
    let files = [file("a/src/x.rs", &with_header("fn {"))];
    let report = check(
        &files,
        &config("inline-tests = \"allow\""),
        Escalation::None,
    );
    assert_eq!(report, Ok(Report::default()));
}

#[test]
fn test_check_both_rules_report_on_the_same_file() {
    let files = [file("a/src/x.rs", INLINE_TESTS)];
    let report = check(&files, &Config::default(), Escalation::None).unwrap();
    let mut rules: Vec<&str> = report
        .entries
        .iter()
        .map(|e| e.finding.rule().name())
        .collect();
    rules.sort();
    assert_eq!(rules, vec!["inline-tests", "spdx-header"]);
}

#[test]
fn test_check_unparsable_src_file_is_an_error() {
    let files = [file("a/src/x.rs", &with_header("fn {"))];
    let err = check(&files, &Config::default(), Escalation::None).unwrap_err();
    assert_eq!(err.path, "a/src/x.rs");
}

// ── check: exclusions ───────────────────────────────────────────────────────

#[test]
fn test_check_excluded_file_is_not_reported() {
    let files = [file("a/src/x.rs", &with_header(INLINE_TESTS))];
    let report = check(
        &files,
        &config("inline-tests = { exclude = [\"a/src/x.rs\"] }"),
        Escalation::None,
    )
    .unwrap();
    assert!(report.entries.is_empty(), "{:?}", report.entries);
    assert_eq!(report.exit_code(), 0);
}

#[test]
fn test_check_exclusion_only_covers_its_own_file() {
    let files = [
        file("a/src/x.rs", &with_header(INLINE_TESTS)),
        file("a/src/y.rs", &with_header(INLINE_TESTS)),
    ];
    let report = check(
        &files,
        &config("inline-tests = { exclude = [\"a/src/x.rs\"] }"),
        Escalation::None,
    )
    .unwrap();
    let paths: Vec<&str> = report.entries.iter().map(|e| e.finding.path()).collect();
    assert_eq!(paths, vec!["a/src/y.rs"]);
}

#[test]
fn test_check_exclusion_does_not_silence_another_rule() {
    let files = [file("a/src/x.rs", INLINE_TESTS)];
    let report = check(
        &files,
        &config("inline-tests = { exclude = [\"a/src/x.rs\"] }"),
        Escalation::None,
    )
    .unwrap();
    assert_eq!(
        severities(&report),
        vec![(Severity::Error, Rule::SpdxHeader)]
    );
}

#[test]
fn test_check_exclusion_of_an_untracked_path_is_stale() {
    let files = [file("a/src/x.rs", &with_header("fn f() {}\n"))];
    let report = check(
        &files,
        &config("inline-tests = { exclude = [\"a/src/gone.rs\"] }"),
        Escalation::None,
    )
    .unwrap();
    assert_eq!(
        report.entries,
        vec![Entry {
            severity: Severity::Error,
            finding: Finding::StaleExclusion {
                rule: Rule::InlineTests,
                path: "a/src/gone.rs".to_string(),
                reason: Staleness::NoSuchFile,
            },
        }]
    );
    assert_eq!(report.exit_code(), 1);
}

#[test]
fn test_check_exclusion_by_prefix_excludes_nothing_and_is_stale() {
    let files = [file("a/src/graph.rs", &with_header(INLINE_TESTS))];
    let report = check(
        &files,
        &config("inline-tests = { exclude = [\"a/src/graph\"] }"),
        Escalation::None,
    )
    .unwrap();
    let mut found: Vec<Finding> = report.entries.into_iter().map(|e| e.finding).collect();
    found.sort_by_key(|f| f.path().to_string());
    assert!(matches!(
        &found[..],
        [
            Finding::StaleExclusion { path: stale, reason: Staleness::NoSuchFile, .. },
            Finding::Violation { path: hit, .. },
        ] if stale == "a/src/graph" && hit == "a/src/graph.rs"
    ));
}

#[test]
fn test_check_exclusion_of_a_clean_file_is_stale() {
    let files = [file("a/src/x.rs", &with_header("fn f() {}\n"))];
    let report = check(
        &files,
        &config("inline-tests = { exclude = [\"a/src/x.rs\"] }"),
        Escalation::None,
    )
    .unwrap();
    assert_eq!(
        report.entries,
        vec![Entry {
            severity: Severity::Error,
            finding: Finding::StaleExclusion {
                rule: Rule::InlineTests,
                path: "a/src/x.rs".to_string(),
                reason: Staleness::NoLongerViolates,
            },
        }]
    );
}

#[test]
fn test_check_stale_exclusion_under_warn_is_still_an_error() {
    let files = [file("a/src/x.rs", &with_header("fn f() {}\n"))];
    let report = check(
        &files,
        &config("inline-tests = { level = \"warn\", exclude = [\"a/src/x.rs\"] }"),
        Escalation::None,
    )
    .unwrap();
    assert_eq!(
        severities(&report),
        vec![(Severity::Error, Rule::InlineTests)]
    );
    assert_eq!(report.exit_code(), 1);
}

#[test]
fn test_check_stale_exclusion_of_an_allowed_rule_is_not_checked() {
    let files = [file("a/src/x.rs", &with_header("fn f() {}\n"))];
    let report = check(
        &files,
        &config("inline-tests = { level = \"allow\", exclude = [\"a/src/gone.rs\"] }"),
        Escalation::None,
    )
    .unwrap();
    assert!(report.entries.is_empty());
}

#[test]
fn test_check_spdx_exclusion_covers_its_file() {
    let files = [file("a/src/x.rs", "fn f() {}\n")];
    let report = check(
        &files,
        &config("spdx-header = { exclude = [\"a/src/x.rs\"] }"),
        Escalation::None,
    )
    .unwrap();
    assert!(report.entries.is_empty());
}

// ── output ──────────────────────────────────────────────────────────────────

#[test]
fn test_entry_display_names_severity_rule_path_and_line() {
    let entry = Entry {
        severity: Severity::Error,
        finding: Finding::Violation {
            rule: Rule::InlineTests,
            path: "a/src/x.rs".to_string(),
            line: 42,
            message: "unit tests inline".to_string(),
        },
    };
    assert_eq!(
        entry.to_string(),
        "error[inline-tests]: a/src/x.rs:42: unit tests inline"
    );
}

#[test]
fn test_entry_display_of_a_warning_says_warning() {
    let entry = Entry {
        severity: Severity::Warning,
        finding: Finding::Violation {
            rule: Rule::SpdxHeader,
            path: "a/src/x.rs".to_string(),
            line: 1,
            message: "missing".to_string(),
        },
    };
    assert!(entry
        .to_string()
        .starts_with("warning[spdx-header]: a/src/x.rs:1: "));
}

#[test]
fn test_entry_display_of_a_stale_exclusion_names_the_path_and_reason() {
    let gone = Entry {
        severity: Severity::Error,
        finding: Finding::StaleExclusion {
            rule: Rule::InlineTests,
            path: "a/src/gone.rs".to_string(),
            reason: Staleness::NoSuchFile,
        },
    };
    assert_eq!(
        gone.to_string(),
        "error[inline-tests]: stale exclusion a/src/gone.rs: no such tracked file"
    );
    let clean = Entry {
        severity: Severity::Error,
        finding: Finding::StaleExclusion {
            rule: Rule::InlineTests,
            path: "a/src/x.rs".to_string(),
            reason: Staleness::NoLongerViolates,
        },
    };
    assert_eq!(
        clean.to_string(),
        "error[inline-tests]: stale exclusion a/src/x.rs: the file no longer violates the rule"
    );
}
