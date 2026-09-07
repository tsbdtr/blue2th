#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Contract of scripts/workspace-version.sh <Cargo.toml>: print the
# `[workspace.package] version` alone on stdout; exit 1 when the manifest,
# the table, the key or the value is missing. Only that table is read. It is
# the reader check-tag-version.sh builds on, and the one the release workflow
# runs on a workflow_dispatch, where there is no tag to compare against.
# Sourced by scripts/tests/run.sh; each test_* runs in its own subshell.

script="$SCRIPTS_DIR/workspace-version.sh"

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

# Criterion: prints the `[workspace.package] version`, nothing else.
test_workspace_version_prints_the_workspace_package_version() {
    write_root_like_manifest "$tmp/Cargo.toml"
    assert_succeeds "$script" "$tmp/Cargo.toml"
    assert_eq "0.1.0" "$stdout" "workspace version"
}

# Criterion: the real root manifest is read the same way — the one the
# workflow hands over on a dispatch.
test_workspace_version_reads_the_repository_manifest() {
    assert_succeeds "$script" "$REPO_ROOT/Cargo.toml"
    [[ "$stdout" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] \
        || fail "expected a X.Y.Z version from the root manifest, got '$stdout'"
}

# Criterion: a `version = ...` line in a dependency table, before or after
# the package table, is never the one printed.
test_workspace_version_ignores_dependency_table_version_lines() {
    cat >"$tmp/Cargo.toml" <<'EOF'
[workspace.dependencies.before]
version = "9.9.9"

[workspace.package]
version = "0.1.0"

[workspace.dependencies.after]
version = "1.0.0"
EOF
    assert_succeeds "$script" "$tmp/Cargo.toml"
    assert_eq "0.1.0" "$stdout" "workspace version with decoys around"
}

# Criterion: a commented-out `# version = ...` inside the table is not a
# declaration.
test_workspace_version_ignores_a_commented_version_line() {
    cat >"$tmp/Cargo.toml" <<'EOF'
[workspace.package]
# version = "9.9.9"
version = "0.2.0"
EOF
    assert_succeeds "$script" "$tmp/Cargo.toml"
    assert_eq "0.2.0" "$stdout" "workspace version past a commented decoy"
}

# Criterion: no `[workspace.package]` table is an error naming it, not a
# fallback to `[package] version`.
test_workspace_version_refuses_manifest_without_workspace_package() {
    cat >"$tmp/Cargo.toml" <<'EOF'
[package]
name = "blue2th"
version = "0.1.0"
EOF
    assert_fails "$script" "$tmp/Cargo.toml"
    assert_eq "" "$stdout" "stdout on refusal"
    assert_contains "$stderr" "workspace.package" "error names the missing table"
}

# Criterion: a table without the key is the same error; the key in the next
# table is not picked up.
test_workspace_version_refuses_table_without_version_key() {
    cat >"$tmp/Cargo.toml" <<'EOF'
[workspace.package]
license = "MIT OR Apache-2.0"

[workspace.dependencies.serde]
version = "0.1.0"
EOF
    assert_fails "$script" "$tmp/Cargo.toml"
    assert_eq "" "$stdout" "stdout on refusal"
}

# Criterion: an empty value is refused — printing "" would let the workflow
# tag `v` and compare equal to an empty extraction elsewhere.
test_workspace_version_refuses_empty_version_value() {
    cat >"$tmp/Cargo.toml" <<'EOF'
[workspace.package]
version = ""
EOF
    assert_fails "$script" "$tmp/Cargo.toml"
    assert_eq "" "$stdout" "stdout on refusal"
}

# Criterion: a manifest that does not exist is refused, naming its path.
test_workspace_version_refuses_missing_manifest() {
    assert_fails "$script" "$tmp/does-not-exist/Cargo.toml"
    assert_contains "$stderr" "$tmp/does-not-exist/Cargo.toml" "error names the path"
}

# Criterion: no argument is refused — the script must not read a default.
test_workspace_version_refuses_missing_argument() {
    assert_fails "$script"
}
