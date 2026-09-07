#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Contract of scripts/set-version-code.sh <build.gradle.kts> <code>: rewrite the
# `versionCode = 1` line dx emits to the given code, touching nothing else;
# exit 1 and leave the file byte-identical when that line is absent or when the
# code is not a positive integer.
# Sourced by scripts/tests/run.sh; each test_* runs in its own subshell.

script="$SCRIPTS_DIR/set-version-code.sh"

# The shape of target/dx/blue2th-frontend/<profile>/android/app/app/build.gradle.kts
# as dx 0.7 generates it, distractor values included: `= 34`, `= 24`, a quoted
# `"0.1.0"` and `= true` must all survive the rewrite untouched.
write_gradle_fixture() {
    cat >"$1" <<'EOF'
plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace="io.github.tsbdtr.blue2th"
    compileSdk = 34
    defaultConfig {
        applicationId = "io.github.tsbdtr.blue2th"
        minSdk = 24
        targetSdk = 34
        versionCode = 1
        versionName = "0.1.0"
    }
    buildTypes {
        getByName("debug") {
            isDebuggable = true
            isJniDebuggable = true
        }
    }
}
EOF
}

# Criterion: rewrites the `versionCode = 1` line to the given code — the
# whole file is compared, so anything else changing fails the test.
test_set_version_code_rewrites_only_the_version_code_line() {
    write_gradle_fixture "$tmp/build.gradle.kts"
    write_gradle_fixture "$tmp/expected.gradle.kts"
    sed -i 's/^        versionCode = 1$/        versionCode = 1000/' "$tmp/expected.gradle.kts"

    assert_succeeds "$script" "$tmp/build.gradle.kts" 1000

    assert_eq "$(cat "$tmp/expected.gradle.kts")" "$(cat "$tmp/build.gradle.kts")" \
        "build.gradle.kts after rewrite"
    assert_contains "$(cat "$tmp/build.gradle.kts")" "        versionCode = 1000" \
        "rewritten line keeps its indentation"
}

# Criterion: the largest code the tag formula can produce is written as is —
# no truncation, no scientific notation.
test_set_version_code_writes_a_nine_digit_code() {
    write_gradle_fixture "$tmp/build.gradle.kts"
    assert_succeeds "$script" "$tmp/build.gradle.kts" 999999999
    assert_contains "$(cat "$tmp/build.gradle.kts")" "versionCode = 999999999" \
        "nine-digit code written verbatim"
}

# Criterion: a `versionCode = 1` that is not a statement on its own line (here
# inside a comment) is not the line to rewrite; only the statement changes.
test_set_version_code_leaves_a_commented_decoy_alone() {
    write_gradle_fixture "$tmp/build.gradle.kts"
    sed -i 's|^    defaultConfig {$|    // dx writes versionCode = 1; the release workflow rewrites it\n    defaultConfig {|' \
        "$tmp/build.gradle.kts"
    cp "$tmp/build.gradle.kts" "$tmp/expected.gradle.kts"
    sed -i 's/^        versionCode = 1$/        versionCode = 1000/' "$tmp/expected.gradle.kts"

    assert_succeeds "$script" "$tmp/build.gradle.kts" 1000

    assert_eq "$(cat "$tmp/expected.gradle.kts")" "$(cat "$tmp/build.gradle.kts")" \
        "build.gradle.kts after rewrite with a commented decoy"
}

# Criterion: when the line is absent (a future dx rephrased it), exit 1 and
# leave the file untouched — the job must fail at this step, not on the phone
# with an APK that still says versionCode 1.
test_set_version_code_refuses_when_line_is_absent_and_leaves_file_untouched() {
    write_gradle_fixture "$tmp/build.gradle.kts"
    sed -i '/^        versionCode = 1$/d' "$tmp/build.gradle.kts"
    local before
    before="$(sha256sum "$tmp/build.gradle.kts")"

    assert_fails "$script" "$tmp/build.gradle.kts" 1000

    assert_contains "$stderr" "versionCode" "refusal names what was not found"
    assert_eq "$before" "$(sha256sum "$tmp/build.gradle.kts")" "file unchanged after refusal"
}

# Criterion: a code that is not a positive integer is refused and the file is
# byte-identical afterwards. `0` is below Android's minimum, `-1` and `abc`
# are not codes, `1.5` is not an integer, and the empty string is passed
# explicitly so a missing validation cannot let it through as "no change".
test_set_version_code_refuses_non_positive_integer_codes_and_leaves_file_untouched() {
    write_gradle_fixture "$tmp/build.gradle.kts"
    local before
    before="$(sha256sum "$tmp/build.gradle.kts")"

    local code
    for code in 0 -1 abc 1.5 "" "10 00" 1e3; do
        assert_fails "$script" "$tmp/build.gradle.kts" "$code"
        assert_eq "$before" "$(sha256sum "$tmp/build.gradle.kts")" \
            "file unchanged after refusing code '$code'"
    done
}

# Criterion: a missing code argument is refused too — distinct from an empty
# one, and just as likely from a workflow expansion gone wrong.
test_set_version_code_refuses_missing_code_argument() {
    write_gradle_fixture "$tmp/build.gradle.kts"
    local before
    before="$(sha256sum "$tmp/build.gradle.kts")"

    assert_fails "$script" "$tmp/build.gradle.kts"

    assert_eq "$before" "$(sha256sum "$tmp/build.gradle.kts")" "file unchanged after refusal"
}

# Criterion: a Gradle file that does not exist is refused, naming its path —
# the dx output layout moving is the likeliest way this happens.
test_set_version_code_refuses_missing_file() {
    assert_fails "$script" "$tmp/nope/build.gradle.kts" 1000
    assert_contains "$stderr" "$tmp/nope/build.gradle.kts" "refusal names the path"
    assert_file_absent "$tmp/nope/build.gradle.kts"
}
