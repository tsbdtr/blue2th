#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Contract of scripts/lib/build-tools.sh, the lookup every release script and
# the runner itself source: `locate_build_tool <name>` prints the binary's
# path, or explains on stderr and returns 2 — the exit code the scripts pass
# on, distinct from 1 (a refused input), so a missing SDK is never mistaken
# for a bad tag or a wrong password.
# Sourced by scripts/tests/run.sh; each test_* runs in its own subshell.

# Criterion: under the runner's own ANDROID_HOME the three tools the release
# needs resolve to executables — the same ones the runner exported.
test_build_tools_resolves_the_three_release_tools() {
    local name path
    for name in apksigner zipalign aapt; do
        path="$(locate_build_tool "$name")" || fail "could not locate $name"
        [[ -x "$path" ]] || fail "$name resolved to a non-executable: $path"
    done
}

# Criterion: an SDK without the pinned build-tools is reported with the path
# that was tried and the two variables that made it, and returns 2.
test_build_tools_names_the_missing_candidate_and_returns_2() {
    run env "ANDROID_HOME=$tmp/no-sdk" bash -c \
        'source "$SCRIPTS_DIR/lib/build-tools.sh" && locate_build_tool apksigner'
    assert_eq 2 "$status" "exit status without build-tools"
    assert_eq "" "$stdout" "stdout on a missing tool"
    assert_contains "$stderr" "$tmp/no-sdk/build-tools/$BUILD_TOOLS_VERSION/apksigner" \
        "message names the path tried"
    assert_contains "$stderr" "BUILD_TOOLS_VERSION=$BUILD_TOOLS_VERSION" "message names the version"
}

# Criterion: an empty tool name is refused. The build-tools directory itself
# passes `-x`, so "" would otherwise resolve to a directory and be run.
test_build_tools_refuses_an_empty_tool_name() {
    run bash -c 'source "$SCRIPTS_DIR/lib/build-tools.sh" && locate_build_tool ""'
    assert_eq 2 "$status" "exit status with an empty name"
    assert_eq "" "$stdout" "stdout with an empty name"
    assert_contains "$stderr" "without a tool name" "message says what was missing"
}

# Criterion: sign-apk.sh exits 2, not 1, when the SDK is missing — after its
# input checks passed, so a wrong secret and a missing SDK are told apart —
# and creates no output.
test_build_tools_missing_sdk_makes_sign_apk_exit_2() {
    : >"$tmp/unsigned.apk"
    : >"$tmp/keystore.p12"
    run env "ANDROID_HOME=$tmp/no-sdk" "ANDROID_KEYSTORE_FILE=$tmp/keystore.p12" \
        ANDROID_KEYSTORE_PASSWORD=x ANDROID_KEY_ALIAS=x ANDROID_KEY_PASSWORD=x \
        "$SCRIPTS_DIR/sign-apk.sh" "$tmp/unsigned.apk" "$tmp/signed.apk"
    assert_eq 2 "$status" "exit status without build-tools"
    assert_contains "$stderr" "zipalign" "message names the first tool it looked for"
    assert_file_absent "$tmp/signed.apk"
}

# Criterion: the runner refuses to start without build-tools, exit 2, naming
# the tool — never a green run that skipped the signing tests.
test_build_tools_missing_sdk_makes_run_sh_refuse_to_start() {
    run env "ANDROID_HOME=$tmp/no-sdk" "$TESTS_DIR/run.sh"
    assert_eq 2 "$status" "runner exit status without build-tools"
    assert_contains "$stderr" "apksigner" "runner names the missing tool"
    [[ "$stdout" != *"passed,"* ]] || fail "runner must not reach the summary without build-tools"
}
