#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# check-workflow-permissions.sh [dir]: fail when a workflow under `dir`
# (default `.github/workflows`) leaves any job on the default GITHUB_TOKEN
# permissions — no column-0 `permissions:` key and at least one job without
# its own. Only bash, grep and awk: the runner has no yq.
#
# Exit codes: 0 every job is scoped, 1 at least one job is not (or a file has
# no `jobs:` key), 2 `dir` does not exist.
set -euo pipefail

# RED-phase stub: accepts everything. The behaviour is pinned by
# scripts/tests/workflow-permissions.test.sh and arrives in the GREEN phase.
exit 0
