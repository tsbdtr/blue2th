#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# pr-type-label.sh <title>: print the label a pull request gets from the
# Conventional Commits type of its title.
#
# The type-to-label table lives here and nowhere else: the workflow
# (.github/workflows/pr-title.yml) adds what this prints and removes what
# `--all` prints, and .github/release.yml groups the release notes by the
# same label names — a test in scripts/tests/pr-type-label.test.sh holds that
# file to this table. The title is checked against the `PATTERN` line of
# .githooks/commit-msg, read from the hook rather than copied, so the type set
# the hook accepts is the type set this script knows (#82).
#
# An empty title is refused on its own, before the pattern, so that refusal
# never depends on how the hook's regex happens to be anchored: this script
# follows whatever PATTERN the hook beside it carries, and a pattern that let
# "" through would hand the workflow an empty label, which `gh pr edit
# --add-label` rejects rather than ignores.
#
# Exit codes: 0 label printed, 1 refused title, 2 hook or its PATTERN line
# missing, 3 valid title whose type has no label.
set -euo pipefail

usage() {
    echo "usage: $(basename "$0") <title> | --all"
    echo "  <title>  a Conventional Commits pull-request title; prints its label"
    echo "  --all    prints every label this script can assign, one per line"
}

# The table, as `type=label` pairs. Its order is the order `--all` prints:
# the workflow subtracts the label it adds and hands the rest to
# `--remove-label`, so the list is a contract, not a set.
declare -a TYPE_LABELS=(
    feat=enhancement
    fix=bug
    docs=documentation
    ci=ci
    refactor=refactor
)

label_for() {
    local entry
    for entry in "${TYPE_LABELS[@]}"; do
        if [[ "${entry%%=*}" == "$1" ]]; then
            echo "${entry#*=}"
            return 0
        fi
    done
    return 1
}

if [[ "${1:-}" == "-h" ]]; then
    usage
    exit 0
fi
if [[ "${1:-}" == "--all" ]]; then
    printf '%s\n' "${TYPE_LABELS[@]#*=}"
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

# A title is one line. `grep` judges lines, not strings: a second line would
# pass the check on its own and the type would be cut from a line the check
# never looked at. The hook has no such case — it reads `head -1` of a commit
# message, where the second line is the body.
if [[ "$title" == *$'\n'* ]]; then
    echo "pr-type-label.sh: title spans more than one line" >&2
    exit 1
fi

hook="$(dirname "$0")/../.githooks/commit-msg"
if [[ ! -f "$hook" ]]; then
    echo "pr-type-label.sh: hook not found at $hook" >&2
    exit 2
fi
# The same sed expression pr-title.yml uses to read the hook; a test compares
# the two, character for character.
pattern=$(sed -n "s/^PATTERN='\(.*\)'$/\1/p" "$hook")
if [[ -z "$pattern" ]]; then
    echo "pr-type-label.sh: could not read PATTERN from $hook" >&2
    exit 2
fi

if ! printf '%s' "$title" | grep -qE "$pattern"; then
    echo "pr-type-label.sh: '$title' does not follow Conventional Commits" >&2
    exit 1
fi

# The hook's pattern opens with the type, so the cut stops at the first
# character that cannot belong to one — the scope's `(`, the marker `!` or
# the separator `:`. A hook whose pattern does not open that way yields no
# type, and no type has no label.
type=""
if [[ "$title" =~ ^([a-z]+) ]]; then
    type="${BASH_REMATCH[1]}"
fi

if [[ -z "$type" ]] || ! label=$(label_for "$type"); then
    exit 3
fi
echo "$label"
