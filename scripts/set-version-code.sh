#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# set-version-code.sh <build.gradle.kts> <code>: rewrite the `versionCode = 1`
# line dx emits in its generated Gradle project to the given code.
#
# dx hardcodes `versionCode = 1` in its template (docs/PUBLISHING.md, section
# 03); nothing in Dioxus.toml reaches it. A `sed` that finds nothing is a
# silent no-op, and a `versionCode = 1` APK builds, signs and verifies
# perfectly: it fails on the phone, on the second release, as "cannot install
# over the existing app". So the line has to be found exactly once, and the
# file is not touched when it is not.
#
# Exit codes: 0 rewritten, 1 refused (file, code or line).
set -euo pipefail

usage() {
    echo "usage: $(basename "$0") <build.gradle.kts> <code>" >&2
    echo "  rewrites the single 'versionCode = 1' statement to 'versionCode = <code>'" >&2
}

if [[ "${1:-}" == "-h" ]]; then
    usage
    exit 0
fi
if [[ $# -ne 2 ]]; then
    usage
    exit 1
fi

file="$1"
code="$2"

if [[ ! -f "$file" ]]; then
    echo "set-version-code.sh: file not found: '$file'" >&2
    exit 1
fi

if [[ ! "$code" =~ ^[1-9][0-9]*$ ]]; then
    echo "set-version-code.sh: '$code' is not a positive integer versionCode" >&2
    exit 1
fi

# A statement on its own line only: the same text inside a comment is not
# the value Gradle reads, and rewriting it would hide that the real one
# was missing.
pattern='^[[:space:]]*versionCode = 1[[:space:]]*$'
matches="$(grep -c -E "$pattern" "$file" || true)"

if [[ "$matches" -ne 1 ]]; then
    echo "set-version-code.sh: expected exactly one 'versionCode = 1' line in '$file', found $matches; the file is left untouched" >&2
    exit 1
fi

sed -i -E "s/^([[:space:]]*)versionCode = 1([[:space:]]*)$/\1versionCode = ${code}\2/" "$file"
