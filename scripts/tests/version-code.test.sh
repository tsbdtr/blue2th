#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Contract of scripts/version-code.sh: a strict `vX.Y.Z` tag becomes the
# Android versionCode `X*1000000 + Y*1000 + Z`, printed alone on stdout.
# Sourced by scripts/tests/run.sh; each test_* runs in its own subshell.

script="$SCRIPTS_DIR/version-code.sh"

# Criterion: `v0.1.0` → `1000`.
test_version_code_maps_v0_1_0_to_1000() {
    assert_succeeds "$script" v0.1.0
    assert_eq 1000 "$stdout" "versionCode of v0.1.0"
}

# Criterion: `v1.2.3` → `1002003`.
test_version_code_maps_v1_2_3_to_1002003() {
    assert_succeeds "$script" v1.2.3
    assert_eq 1002003 "$stdout" "versionCode of v1.2.3"
}

# Criterion: `v0.0.1` → `1`, the smallest versionCode Android accepts.
test_version_code_maps_v0_0_1_to_1() {
    assert_succeeds "$script" v0.0.1
    assert_eq 1 "$stdout" "versionCode of v0.0.1"
}

# Criterion: components go up to 999, so the largest tag encodes without
# collision and stays below Android's 2100000000 ceiling.
test_version_code_maps_v999_999_999_to_999999999() {
    assert_succeeds "$script" v999.999.999
    assert_eq 999999999 "$stdout" "versionCode of v999.999.999"
}

# Criterion: a middle component of exactly 999 is the boundary that must
# still pass — `v0.999.0` and `v1.0.0` are distinct codes.
test_version_code_keeps_v0_999_0_below_v1_0_0() {
    assert_succeeds "$script" v0.999.0
    assert_eq 999000 "$stdout" "versionCode of v0.999.0"
}

# Criterion: refuses `0.1.0` (no `v` prefix).
test_version_code_refuses_tag_without_v_prefix() {
    assert_fails "$script" 0.1.0
    assert_eq "" "$stdout" "stdout on refusal"
}

# Criterion: refuses `v0.1` (two components).
test_version_code_refuses_two_component_tag() {
    assert_fails "$script" v0.1
    assert_eq "" "$stdout" "stdout on refusal"
}

# Criterion: refuses `v0.1.0-rc1` — the formula cannot encode a pre-release.
test_version_code_refuses_prerelease_tag() {
    assert_fails "$script" v0.1.0-rc1
    assert_eq "" "$stdout" "stdout on refusal"
}

# Criterion: refuses `v0.1.0.1` (four components).
test_version_code_refuses_four_component_tag() {
    assert_fails "$script" v0.1.0.1
    assert_eq "" "$stdout" "stdout on refusal"
}

# Criterion: refuses `v01.0.0` — a leading zero is not a canonical version,
# and two spellings of one number must not be two tags.
test_version_code_refuses_leading_zero_component() {
    assert_fails "$script" v01.0.0
    assert_eq "" "$stdout" "stdout on refusal"
}

# Criterion: refuses `v0.0.0` — versionCode must be at least 1.
test_version_code_refuses_v0_0_0() {
    assert_fails "$script" v0.0.0
    assert_eq "" "$stdout" "stdout on refusal"
}

# Criterion: refuses `v0.1000.0` — it would collide with `v1.0.0`.
test_version_code_refuses_component_above_999() {
    assert_fails "$script" v0.1000.0
    assert_eq "" "$stdout" "stdout on refusal"
    assert_fails "$script" v1000.0.0
    assert_fails "$script" v0.0.1000
}

# Criterion: refuses an empty argument. The empty string is passed
# explicitly: a pattern match on "" fails open, and this is the case that
# proves the script does not.
test_version_code_refuses_empty_tag() {
    assert_fails "$script" ""
    assert_eq "" "$stdout" "stdout on refusal"
}

# Criterion: refuses a missing argument as well — the workflow passing
# nothing is a different bug from passing an empty expansion.
test_version_code_refuses_missing_argument() {
    assert_fails "$script"
    assert_eq "" "$stdout" "stdout on refusal"
}

# Criterion: the refusal is explained on stderr, naming the offending tag,
# so a failed workflow step reads without opening the script.
test_version_code_names_the_refused_tag_on_stderr() {
    assert_fails "$script" v0.1.0-rc1
    assert_contains "$stderr" "v0.1.0-rc1" "refusal message"
}
