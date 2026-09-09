#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# pr-type-label.sh <title>: print the label a pull request gets from the
# Conventional Commits type of its title.
#
# The type-to-label table lives here and nowhere else: the workflow
# (.github/workflows/pr-title.yml) and the release notes (.github/release.yml)
# both read it through this script, so the two cannot disagree on what a
# `feat:` is called. The title is checked against the `PATTERN` line of
# .githooks/commit-msg, read from the hook rather than copied, so the type set
# the hook accepts is the type set this script knows (#82).
#
# An empty title is refused on its own, before the pattern: every prefix
# matches "", and an empty label handed to `gh pr edit --add-label` is not a
# no-op — it is an error the workflow would have to explain.
#
# Exit codes: 0 label printed, 1 refused title, 2 hook or its PATTERN line
# missing, 3 valid title whose type has no label.
set -euo pipefail

usage() {
    echo "usage: $(basename "$0") <title> | --all"
    echo "  <title>  a Conventional Commits pull-request title; prints its label"
    echo "  --all    prints every label this script can assign, one per line"
}

# Fixed order: the workflow subtracts the label it adds and hands the rest to
# `--remove-label`, so the list is a contract, not a set.
declare -a LABELS=(enhancement bug documentation ci refactor)

label_for() {
    case "$1" in
        feat) echo enhancement ;;
        fix) echo bug ;;
        docs) echo documentation ;;
        ci) echo ci ;;
        refactor) echo refactor ;;
        *) return 1 ;;
    esac
}

if [[ "${1:-}" == "-h" ]]; then
    usage
    exit 0
fi
if [[ "${1:-}" == "--all" ]]; then
    printf '%s\n' "${LABELS[@]}"
    exit 0
fi
if [[ $# -ne 1 ]]; then
    usage >&2
    exit 1
fi

title="$1"

if [[ -z "$title" ]]; then
    echo "pr-type-label.sh: empty title" >&2
    exit 1
fi

hook="$(dirname "$0")/../.githooks/commit-msg"
if [[ ! -f "$hook" ]]; then
    echo "pr-type-label.sh: hook not found at $hook" >&2
    exit 2
fi
# The same sed expression pr-title.yml uses to read the hook.
pattern=$(sed -n "s/^PATTERN='\(.*\)'$/\1/p" "$hook")
if [[ -z "$pattern" ]]; then
    echo "pr-type-label.sh: could not read PATTERN from $hook" >&2
    exit 2
fi

if ! printf '%s' "$title" | grep -qE "$pattern"; then
    echo "pr-type-label.sh: '$title' does not follow Conventional Commits" >&2
    exit 1
fi

# The pattern already guarantees a leading lower-case type; this only cuts it
# off before the scope, the marker and the separator.
type=$(printf '%s' "$title" | sed -E 's/^([a-z]+).*$/\1/')

if ! label=$(label_for "$type"); then
    exit 3
fi
echo "$label"
