#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# version-code.sh <tag>: derive the Android versionCode from a `vX.Y.Z` tag.
#
# The formula, X*1000000 + Y*1000 + Z, is the one the upstream dx fix uses
# (DioxusLabs/dioxus#5735), so the value computed here is the value dx computes
# once the pin moves past it, and every code published before stays ordered
# with those published after. The strict shape is what keeps the mapping
# injective: `v01.0.0` and `v1.0.0` would be one number under two tags, and a
# component above 999 would land on the next major's code. Both are refused.
#
# Exit codes: 0 printed, 1 refused tag.
set -euo pipefail

usage() {
    echo "usage: $(basename "$0") <tag>" >&2
    echo "  <tag> is a strict vX.Y.Z (each component 0..=999); prints X*1000000+Y*1000+Z" >&2
}

if [[ "${1:-}" == "-h" ]]; then
    usage
    exit 0
fi
if [[ $# -ne 1 ]]; then
    usage
    exit 1
fi

tag="$1"

# The empty string is the wildcard case (CLAUDE.md): checked on its own so a
# regex that failed open could never let it through.
if [[ -z "$tag" ]]; then
    echo "version-code.sh: empty tag" >&2
    exit 1
fi

if [[ ! "$tag" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
    echo "version-code.sh: '$tag' is not a strict vX.Y.Z tag" >&2
    exit 1
fi

major="${BASH_REMATCH[1]}"
minor="${BASH_REMATCH[2]}"
patch="${BASH_REMATCH[3]}"

for component in "$major" "$minor" "$patch"; do
    if (( component > 999 )); then
        echo "version-code.sh: '$tag' has a component above 999" >&2
        exit 1
    fi
done

code=$(( major * 1000000 + minor * 1000 + patch ))

if (( code == 0 )); then
    echo "version-code.sh: '$tag' maps to versionCode 0, below Android's minimum of 1" >&2
    exit 1
fi

echo "$code"
