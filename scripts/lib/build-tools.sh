#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Sourced, never run: defines `locate_build_tool <name>`, which prints the
# path of an Android build-tools binary (apksigner, zipalign, aapt) on stdout,
# or explains on stderr and returns 2.
#
# Those binaries sit in a versioned directory under ANDROID_HOME that is not
# on PATH. The lookup is shared between the release scripts and
# scripts/tests/run.sh so the runner exercises the very binary the scripts
# resolve: three private copies had already stopped printing the same hint
# when they failed.
#
# BUILD_TOOLS_VERSION selects the directory. The default is the version the
# workflows install with sdkmanager (.github/workflows/ci.yml, release.yml).

BUILD_TOOLS_VERSION="${BUILD_TOOLS_VERSION:-36.0.0}"

locate_build_tool() {
    local name="$1"
    local caller
    caller="$(basename "$0")"
    # An empty name would resolve to the build-tools directory itself, which
    # passes `-x`: the empty value is refused rather than treated as a match.
    if [[ -z "$name" ]]; then
        echo "$caller: locate_build_tool called without a tool name" >&2
        return 2
    fi
    if [[ -n "${ANDROID_HOME:-}" ]]; then
        local candidate="$ANDROID_HOME/build-tools/$BUILD_TOOLS_VERSION/$name"
        if [[ -x "$candidate" ]]; then
            echo "$candidate"
            return 0
        fi
        echo "$caller: $name not found at $candidate" >&2
        echo "  (ANDROID_HOME=$ANDROID_HOME, BUILD_TOOLS_VERSION=$BUILD_TOOLS_VERSION)" >&2
        return 2
    fi
    if command -v "$name"; then
        return 0
    fi
    echo "$caller: $name not found on PATH and ANDROID_HOME is unset" >&2
    return 2
}
