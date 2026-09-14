// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pins the design-token contract of the redesign (#46, sub-issue #111): the
//! charter's tokens live in `docs/design/tokens.css`, `tailwind.css` aliases
//! them into a Tailwind v4 `@theme inline` block, and `assets/main.css` speaks
//! only that vocabulary — no colour literal of its own.
//!
//! Dioxus components are not test-runnable here, so the stylesheets are read
//! with `include_str!` and scanned by hand. The helpers are dependency-free on
//! purpose: a `regex` crate would buy nothing worth a manifest change.

use std::collections::{BTreeMap, BTreeSet};

const MAIN_CSS: &str = include_str!("../assets/main.css");
const TAILWIND_CSS: &str = include_str!("../tailwind.css");
const TOKENS_CSS: &str = include_str!("../../docs/design/tokens.css");
const MAIN_RS: &str = include_str!("../src/main.rs");

const RADII: [&str; 4] = ["--radius-sm", "--radius-md", "--radius-lg", "--radius-full"];

/// The charter's two theme blocks, verbatim (spec section A).
const EXPECTED_THEME_BLOCKS: &str = r#"
:root {
  color-scheme: dark;
  --bg-sunken: oklch(0.132 0.012 265);
  --bg-base: oklch(0.175 0.013 265);
  --bg-raised: oklch(0.222 0.014 265);
  --bg-overlay: oklch(0.262 0.015 265);
  --bg-inset: oklch(0.150 0.012 265);
  --border-subtle: oklch(0.300 0.014 265);
  --border-strong: oklch(0.420 0.016 265);
  --border-focus: oklch(0.760 0.130 258);
  --text-primary: oklch(0.962 0.004 265);
  --text-secondary: oklch(0.762 0.012 265);
  --text-muted: oklch(0.605 0.013 265);
  --text-disabled: oklch(0.450 0.011 265);
  --text-inverse: oklch(0.180 0.012 265);
  --accent: oklch(0.650 0.170 258);
  --accent-hover: oklch(0.712 0.155 258);
  --accent-pressed: oklch(0.578 0.175 258);
  --accent-subtle: oklch(0.300 0.070 258);
  --accent-border: oklch(0.430 0.110 258);
  --text-on-accent: oklch(0.995 0 0);
  --state-ok: oklch(0.735 0.155 152);
  --state-ok-subtle: oklch(0.290 0.062 152);
  --state-degraded: oklch(0.805 0.150 82);
  --state-degraded-subtle: oklch(0.310 0.062 82);
  --state-error: oklch(0.672 0.185 25);
  --state-error-subtle: oklch(0.298 0.075 25);
  --state-unknown: oklch(0.620 0.018 265);
  --state-unknown-subtle: oklch(0.278 0.012 265);
  --shadow-sm: 0 1px 2px oklch(0 0 0 / 0.45);
  --shadow-md: 0 6px 16px -4px oklch(0 0 0 / 0.55);
  --shadow-lg: 0 24px 48px -12px oklch(0 0 0 / 0.65);
  --radius-sm: 6px;
  --radius-md: 10px;
  --radius-lg: 16px;
  --radius-full: 999px;
}
:root[data-theme="light"], body[data-theme="light"] {
  color-scheme: light;
  --bg-sunken: oklch(0.955 0.004 265);
  --bg-base: oklch(0.988 0.003 265);
  --bg-raised: oklch(1 0 0);
  --bg-overlay: oklch(1 0 0);
  --bg-inset: oklch(0.968 0.004 265);
  --border-subtle: oklch(0.906 0.006 265);
  --border-strong: oklch(0.782 0.009 265);
  --border-focus: oklch(0.560 0.170 258);
  --text-primary: oklch(0.225 0.013 265);
  --text-secondary: oklch(0.430 0.013 265);
  --text-muted: oklch(0.552 0.012 265);
  --text-disabled: oklch(0.700 0.008 265);
  --text-inverse: oklch(0.988 0.003 265);
  --accent: oklch(0.548 0.190 258);
  --accent-hover: oklch(0.488 0.195 258);
  --accent-pressed: oklch(0.432 0.180 258);
  --accent-subtle: oklch(0.948 0.030 258);
  --accent-border: oklch(0.860 0.070 258);
  --text-on-accent: oklch(0.995 0 0);
  --state-ok: oklch(0.510 0.135 152);
  --state-ok-subtle: oklch(0.948 0.040 152);
  --state-degraded: oklch(0.578 0.135 72);
  --state-degraded-subtle: oklch(0.958 0.048 82);
  --state-error: oklch(0.522 0.195 25);
  --state-error-subtle: oklch(0.952 0.032 25);
  --state-unknown: oklch(0.552 0.014 265);
  --state-unknown-subtle: oklch(0.938 0.006 265);
  --shadow-sm: 0 1px 2px oklch(0.4 0.02 265 / 0.10);
  --shadow-md: 0 6px 16px -4px oklch(0.4 0.02 265 / 0.14);
  --shadow-lg: 0 24px 48px -12px oklch(0.4 0.02 265 / 0.20);
}
"#;

/// The charter's Tailwind mapping, verbatim (spec section C).
const EXPECTED_THEME_ALIASES: &str = r#"
@theme inline {
  --color-bg-sunken:   var(--bg-sunken);
  --color-bg-base:     var(--bg-base);
  --color-bg-raised:   var(--bg-raised);
  --color-bg-overlay:  var(--bg-overlay);
  --color-bg-inset:    var(--bg-inset);

  --color-border-subtle: var(--border-subtle);
  --color-border-strong: var(--border-strong);

  --color-text-primary:   var(--text-primary);
  --color-text-secondary: var(--text-secondary);
  --color-text-muted:     var(--text-muted);
  --color-text-disabled:  var(--text-disabled);

  --color-accent:         var(--accent);
  --color-accent-hover:   var(--accent-hover);
  --color-accent-subtle:  var(--accent-subtle);
  --color-on-accent:      var(--text-on-accent);

  --color-state-ok:        var(--state-ok);
  --color-state-degraded:  var(--state-degraded);
  --color-state-error:     var(--state-error);
  --color-state-unknown:   var(--state-unknown);

  --radius-md: var(--radius-md);
  --font-sans: "Inter Tight", Helvetica, system-ui, sans-serif;
  --font-mono: "IBM Plex Mono", ui-monospace, monospace;
}
"#;

// ---------------------------------------------------------------------------
// Helpers: a hand-written scanner over `&str`, enough for these stylesheets.
// ---------------------------------------------------------------------------

/// One `selector { body }` pair. `depth` is 0 for a top-level block, 1 for a
/// block nested inside another (a `:root` inside an `@media`, say).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Block {
    selector: String,
    body: String,
    depth: usize,
}

/// Replaces every `/* … */` with a single space so nothing inside a comment
/// is ever scanned, and adjacent tokens do not fuse.
fn strip_comments(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        out.push(' ');
        match rest[start + 2..].find("*/") {
            Some(end) => rest = &rest[start + 2 + end + 2..],
            // An unterminated comment swallows the rest of the sheet, as in CSS.
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// Collapses every whitespace run to one space and trims, so a media query
/// split over two lines compares equal to its one-line form.
fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_ident_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'-' || c == b'_'
}

/// Index of the `}` matching the `{` at `open`, or the sheet's length when
/// the block is unterminated.
fn matching_brace(css: &str, open: usize) -> usize {
    let mut depth = 0usize;
    for (offset, b) in css.as_bytes()[open..].iter().enumerate() {
        match b {
            b'{' => depth += 1,
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return open + offset;
                }
            },
            _ => {},
        }
    }
    css.len()
}

/// Every block of the sheet, nested ones included, in source order. The
/// selector is the text between the previous `{`, `}` or `;` and the `{`.
fn blocks(css: &str) -> Vec<Block> {
    let css = strip_comments(css);
    let bytes = css.as_bytes();
    let mut out = Vec::new();
    let mut selector_start = 0;
    let mut depth = 0usize;
    for (i, b) in bytes.iter().enumerate() {
        match b {
            b'{' => {
                let end = matching_brace(&css, i);
                out.push(Block {
                    selector: collapse_ws(&css[selector_start..i]),
                    body: css[i + 1..end].to_string(),
                    depth,
                });
                depth += 1;
                selector_start = i + 1;
            },
            b'}' => {
                depth = depth.saturating_sub(1);
                selector_start = i + 1;
            },
            b';' => selector_start = i + 1,
            _ => {},
        }
    }
    out
}

/// The first block whose collapsed selector starts with `prefix`. An empty
/// prefix would match every block, so it matches none.
fn block_of(css: &str, prefix: &str) -> Option<Block> {
    if prefix.is_empty() {
        return None;
    }
    blocks(css)
        .into_iter()
        .find(|b| b.selector.starts_with(prefix))
}

/// The body with every nested `{ … }` blanked out, so a declaration inside a
/// nested block is attributed to that block only.
fn top_level(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut depth = 0usize;
    for c in body.chars() {
        match c {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {},
        }
    }
    out
}

/// `name → value` for every top-level declaration of one block body, values
/// whitespace-collapsed. Comments are expected to be stripped already (they
/// are, when the body comes from `blocks`), but stripping again is harmless.
fn declarations(body: &str) -> BTreeMap<String, String> {
    top_level(&strip_comments(body))
        .split(';')
        .filter_map(|decl| decl.split_once(':'))
        .map(|(name, value)| (name.trim().to_string(), collapse_ws(value)))
        .filter(|(name, _)| !name.is_empty())
        .collect()
}

/// The custom-property names (`--x`) declared at the top level of one block.
fn declared_tokens(css_block: &str) -> BTreeSet<String> {
    declarations(css_block)
        .into_keys()
        .filter(|name| name.starts_with("--"))
        .collect()
}

/// Every token declared anywhere in the sheet, at any depth.
fn all_declared_tokens(css: &str) -> BTreeSet<String> {
    blocks(css)
        .iter()
        .flat_map(|b| declared_tokens(&b.body))
        .collect()
}

/// Every `var(--x)` reference, wherever it sits (a `color-mix()` argument, a
/// shorthand, a fallback list).
fn used_tokens(css: &str) -> BTreeSet<String> {
    let css = strip_comments(css);
    let bytes = css.as_bytes();
    let mut out = BTreeSet::new();
    let mut rest = css.as_str();
    let mut base = 0;
    while let Some(pos) = rest.find("var(--") {
        let name_start = base + pos + 4;
        let mut end = name_start;
        while end < bytes.len() && is_ident_char(bytes[end]) {
            end += 1;
        }
        if end > name_start + 2 {
            out.insert(css[name_start..end].to_string());
        }
        base = end;
        rest = &css[base..];
    }
    out
}

/// Every `#hex` (3 to 8 digits), `rgb(` and `rgba(` outside comments and
/// inside a block, as `"<line>: <literal>"`. An id selector such as `#hero`
/// sits outside any block in a flat sheet, so it is not reported.
fn colour_literals(css: &str) -> Vec<String> {
    let css = strip_comments(css);
    let bytes = css.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut line = 1usize;
    let mut i = 0;
    while i < bytes.len() {
        let prev_is_ident = i > 0 && is_ident_char(bytes[i - 1]);
        match bytes[i] {
            b'\n' => line += 1,
            b'{' => depth += 1,
            b'}' => depth = depth.saturating_sub(1),
            b'#' if depth > 0 && !prev_is_ident => {
                let mut end = i + 1;
                while end < bytes.len() && bytes[end].is_ascii_hexdigit() {
                    end += 1;
                }
                let digits = end - (i + 1);
                let bounded = end >= bytes.len() || !is_ident_char(bytes[end]);
                if (3..=8).contains(&digits) && bounded {
                    out.push(format!("{line}: {}", &css[i..end]));
                    i = end;
                    continue;
                }
            },
            b'r' if depth > 0 && !prev_is_ident => {
                if let Some(func) = ["rgba(", "rgb("]
                    .into_iter()
                    .find(|func| css[i..].starts_with(func))
                {
                    out.push(format!("{line}: {func}"));
                    i += func.len();
                    continue;
                }
            },
            _ => {},
        }
        i += 1;
    }
    out
}

/// Byte offset of `needle` in `css` with comments stripped.
fn position_of(css: &str, needle: &str) -> Option<usize> {
    strip_comments(css).find(needle)
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

// Criterion: `main.css` contains no `#hex` colour literal and no `rgb(`/`rgba(`.
#[test]
fn test_main_css_has_no_colour_literal() {
    let offenders = colour_literals(MAIN_CSS);
    assert!(
        offenders.is_empty(),
        "main.css still carries {} colour literal(s):\n{}",
        offenders.len(),
        offenders.join("\n")
    );
}

// Criterion: every `var(--name)` in `main.css` is declared in `tokens.css`.
// A sheet using no token at all would pass `⊆` vacuously, so the left side
// must be non-empty: the empty set is a wildcard, not an edge case.
#[test]
fn test_main_css_uses_only_tokens_that_tokens_css_declares() {
    let used = used_tokens(MAIN_CSS);
    assert!(!used.is_empty(), "main.css uses no var(--…) at all");
    let declared = all_declared_tokens(TOKENS_CSS);
    let undeclared: Vec<_> = used.difference(&declared).cloned().collect();
    assert!(
        undeclared.is_empty(),
        "main.css uses tokens that tokens.css does not declare: {undeclared:?}"
    );
}

// Criterion: every `var(--name)` inside the `@theme inline` block is declared
// in `tokens.css`, and the block is `@theme inline`, not `@theme`.
#[test]
fn test_theme_aliases_point_at_declared_tokens() {
    let theme_blocks: Vec<_> = blocks(TAILWIND_CSS)
        .into_iter()
        .filter(|b| b.selector.starts_with("@theme"))
        .collect();
    assert_eq!(
        theme_blocks.len(),
        1,
        "tailwind.css must hold exactly one @theme block, found {}",
        theme_blocks.len()
    );
    let theme = theme_blocks.first().cloned().unwrap_or_default();
    assert_eq!(
        theme.selector, "@theme inline",
        "the block must be `@theme inline` (a plain `@theme` would inline the \
         resolved colour and break the light/dark switch)"
    );
    let used = used_tokens(&theme.body);
    assert!(!used.is_empty(), "the @theme block references no token");
    let declared = all_declared_tokens(TOKENS_CSS);
    let undeclared: Vec<_> = used.difference(&declared).cloned().collect();
    assert!(
        undeclared.is_empty(),
        "the @theme block aliases tokens that tokens.css does not declare: {undeclared:?}"
    );
}

// Criterion: `tailwind.css` carries the canvas's `@theme inline` block verbatim
// (the 19 `--color-*` aliases, `--radius-md`, `--font-sans`, `--font-mono`).
#[test]
fn test_theme_block_is_the_charters_mapping_verbatim() {
    let expected = block_of(EXPECTED_THEME_ALIASES, "@theme inline").unwrap_or_default();
    let actual = block_of(TAILWIND_CSS, "@theme inline");
    assert!(
        actual.is_some(),
        "tailwind.css has no `@theme inline` block"
    );
    let actual = actual.unwrap_or_default();
    assert_eq!(
        declarations(&actual.body),
        declarations(&expected.body),
        "the @theme inline block differs from the charter's"
    );
    let colour_aliases = declared_tokens(&actual.body)
        .iter()
        .filter(|name| name.starts_with("--color-"))
        .count();
    assert_eq!(colour_aliases, 19, "expected 19 --color-* aliases");
}

// Criterion: the light block declares exactly the dark block's tokens minus
// the four radii, and `light ⊆ dark`. Both sides non-empty.
#[test]
fn test_light_theme_redefines_every_colour_and_shadow_token() {
    let dark = blocks(TOKENS_CSS)
        .into_iter()
        .find(|b| b.selector == ":root" && declarations(&b.body).contains_key("color-scheme"));
    assert!(
        dark.is_some(),
        "tokens.css has no `:root` block declaring color-scheme"
    );
    let light = block_of(
        TOKENS_CSS,
        r#":root[data-theme="light"], body[data-theme="light"]"#,
    );
    assert!(light.is_some(), "tokens.css has no light block");

    let dark = declared_tokens(&dark.unwrap_or_default().body);
    let light = declared_tokens(&light.unwrap_or_default().body);
    assert!(!dark.is_empty(), "the dark block declares no token");
    assert!(!light.is_empty(), "the light block declares no token");

    let radii: BTreeSet<String> = RADII.iter().map(|r| (*r).to_string()).collect();
    let only_dark: BTreeSet<String> = dark.difference(&light).cloned().collect();
    assert_eq!(
        only_dark, radii,
        "dark − light must be exactly the four radii"
    );
    let only_light: Vec<_> = light.difference(&dark).cloned().collect();
    assert!(
        only_light.is_empty(),
        "the light block declares tokens the dark block does not: {only_light:?}"
    );
    assert_eq!(dark.len(), 34, "the dark block must declare 34 tokens");
    assert_eq!(light.len(), 30, "the light block must declare 30 tokens");
}

// Criterion: `tokens.css` declares `:root { color-scheme: dark; … }` and the
// light block with the canvas's values, verbatim.
#[test]
fn test_tokens_css_carries_the_charters_theme_values_verbatim() {
    let expected: Vec<_> = blocks(EXPECTED_THEME_BLOCKS)
        .into_iter()
        .map(|b| (b.selector, declarations(&b.body)))
        .collect();
    assert_eq!(expected.len(), 2, "the fixture holds two theme blocks");

    for (selector, expected_decls) in expected {
        let actual = blocks(TOKENS_CSS)
            .into_iter()
            .find(|b| b.selector == selector && declarations(&b.body).contains_key("color-scheme"));
        assert!(
            actual.is_some(),
            "tokens.css has no `{selector}` block with a color-scheme"
        );
        let actual_decls = declarations(&actual.unwrap_or_default().body);
        assert_eq!(
            actual_decls, expected_decls,
            "the `{selector}` block differs from the charter's"
        );
    }
}

// Criterion: `tokens.css` starts with a comment naming the canvas as the
// visual reference and this file as its text form.
#[test]
fn test_tokens_css_opens_with_the_header_naming_the_canvas() {
    let trimmed = TOKENS_CSS.trim_start();
    assert!(
        trimmed.starts_with("/*"),
        "tokens.css must open with a comment"
    );
    let header_end = trimmed.find("*/").unwrap_or_default();
    let header = &trimmed[..header_end];
    assert!(
        header.contains("canvas"),
        "the header must name the canvas as the reference"
    );
    assert!(
        header.contains("text form"),
        "the header must present this file as its text form"
    );
}

// Criterion: `--tap-min`, `--row-h`, `--pad-screen` at 44px/60px/16px in a
// top-level `:root`, and 32px/40px/24px under
// `@media (min-width: 900px) and (pointer: fine)` (one line or two).
#[test]
fn test_tokens_css_declares_the_density_tokens_and_their_desktop_override() {
    let density = blocks(TOKENS_CSS).into_iter().find(|b| {
        b.selector == ":root" && b.depth == 0 && declarations(&b.body).contains_key("--tap-min")
    });
    assert!(
        density.is_some(),
        "tokens.css has no top-level `:root` declaring --tap-min"
    );
    let density = declarations(&density.unwrap_or_default().body);
    let expected: BTreeMap<String, String> = [
        ("--tap-min", "44px"),
        ("--row-h", "60px"),
        ("--pad-screen", "16px"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    assert_eq!(density, expected, "the mobile density block differs");

    let media = block_of(TOKENS_CSS, "@media (min-width: 900px) and (pointer: fine)");
    assert!(media.is_some(), "tokens.css has no desktop @media override");
    let inner = block_of(&media.unwrap_or_default().body, ":root");
    assert!(
        inner.is_some(),
        "the desktop @media override has no `:root` block"
    );
    let desktop = declarations(&inner.unwrap_or_default().body);
    let expected: BTreeMap<String, String> = [
        ("--tap-min", "32px"),
        ("--row-h", "40px"),
        ("--pad-screen", "24px"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    assert_eq!(desktop, expected, "the desktop density override differs");
}

// Criterion: `tailwind.css` is `@import "tailwindcss";` then
// `@import "../docs/design/tokens.css";`, both before `@theme`.
#[test]
fn test_tailwind_css_imports_tailwind_then_the_tokens() {
    let tailwind = position_of(TAILWIND_CSS, "@import \"tailwindcss\";");
    let tokens = position_of(TAILWIND_CSS, "@import \"../docs/design/tokens.css\";");
    let theme = position_of(TAILWIND_CSS, "@theme");
    assert!(tailwind.is_some(), "tailwind.css must import tailwindcss");
    assert!(
        tokens.is_some(),
        "tailwind.css must import ../docs/design/tokens.css"
    );
    assert!(theme.is_some(), "tailwind.css must declare a @theme block");
    let (tailwind, tokens, theme) = (
        tailwind.unwrap_or_default(),
        tokens.unwrap_or_default(),
        theme.unwrap_or_default(),
    );
    assert!(
        tailwind < tokens,
        "tailwindcss must be imported before the tokens"
    );
    assert!(
        tokens < theme,
        "the tokens must be imported before the @theme block"
    );
}

// Criterion: no change under `src/` — `main.rs` keeps linking both sheets.
#[test]
fn test_main_rs_keeps_linking_both_stylesheets() {
    assert!(
        MAIN_RS.contains(r#"asset!("/assets/main.css")"#),
        "main.rs must keep linking main.css"
    );
    assert!(
        MAIN_RS.contains(r#"asset!("/assets/tailwind.css")"#),
        "main.rs must keep linking the generated tailwind.css"
    );
}

// Test strategy: the helpers themselves, on a small fixture — a commented-out
// literal is not reported, a `var()` nested in `color-mix()` is found, a
// declaration inside a nested `@media` belongs to that block only.
#[test]
fn test_the_helpers_strip_comments_and_find_every_var() {
    let fixture = "\
/* #123456 rgb(1, 2, 3) var(--commented) */
.a { color: color-mix(in oklch, var(--a) 60%, var(--b)); background: #fff; }
#hero { margin: 0; border-color: rgba(0, 0, 0, 0.5); }
:root { --x: 1px; @media (min-width: 1px) { --y: 2px; } }
";

    assert_eq!(
        colour_literals(fixture),
        vec!["2: #fff".to_string(), "3: rgba(".to_string()],
        "comments are skipped, id selectors are not colours, the rest is found"
    );

    let used = used_tokens(fixture);
    let expected: BTreeSet<String> = ["--a", "--b"].iter().map(|s| (*s).to_string()).collect();
    assert_eq!(
        used, expected,
        "var() inside color-mix() is found, commented ones are not"
    );

    let root = block_of(fixture, ":root");
    assert!(root.is_some());
    assert_eq!(
        declared_tokens(&root.unwrap_or_default().body),
        ["--x"].iter().map(|s| (*s).to_string()).collect(),
        "a nested block's declarations are not the parent's"
    );
    let media = block_of(fixture, "@media (min-width: 1px)");
    assert!(media.is_some());
    let media = media.unwrap_or_default();
    assert_eq!(media.depth, 1, "the @media is nested inside :root");
    assert_eq!(
        declared_tokens(&media.body),
        ["--y"].iter().map(|s| (*s).to_string()).collect(),
        "a --x inside a nested @media belongs to that block"
    );

    assert!(
        block_of(fixture, "").is_none(),
        "an empty prefix matches nothing"
    );
    let two_line = "@media (min-width: 900px) and\n       (pointer: fine) { :root { --z: 1; } }";
    assert!(
        block_of(two_line, "@media (min-width: 900px) and (pointer: fine)").is_some(),
        "a media query split over two lines is matched"
    );
}
