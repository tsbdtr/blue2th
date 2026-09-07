#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Runs every scripts/tests/*.test.sh and exits non-zero if any test fails.
#
# A test file defines functions named test_*; each one runs in its own subshell
# with a fresh temporary directory in $tmp, removed when the test ends. The
# assertion helpers below abort the test on the first failure, so a test body
# reads top to bottom as a claim about the script it exercises.
#
# Exit codes: 0 all tests passed, 1 at least one failed, 2 a required tool is
# missing. The tool check comes first and fails loudly: a run that skipped the
# signing tests because apksigner was absent would look green while proving
# nothing about the one script that handles the release key.
set -euo pipefail

TESTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPTS_DIR="$(dirname "$TESTS_DIR")"
REPO_ROOT="$(dirname "$SCRIPTS_DIR")"
FIXTURES_DIR="$TESTS_DIR/fixtures"
export TESTS_DIR SCRIPTS_DIR REPO_ROOT FIXTURES_DIR

# ── Tools ────────────────────────────────────────────────────────────────────

# apksigner, zipalign and aapt live in a versioned build-tools directory that
# is not on PATH; the scripts under test resolve them the same way, so the
# runner exports the two variables it resolved with and both sides agree.
BUILD_TOOLS_VERSION="${BUILD_TOOLS_VERSION:-36.0.0}"
export BUILD_TOOLS_VERSION

locate_build_tool() {
    local name="$1"
    if [[ -n "${ANDROID_HOME:-}" ]]; then
        local candidate="$ANDROID_HOME/build-tools/$BUILD_TOOLS_VERSION/$name"
        if [[ -x "$candidate" ]]; then
            echo "$candidate"
            return 0
        fi
        echo "scripts/tests/run.sh: $name not found at $candidate" >&2
        echo "  (ANDROID_HOME=$ANDROID_HOME, BUILD_TOOLS_VERSION=$BUILD_TOOLS_VERSION)" >&2
        return 2
    fi
    if command -v "$name"; then
        return 0
    fi
    echo "scripts/tests/run.sh: $name not found on PATH and ANDROID_HOME is unset" >&2
    return 2
}

if ! KEYTOOL="$(command -v keytool)"; then
    echo "scripts/tests/run.sh: keytool not found on PATH (a JDK is required)" >&2
    exit 2
fi
APKSIGNER="$(locate_build_tool apksigner)" || exit 2
ZIPALIGN="$(locate_build_tool zipalign)" || exit 2
AAPT="$(locate_build_tool aapt)" || exit 2
export KEYTOOL APKSIGNER ZIPALIGN AAPT

# ── Assertion helpers ────────────────────────────────────────────────────────

# Aborts the current test. Every helper ends up here, and the test subshell's
# non-zero exit is what the runner records as a failure.
fail() {
    echo "assertion failed: $*" >&2
    exit 1
}

# Runs a command and captures its exit status, stdout and stderr into the
# globals $status, $stdout and $stderr, without letting `set -e` abort the test
# on a non-zero exit — a refused input is often the expected outcome.
run() {
    set +e
    stdout="$("$@" 2>"$tmp/.run-stderr")"
    status=$?
    set -e
    stderr="$(cat "$tmp/.run-stderr")"
    rm -f "$tmp/.run-stderr"
}

# assert_eq <expected> <actual> [label]
assert_eq() {
    local expected="$1" actual="$2" label="${3:-value}"
    [[ "$expected" == "$actual" ]] \
        || fail "$label: expected '$expected', got '$actual'"
}

# assert_succeeds <command...>: exit status 0. $stdout/$stderr stay available.
assert_succeeds() {
    run "$@"
    [[ "$status" -eq 0 ]] \
        || fail "expected success (exit 0), got exit $status for: $*"$'\n'"stderr: $stderr"
}

# assert_fails <command...>: exit status exactly 1, the scripts' code for a
# refused input. Anything else is not a refusal: 2 is a missing tool and 127 a
# missing script — both would be mistaken for "refused" by a bare non-zero
# check, and a script that does not exist yet must not pass its own tests.
assert_fails() {
    run "$@"
    [[ "$status" -eq 1 ]] \
        || fail "expected a refusal (exit 1), got exit $status for: $*"$'\n'"stdout: $stdout"$'\n'"stderr: $stderr"
}

# assert_contains <haystack> <needle> [label]. The needle must be non-empty:
# every string contains "", so an empty needle would make the check vacuous.
assert_contains() {
    local haystack="$1" needle="$2" label="${3:-text}"
    [[ -n "$needle" ]] || fail "$label: assert_contains called with an empty needle"
    [[ "$haystack" == *"$needle"* ]] \
        || fail "$label: expected to contain '$needle', got: $haystack"
}

assert_file_exists() {
    [[ -e "$1" ]] || fail "expected file to exist: $1"
}

assert_file_absent() {
    [[ ! -e "$1" ]] || fail "expected no file at: $1"
}

# assert_dir_empty <dir>: no entry at all, hidden ones included. A decoded
# keystore left behind would typically have a mktemp-style name, which is not
# hidden, but the guard costs nothing.
assert_dir_empty() {
    local dir="$1"
    [[ -d "$dir" ]] || fail "expected a directory at: $dir"
    local leftovers
    leftovers="$(find "$dir" -mindepth 1 | sort)"
    [[ -z "$leftovers" ]] || fail "expected $dir to be empty, found:"$'\n'"$leftovers"
}

# ── Fixtures ─────────────────────────────────────────────────────────────────

# make_unsigned_apk <path>: an APK as ./gradlew assembleRelease would hand to
# sign-apk.sh — unsigned, and not yet aligned. The manifest is *stored* (-0)
# on purpose: zipalign only checks uncompressed entries, and a deflated one
# reports "aligned" whatever its offset, which would make the alignment
# assertion vacuous. Stored behind a 30-byte header and a 19-byte name, the
# entry starts at offset 49, so `zipalign -c 4` refuses the input until the
# script under test has aligned it.
make_unsigned_apk() {
    local out="$1"
    local staging
    staging="$(mktemp -d "$tmp/staging.XXXXXX")"
    cp "$FIXTURES_DIR/AndroidManifest.xml" "$staging/AndroidManifest.xml"
    (cd "$staging" && zip -q -0 -X "$out" AndroidManifest.xml)
    rm -rf "$staging"
}

# ── Runner ───────────────────────────────────────────────────────────────────

run_test() {
    local name="$1" log="$2"
    (
        tmp="$(mktemp -d)"
        # shellcheck disable=SC2064  # $tmp is meant to expand now, not at exit
        trap "rm -rf '$tmp'" EXIT
        cd "$tmp"
        "$name"
    ) >"$log" 2>&1
}

passed=0
failed=0
declare -a failures=()

shopt -s nullglob
test_files=("$TESTS_DIR"/*.test.sh)
shopt -u nullglob
if [[ ${#test_files[@]} -eq 0 ]]; then
    echo "scripts/tests/run.sh: no *.test.sh found under $TESTS_DIR" >&2
    exit 1
fi

runner_log="$(mktemp)"
# shellcheck disable=SC2064
trap "rm -f '$runner_log'" EXIT

for file in "${test_files[@]}"; do
    echo "== $(basename "$file")"
    # Tests are listed in definition order, so a file reads like its report.
    mapfile -t names < <(grep -oE '^test_[A-Za-z0-9_]+\(\)' "$file" | sed 's/()$//')
    if [[ ${#names[@]} -eq 0 ]]; then
        echo "FAIL $(basename "$file"): defines no test_* function"
        failed=$((failed + 1))
        failures+=("$(basename "$file")")
        continue
    fi
    (
        # shellcheck disable=SC1090  # the file is one of ours, discovered above
        source "$file"
        for name in "${names[@]}"; do
            if run_test "$name" "$runner_log"; then
                echo "PASS $name"
                echo "PASS" >>"$runner_log.status"
            else
                echo "FAIL $name"
                sed 's/^/     /' "$runner_log"
                echo "FAIL $name" >>"$runner_log.status"
            fi
        done
    )
done

# The per-file loop runs in a subshell, so counters come back through a file.
if [[ -f "$runner_log.status" ]]; then
    passed="$(grep -c '^PASS$' "$runner_log.status" || true)"
    mapfile -t failures < <(grep '^FAIL ' "$runner_log.status" | sed 's/^FAIL //')
    failed=$((failed + ${#failures[@]}))
    rm -f "$runner_log.status"
fi

echo
echo "$passed passed, $failed failed"
if [[ "$failed" -ne 0 ]]; then
    printf '  %s\n' "${failures[@]}"
    exit 1
fi
