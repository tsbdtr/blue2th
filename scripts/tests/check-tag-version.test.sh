#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Contract of scripts/check-tag-version.sh <tag> <Cargo.toml>: exit 0 when the
# tag is `v` + the `[workspace.package] version` of that manifest, exit 1
# otherwise, with a message naming both values. Only that table is read.
# Sourced by scripts/tests/run.sh; each test_* runs in its own subshell.

script="$SCRIPTS_DIR/check-tag-version.sh"

# Mirrors the real root manifest: a `[workspace.package]` table followed by
# `[workspace.dependencies]` whose inline tables carry their own `version`.
write_root_like_manifest() {
    cat >"$1" <<'EOF'
[workspace]
members = ["blue2th-frontend", "blue2th-proto", "blue2th-server"]
resolver = "2"

[workspace.package]
version = "0.1.0"
license = "MIT OR Apache-2.0"
publish = false

[workspace.dependencies]
serde = { version = "1", features = ["derive"] }
tokio = { version = "1" }
EOF
}

# Criterion: exit 0 when the tag equals `v` + `[workspace.package] version`.
test_check_tag_version_accepts_matching_tag() {
    write_root_like_manifest "$tmp/Cargo.toml"
    assert_succeeds "$script" v0.1.0 "$tmp/Cargo.toml"
}

# Criterion: non-zero on a mismatch, and the error names both values — the
# maintainer who tagged `v0.2.0` on a `0.1.0` tree learns which one to fix.
test_check_tag_version_refuses_mismatch_naming_both_values() {
    write_root_like_manifest "$tmp/Cargo.toml"
    assert_fails "$script" v0.2.0 "$tmp/Cargo.toml"
    assert_contains "$stderr" "v0.2.0" "mismatch message names the tag"
    assert_contains "$stderr" "0.1.0" "mismatch message names the manifest version"
}

# Criterion: a `version = "1"` under `[workspace.dependencies]` is never the
# one compared. With the inline form (the real manifest's shape), `v1` and
# `v1.0.0` must both be refused.
test_check_tag_version_ignores_inline_dependency_version() {
    write_root_like_manifest "$tmp/Cargo.toml"
    assert_fails "$script" v1 "$tmp/Cargo.toml"
    assert_fails "$script" v1.0.0 "$tmp/Cargo.toml"
}

# Criterion: the same decoy on its own `version = ...` line — the shape a
# line-oriented grep would match first — is ignored whether it sits before
# or after the package table.
test_check_tag_version_ignores_dependency_table_version_line() {
    cat >"$tmp/Cargo.toml" <<'EOF'
[workspace]
members = ["a"]

[workspace.dependencies.before]
version = "9.9.9"

[workspace.package]
version = "0.1.0"

[workspace.dependencies.after]
version = "1.0.0"
features = ["derive"]
EOF
    assert_succeeds "$script" v0.1.0 "$tmp/Cargo.toml"
    assert_fails "$script" v9.9.9 "$tmp/Cargo.toml"
    assert_fails "$script" v1.0.0 "$tmp/Cargo.toml"
}

# Criterion: a manifest with no `[workspace.package] version` is an error,
# not a match — a `[package] version` at the root is not the workspace's.
test_check_tag_version_refuses_manifest_without_workspace_package_version() {
    cat >"$tmp/Cargo.toml" <<'EOF'
[package]
name = "blue2th"
version = "0.1.0"

[dependencies]
serde = { version = "1" }
EOF
    assert_fails "$script" v0.1.0 "$tmp/Cargo.toml"
    assert_contains "$stderr" "workspace.package" "error names the missing table"
}

# Criterion: a `[workspace.package]` table that carries no `version` key is
# the same error; the key in the next table must not be picked up.
test_check_tag_version_refuses_workspace_package_without_version_key() {
    cat >"$tmp/Cargo.toml" <<'EOF'
[workspace.package]
license = "MIT OR Apache-2.0"

[workspace.dependencies.serde]
version = "0.1.0"
EOF
    assert_fails "$script" v0.1.0 "$tmp/Cargo.toml"
}

# Criterion: an empty tag is refused — passed explicitly, because an empty
# string compares equal to an empty extraction and would fail open.
test_check_tag_version_refuses_empty_tag() {
    write_root_like_manifest "$tmp/Cargo.toml"
    assert_fails "$script" "" "$tmp/Cargo.toml"
}

# Criterion: the tag must be `v` + version, so the bare version is refused
# even though it equals the manifest's value.
test_check_tag_version_refuses_tag_without_v_prefix() {
    write_root_like_manifest "$tmp/Cargo.toml"
    assert_fails "$script" 0.1.0 "$tmp/Cargo.toml"
}

# Criterion: a manifest that does not exist is refused, naming its path.
test_check_tag_version_refuses_missing_manifest() {
    assert_fails "$script" v0.1.0 "$tmp/does-not-exist/Cargo.toml"
    assert_contains "$stderr" "$tmp/does-not-exist/Cargo.toml" "error names the path"
}

# Criterion: missing arguments are refused; the workflow calling it with one
# argument is a wiring bug that must not read as "versions agree".
test_check_tag_version_refuses_missing_arguments() {
    write_root_like_manifest "$tmp/Cargo.toml"
    assert_fails "$script" v0.1.0
    assert_fails "$script"
}
