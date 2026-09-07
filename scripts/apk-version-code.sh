#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# apk-version-code.sh <apk> | --parse: print the versionCode an APK declares.
#
# `<apk>` runs `aapt dump badging` on the file; `--parse` reads that output on
# stdin, so the parser is testable without an APK. The value comes back from
# the built artifact itself, not from the Gradle file the workflow rewrote:
# the point is to catch a rewrite that Gradle ignored, and only the APK can
# say what it carries.
#
# Exit codes: 0 printed, 1 refused (no file, no attribute, empty or
# non-numeric value), 2 aapt missing.
set -euo pipefail

usage() {
    echo "usage: $(basename "$0") <apk>" >&2
    echo "       $(basename "$0") --parse  < <output of 'aapt dump badging'>" >&2
}

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib/build-tools.sh
source "$here/lib/build-tools.sh"

# parse_badging: stdin to versionCode on stdout, or exit 1.
parse_badging() {
    local package_line value
    package_line="$(grep -m1 '^package:' || true)"
    if [[ -z "$package_line" ]]; then
        echo "apk-version-code.sh: no 'package:' line in the badging output" >&2
        return 1
    fi
    if [[ ! "$package_line" =~ versionCode=\'([^\']*)\' ]]; then
        echo "apk-version-code.sh: no versionCode attribute on the package line" >&2
        return 1
    fi
    value="${BASH_REMATCH[1]}"
    if [[ ! "$value" =~ ^[1-9][0-9]*$ ]]; then
        echo "apk-version-code.sh: versionCode '$value' is not a positive integer" >&2
        return 1
    fi
    echo "$value"
}

if [[ "${1:-}" == "-h" ]]; then
    usage
    exit 0
fi
if [[ $# -ne 1 ]]; then
    usage
    exit 1
fi

if [[ "$1" == "--parse" ]]; then
    parse_badging
    exit $?
fi

apk="$1"
if [[ ! -f "$apk" ]]; then
    echo "apk-version-code.sh: APK not found: '$apk'" >&2
    exit 1
fi

aapt="$(locate_build_tool aapt)" || exit 2

if ! badging="$("$aapt" dump badging "$apk" 2>/dev/null)"; then
    echo "apk-version-code.sh: aapt could not read '$apk'" >&2
    exit 1
fi

parse_badging <<<"$badging"
