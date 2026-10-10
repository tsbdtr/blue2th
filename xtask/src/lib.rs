// SPDX-License-Identifier: MIT OR Apache-2.0
//! Repository conventions, checked by `cargo check-conventions` (#180).

use std::fmt;

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

pub fn parse_config(_manifest: &str) -> Result<Config, ConfigError> {
    Ok(Config::default())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Escalation {
    None,
    DenyWarnings,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageError(pub String);

pub fn parse_args(_args: &[String]) -> Result<Escalation, UsageError> {
    Ok(Escalation::None)
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

pub fn spdx_header(_path: &str, _content: &str) -> Option<Finding> {
    None
}

pub fn inline_tests(_path: &str, _content: &str) -> Result<Vec<Finding>, ParseError> {
    Ok(Vec::new())
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
    fn fmt(&self, _f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub entries: Vec<Entry>,
}

impl Report {
    pub fn exit_code(&self) -> i32 {
        0
    }
}

pub fn check(
    _files: &[SourceFile],
    _config: &Config,
    _escalation: Escalation,
) -> Result<Report, ParseError> {
    Ok(Report::default())
}

#[cfg(test)]
mod tests;
