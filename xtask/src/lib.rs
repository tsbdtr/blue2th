// SPDX-License-Identifier: MIT OR Apache-2.0
//! Repository conventions, checked by `cargo check-conventions` (#180).

use std::collections::HashSet;
use std::fmt;

use syn::punctuated::Punctuated;
use syn::visit::Visit;
use syn::Token;

pub const SPDX_HEADER: &str = "// SPDX-License-Identifier: MIT OR Apache-2.0";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    InlineTests,
    SpdxHeader,
}

impl Rule {
    pub const ALL: [Rule; 2] = [Rule::InlineTests, Rule::SpdxHeader];

    pub fn name(self) -> &'static str {
        match self {
            Rule::InlineTests => "inline-tests",
            Rule::SpdxHeader => "spdx-header",
        }
    }

    pub fn from_name(name: &str) -> Option<Rule> {
        Rule::ALL.into_iter().find(|rule| rule.name() == name)
    }

    fn findings(self, file: &SourceFile) -> Result<Vec<Finding>, ParseError> {
        match self {
            Rule::InlineTests => inline_tests(&file.path, &file.content),
            Rule::SpdxHeader => Ok(spdx_header(&file.path, &file.content).into_iter().collect()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Allow,
    Warn,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleConfig {
    pub level: Level,
    pub exclude: Vec<String>,
}

impl Default for RuleConfig {
    fn default() -> Self {
        RuleConfig {
            level: Level::Deny,
            exclude: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    inline_tests: RuleConfig,
    spdx_header: RuleConfig,
}

impl Config {
    pub fn rule(&self, rule: Rule) -> &RuleConfig {
        match rule {
            Rule::InlineTests => &self.inline_tests,
            Rule::SpdxHeader => &self.spdx_header,
        }
    }

    fn rule_mut(&mut self, rule: Rule) -> &mut RuleConfig {
        match rule {
            Rule::InlineTests => &mut self.inline_tests,
            Rule::SpdxHeader => &mut self.spdx_header,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// The manifest is not valid TOML.
    Toml(String),
    /// `[workspace.metadata.conventions]` is present but not a table.
    NotATable,
    UnknownRule(String),
    /// A rule's value is neither a level string nor a table.
    InvalidRule {
        rule: String,
    },
    UnknownLevel {
        rule: String,
        value: String,
    },
    UnknownKey {
        rule: String,
        key: String,
    },
    /// `exclude` is not an array of strings.
    InvalidExclude {
        rule: String,
    },
    EmptyExclude {
        rule: String,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Toml(error) => write!(f, "Cargo.toml is not valid TOML: {error}"),
            ConfigError::NotATable => {
                write!(f, "[workspace.metadata.conventions] must be a table")
            }
            ConfigError::UnknownRule(name) => {
                let known: Vec<&str> = Rule::ALL.iter().map(|rule| rule.name()).collect();
                write!(
                    f,
                    "unknown rule `{name}`; the rules are: {}",
                    known.join(", ")
                )
            }
            ConfigError::InvalidRule { rule } => write!(
                f,
                "`{rule}` takes a level (\"allow\", \"warn\", \"deny\") or a table {{ level, exclude }}"
            ),
            ConfigError::UnknownLevel { rule, value } => write!(
                f,
                "`{rule}`: unknown level {value:?}; the levels are allow, warn, deny"
            ),
            ConfigError::UnknownKey { rule, key } => write!(
                f,
                "`{rule}`: unknown key `{key}`; the keys are level, exclude"
            ),
            ConfigError::InvalidExclude { rule } => {
                write!(f, "`{rule}`: exclude must be an array of paths")
            }
            ConfigError::EmptyExclude { rule } => {
                write!(f, "`{rule}`: an empty exclude entry would match every file")
            }
        }
    }
}

pub fn parse_config(manifest: &str) -> Result<Config, ConfigError> {
    let root: toml::Table =
        toml::from_str(manifest).map_err(|e| ConfigError::Toml(e.to_string()))?;
    let mut config = Config::default();
    let Some(conventions) = root
        .get("workspace")
        .and_then(|workspace| workspace.get("metadata"))
        .and_then(|metadata| metadata.get("conventions"))
    else {
        return Ok(config);
    };
    let conventions = conventions.as_table().ok_or(ConfigError::NotATable)?;
    for (name, value) in conventions {
        let rule = Rule::from_name(name).ok_or_else(|| ConfigError::UnknownRule(name.clone()))?;
        *config.rule_mut(rule) = parse_rule(name, value)?;
    }
    Ok(config)
}

fn parse_rule(rule: &str, value: &toml::Value) -> Result<RuleConfig, ConfigError> {
    match value {
        toml::Value::String(level) => Ok(RuleConfig {
            level: parse_level(rule, level)?,
            exclude: Vec::new(),
        }),
        toml::Value::Table(table) => {
            let mut config = RuleConfig::default();
            for (key, value) in table {
                match key.as_str() {
                    "level" => {
                        config.level = match value.as_str() {
                            Some(level) => parse_level(rule, level)?,
                            None => {
                                return Err(ConfigError::UnknownLevel {
                                    rule: rule.to_string(),
                                    value: value.to_string(),
                                })
                            },
                        }
                    },
                    "exclude" => config.exclude = parse_exclude(rule, value)?,
                    _ => {
                        return Err(ConfigError::UnknownKey {
                            rule: rule.to_string(),
                            key: key.clone(),
                        })
                    },
                }
            }
            Ok(config)
        },
        _ => Err(ConfigError::InvalidRule {
            rule: rule.to_string(),
        }),
    }
}

fn parse_level(rule: &str, value: &str) -> Result<Level, ConfigError> {
    match value {
        "allow" => Ok(Level::Allow),
        "warn" => Ok(Level::Warn),
        "deny" => Ok(Level::Deny),
        _ => Err(ConfigError::UnknownLevel {
            rule: rule.to_string(),
            value: value.to_string(),
        }),
    }
}

/// An empty entry is refused here rather than left to the matcher: matched by
/// prefix anywhere downstream, `""` would exclude every file.
fn parse_exclude(rule: &str, value: &toml::Value) -> Result<Vec<String>, ConfigError> {
    let invalid = || ConfigError::InvalidExclude {
        rule: rule.to_string(),
    };
    let entries = value.as_array().ok_or_else(invalid)?;
    let mut exclude = Vec::with_capacity(entries.len());
    for entry in entries {
        let path = entry.as_str().ok_or_else(invalid)?;
        if path.is_empty() {
            return Err(ConfigError::EmptyExclude {
                rule: rule.to_string(),
            });
        }
        exclude.push(path.to_string());
    }
    Ok(exclude)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Escalation {
    None,
    DenyWarnings,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageError(pub String);

pub fn parse_args(args: &[String]) -> Result<Escalation, UsageError> {
    let mut escalation = Escalation::None;
    let mut args = args.iter().map(String::as_str);
    while let Some(arg) = args.next() {
        match arg {
            "-D" => match args.next() {
                Some("warnings") => escalation = Escalation::DenyWarnings,
                Some(other) => {
                    return Err(UsageError(format!(
                        "-D accepts only `warnings`, not `{other}`"
                    )))
                },
                None => return Err(UsageError("-D needs `warnings`".to_string())),
            },
            other => return Err(UsageError(format!("unknown argument `{other}`"))),
        }
    }
    Ok(escalation)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFile {
    /// Repository-relative, as `git ls-files` prints it.
    pub path: String,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Staleness {
    NoSuchFile,
    NoLongerViolates,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finding {
    Violation {
        rule: Rule,
        path: String,
        /// 1-based.
        line: usize,
        message: String,
    },
    StaleExclusion {
        rule: Rule,
        path: String,
        reason: Staleness,
    },
}

impl Finding {
    pub fn rule(&self) -> Rule {
        match self {
            Finding::Violation { rule, .. } | Finding::StaleExclusion { rule, .. } => *rule,
        }
    }

    pub fn path(&self) -> &str {
        match self {
            Finding::Violation { path, .. } | Finding::StaleExclusion { path, .. } => path,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub path: String,
    pub message: String,
}

/// Split on `\n` alone, not `lines()`: a CRLF file must fail, as it does
/// under the `head -1 | grep -qxF` this rule replaces.
pub fn spdx_header(path: &str, content: &str) -> Option<Finding> {
    let first_line = content.split('\n').next().unwrap_or_default();
    (first_line != SPDX_HEADER).then(|| Finding::Violation {
        rule: Rule::SpdxHeader,
        path: path.to_string(),
        line: 1,
        message: format!("the first line must be exactly `{SPDX_HEADER}`"),
    })
}

pub fn inline_tests(path: &str, content: &str) -> Result<Vec<Finding>, ParseError> {
    if !is_under_src(path) {
        return Ok(Vec::new());
    }
    let file = syn::parse_file(content).map_err(|e| {
        let at = e.span().start();
        ParseError {
            path: path.to_string(),
            message: format!("{}:{}: {e}", at.line, at.column + 1),
        }
    })?;
    let mut modules = InlineTestModules {
        path,
        findings: Vec::new(),
    };
    modules.visit_file(&file);
    Ok(modules.findings)
}

/// A `src` directory anywhere above the file, matched as a whole component so
/// that `my_src/` is not one.
fn is_under_src(path: &str) -> bool {
    let mut components: Vec<&str> = path.split('/').collect();
    components.pop();
    components.contains(&"src")
}

struct InlineTestModules<'a> {
    path: &'a str,
    findings: Vec<Finding>,
}

impl<'ast> Visit<'ast> for InlineTestModules<'_> {
    fn visit_item_mod(&mut self, module: &'ast syn::ItemMod) {
        if let Some((_, items)) = &module.content {
            if module.attrs.iter().any(enables_test) && holds_test_fn(items) {
                self.findings.push(Finding::Violation {
                    rule: Rule::InlineTests,
                    path: self.path.to_string(),
                    line: module.mod_token.span.start().line,
                    message: format!(
                        "unit tests inline in module `{}` — move them to a sibling file (#172)",
                        module.ident
                    ),
                });
            }
        }
        syn::visit::visit_item_mod(self, module);
    }
}

/// `cfg(test)`, or `test` under `all`/`any`; never under `not`.
fn enables_test(attr: &syn::Attribute) -> bool {
    attr.path().is_ident("cfg")
        && attr
            .parse_args::<syn::Meta>()
            .is_ok_and(|predicate| predicate_enables_test(&predicate))
}

fn predicate_enables_test(predicate: &syn::Meta) -> bool {
    match predicate {
        syn::Meta::Path(path) => path.is_ident("test"),
        syn::Meta::List(list) if list.path.is_ident("all") || list.path.is_ident("any") => list
            .parse_args_with(Punctuated::<syn::Meta, Token![,]>::parse_terminated)
            .is_ok_and(|inner| inner.iter().any(predicate_enables_test)),
        _ => false,
    }
}

/// What tells a test module from test-support code such as `graph::fake`.
fn holds_test_fn(items: &[syn::Item]) -> bool {
    let mut finder = TestFnFinder { found: false };
    for item in items {
        finder.visit_item(item);
    }
    finder.found
}

struct TestFnFinder {
    found: bool,
}

impl<'ast> Visit<'ast> for TestFnFinder {
    fn visit_item_fn(&mut self, function: &'ast syn::ItemFn) {
        if function.attrs.iter().any(is_test_attr) {
            self.found = true;
        }
        syn::visit::visit_item_fn(self, function);
    }
}

/// `#[test]`, `#[tokio::test]`, `#[tokio::test(flavor = …)]`.
fn is_test_attr(attr: &syn::Attribute) -> bool {
    attr.path()
        .segments
        .last()
        .is_some_and(|segment| segment.ident == "test")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub severity: Severity,
    pub finding: Finding,
}

impl fmt::Display for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let severity = match self.severity {
            Severity::Warning => "warning",
            Severity::Error => "error",
        };
        let rule = self.finding.rule().name();
        match &self.finding {
            Finding::Violation {
                path,
                line,
                message,
                ..
            } => write!(f, "{severity}[{rule}]: {path}:{line}: {message}"),
            Finding::StaleExclusion { path, reason, .. } => {
                let why = match reason {
                    Staleness::NoSuchFile => "no such tracked file",
                    Staleness::NoLongerViolates => "the file no longer violates the rule",
                };
                write!(f, "{severity}[{rule}]: stale exclusion {path}: {why}")
            },
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub entries: Vec<Entry>,
}

impl Report {
    pub fn exit_code(&self) -> i32 {
        if self
            .entries
            .iter()
            .any(|entry| entry.severity == Severity::Error)
        {
            1
        } else {
            0
        }
    }
}

pub fn check(
    files: &[SourceFile],
    config: &Config,
    escalation: Escalation,
) -> Result<Report, ParseError> {
    let mut entries = Vec::new();
    for rule in Rule::ALL {
        let rule_config = config.rule(rule);
        let severity = match (rule_config.level, escalation) {
            (Level::Allow, _) => continue,
            (Level::Warn, Escalation::None) => Severity::Warning,
            (Level::Warn, Escalation::DenyWarnings) | (Level::Deny, _) => Severity::Error,
        };
        let mut excluded_and_violating = HashSet::new();
        for file in files {
            let findings = rule.findings(file)?;
            if rule_config.exclude.contains(&file.path) {
                if !findings.is_empty() {
                    excluded_and_violating.insert(file.path.as_str());
                }
                continue;
            }
            entries.extend(
                findings
                    .into_iter()
                    .map(|finding| Entry { severity, finding }),
            );
        }
        for path in &rule_config.exclude {
            let reason = if !files.iter().any(|file| &file.path == path) {
                Staleness::NoSuchFile
            } else if !excluded_and_violating.contains(path.as_str()) {
                Staleness::NoLongerViolates
            } else {
                continue;
            };
            // A stale exclusion is a defect of the config, not a finding the
            // level was set for: an `allow`ed rule never gets this far.
            entries.push(Entry {
                severity: Severity::Error,
                finding: Finding::StaleExclusion {
                    rule,
                    // The report outlives the borrowed config it came from.
                    path: path.clone(),
                    reason,
                },
            });
        }
    }
    Ok(Report { entries })
}

#[cfg(test)]
mod tests;
