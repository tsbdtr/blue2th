#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# workspace-version.sh <Cargo.toml>: print the `[workspace.package] version`.
#
# Only that table is read. The root manifest also carries `version = "1"` in
# `[workspace.dependencies]` inline tables, and a dependency declared in its
# own `[workspace.dependencies.<name>]` table puts `version = ...` on a line
# of its own: a line-oriented grep would take whichever comes first. The
# table is tracked instead; it opens at its header and closes at the next one.
#
# Exit codes: 0 printed, 1 refused (missing manifest, table, key or value).
set -euo pipefail

usage() {
    echo "usage: $(basename "$0") <Cargo.toml>" >&2
    echo "  prints the version declared under [workspace.package]" >&2
}

if [[ "${1:-}" == "-h" ]]; then
    usage
    exit 0
fi
if [[ $# -ne 1 ]]; then
    usage
    exit 1
fi

manifest="$1"

if [[ ! -f "$manifest" ]]; then
    echo "workspace-version.sh: manifest not found: '$manifest'" >&2
    exit 1
fi

# Comment lines are skipped before anything else: a commented-out
# `# version = "9.9.9"` inside the table is not a declaration.
version="$(awk '
    /^[[:space:]]*#/ { next }
    /^[[:space:]]*\[workspace\.package\][[:space:]]*$/ { in_table = 1; next }
    /^[[:space:]]*\[/ { in_table = 0; next }
    in_table && /^[[:space:]]*version[[:space:]]*=/ {
        line = $0
        sub(/^[[:space:]]*version[[:space:]]*=[[:space:]]*"/, "", line)
        sub(/".*$/, "", line)
        print line
        exit
    }
' "$manifest")"

# An absent key and an empty value both come out as "", and "" is what the
# comparison downstream must never see: it would equal an empty tag.
if [[ -z "$version" ]]; then
    echo "workspace-version.sh: no [workspace.package] version in '$manifest'" >&2
    exit 1
fi

echo "$version"
