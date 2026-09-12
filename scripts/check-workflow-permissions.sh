#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# check-workflow-permissions.sh [dir]: fail when a workflow under `dir`
# (default `.github/workflows`) leaves any job on the default GITHUB_TOKEN
# permissions — no column-0 `permissions:` key and at least one job without
# its own. Only bash, grep and awk: the runner has no yq.
#
# The parse is deliberately shallow, and it is what keeps decoys out: a job
# is a key at exactly two spaces under a column-0 `jobs:` line (until the
# next column-0 key), and a job's own block is `permissions:` at exactly four
# spaces before the next job. Comments are dropped before any match, and a
# `permissions:` deeper than four spaces — a step, a `run: |` body — never
# matches by construction. A column-0 `permissions:` anywhere in the file
# covers every job, so offending jobs are held until the end of the file
# rather than reported as they are met.
#
# Exit codes: 0 every job is scoped, 1 at least one job is not (or a file has
# no `jobs:` key), 2 `dir` does not exist.
set -euo pipefail

dir="${1:-.github/workflows}"

if [[ ! -d "$dir" ]]; then
    echo "check-workflow-permissions.sh: no such directory: $dir" >&2
    exit 2
fi

shopt -s nullglob
files=("$dir"/*.yml "$dir"/*.yaml)
shopt -u nullglob

# check_file <file>: prints one line per unscoped job (or `no jobs found`) on
# stderr and returns 1 when it printed anything, 0 otherwise.
check_file() {
    local file="$1"
    awk -v file="$file" '
        # A job with no block of its own is a finding only if the file has no
        # column-0 block either; the file may declare that after `jobs:`.
        function close_job() {
            if (job != "" && !has_block) {
                missing[n_missing++] = job
            }
            job = ""
            has_block = 0
        }
        /^[[:space:]]*#/ { next }
        /^permissions:/ { top_level = 1 }
        /^jobs:/ { in_jobs = 1; next }
        /^[^[:space:]]/ { close_job(); in_jobs = 0 }
        in_jobs && /^  [^[:space:]]/ {
            close_job()
            job = $0
            sub(/^  /, "", job)
            sub(/:.*$/, "", job)
            if (job != "") {
                n_jobs++
            }
            next
        }
        in_jobs && job != "" && /^    permissions:/ { has_block = 1 }
        END {
            close_job()
            if (n_jobs == 0) {
                print file ": no jobs found" > "/dev/stderr"
                exit 1
            }
            if (top_level) {
                exit 0
            }
            for (i = 0; i < n_missing; i++) {
                print file ": job " missing[i] " has no permissions block" > "/dev/stderr"
            }
            exit n_missing > 0 ? 1 : 0
        }
    ' "$file"
}

status=0
for file in "${files[@]}"; do
    check_file "$file" || status=1
done
exit "$status"
