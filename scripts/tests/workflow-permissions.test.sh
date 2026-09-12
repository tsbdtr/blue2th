#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Contract of scripts/check-workflow-permissions.sh [dir] (#126): every
# `*.yml`/`*.yaml` under `dir` (default `.github/workflows`) must scope its
# GITHUB_TOKEN — a column-0 `permissions:` key, or one on every job at the
# job's indentation + 2. Exit 0 when they all do; exit 1 with
# `<file>: job <name> has no permissions block` per offending job, or
# `<file>: no jobs found` for a file without a `jobs:` key; exit 2 when `dir`
# does not exist. A `permissions:` in a comment or inside a step is not a
# block and must not rescue a job.
# Sourced by scripts/tests/run.sh; each test_* runs in its own subshell.

script="$SCRIPTS_DIR/check-workflow-permissions.sh"

# workflows_dir <name>: a fresh directory under $tmp for one test's fixtures,
# printed on stdout so the caller can point the script at it.
workflows_dir() {
    local dir="$tmp/$1"
    mkdir -p "$dir"
    echo "$dir"
}

# Criterion: a top-level `permissions:` block covers every job, so a workflow
# whose jobs declare none passes — the shape ci.yml takes after this change.
test_top_level_permissions_block_passes() {
    dir="$(workflows_dir top-level)"
    cat >"$dir/ci.yml" <<'YAML'
name: CI

on:
  pull_request:

concurrency:
  group: ci-${{ github.ref }}
  cancel-in-progress: true

permissions:
  contents: read

jobs:
  quality:
    name: fmt, clippy, test
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v7
      - run: cargo test --workspace

  deny:
    name: cargo deny
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v7
YAML
    assert_succeeds "$script" "$dir"
    assert_eq "" "$stdout" "stdout when every job is scoped"
}

# Criterion: no top-level block, but every job carries its own at the job's
# indentation + 2 — the shape of release.yml and pr-title.yml today.
test_per_job_permissions_pass() {
    dir="$(workflows_dir per-job)"
    cat >"$dir/release.yml" <<'YAML'
name: Release

on:
  push:
    tags: ['v*']

jobs:
  build:
    name: Build
    runs-on: ubuntu-latest
    permissions:
      contents: read
    steps:
      - uses: actions/checkout@v7

  publish:
    name: Publish
    runs-on: ubuntu-latest
    needs: build
    permissions:
      contents: write
    steps:
      - uses: actions/checkout@v7
YAML
    assert_succeeds "$script" "$dir"
    assert_eq "" "$stdout" "stdout when every job is scoped"
}

# Criterion: no top-level block and one job without its own — exit 1, and the
# message names the file and the job so the pull request shows what to fix.
test_one_job_without_permissions_fails_naming_it() {
    dir="$(workflows_dir one-missing)"
    cat >"$dir/ci.yml" <<'YAML'
name: CI

on:
  pull_request:

jobs:
  quality:
    name: fmt, clippy, test
    runs-on: ubuntu-latest
    permissions:
      contents: read
    steps:
      - uses: actions/checkout@v7

  licence-headers:
    name: Every .rs carries the SPDX header
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v7
YAML
    assert_fails "$script" "$dir"
    assert_contains "$stdout$stderr" "ci.yml: job licence-headers has no permissions block" "refusal names the job"
}

# Criterion: every offending job is listed, not just the first — a run that
# stopped at the first would make the maintainer fix them one push at a time.
test_every_offending_job_is_named() {
    dir="$(workflows_dir two-missing)"
    cat >"$dir/ci.yml" <<'YAML'
name: CI

on:
  pull_request:

jobs:
  quality:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v7

  scripts:
    runs-on: ubuntu-latest
    permissions:
      contents: read
    steps:
      - uses: actions/checkout@v7

  deny:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v7
YAML
    assert_fails "$script" "$dir"
    assert_contains "$stdout$stderr" "ci.yml: job quality has no permissions block" "first offending job named"
    assert_contains "$stdout$stderr" "ci.yml: job deny has no permissions block" "last offending job named"
    [[ "$stdout$stderr" != *"job scripts"* ]] \
        || fail "the scoped job must not be reported: $stdout$stderr"
}

# Criterion: a file without a `jobs:` key fails with `<file>: no jobs found`
# — a file the check cannot read is not a file it can vouch for.
test_file_without_jobs_fails() {
    dir="$(workflows_dir no-jobs)"
    cat >"$dir/stub.yml" <<'YAML'
name: Stub

on:
  workflow_call:
YAML
    assert_fails "$script" "$dir"
    assert_contains "$stdout$stderr" "stub.yml: no jobs found" "refusal names the file"
}

# Criterion: `permissions:` in a comment or inside a step's `run:` string is
# not a block. The fixture has a top-level comment `# permissions:`, a job
# without its own block, and a step echoing `permissions:` — the decoys must
# not rescue the job, so the file fails and names it.
test_permissions_in_a_comment_or_a_step_does_not_count() {
    dir="$(workflows_dir decoys)"
    cat >"$dir/decoy.yml" <<'YAML'
name: Decoy

on:
  pull_request:

# permissions:
#   contents: read

jobs:
  unscoped:
    runs-on: ubuntu-latest
    # permissions:
    #   contents: read
    steps:
      - uses: actions/checkout@v7
      - name: Mention the key in a string
        run: echo permissions:
      - name: Mention the key on its own line
        run: |
          permissions:
          echo done
YAML
    assert_fails "$script" "$dir"
    assert_contains "$stdout$stderr" "decoy.yml: job unscoped has no permissions block" "decoys do not rescue the job"
}

# Criterion: a directory that does not exist exits 2 with a message — distinct
# from a failing check, so a mistyped path is never read as "all scoped".
test_missing_directory_exits_2() {
    run "$script" "$tmp/does-not-exist"
    assert_eq 2 "$status" "exit status for a missing directory"
    assert_contains "$stderr" "does-not-exist" "message names the missing directory"
}

# Criterion: the repository's own `.github/workflows` passes — the check the
# `scripts` CI job runs with no argument. RED by design: ci.yml has no
# permissions block today, which is the defect #126 fixes; this test goes
# green only once the GREEN phase adds the top-level block to ci.yml.
test_the_repository_workflows_pass() {
    assert_succeeds "$script" "$REPO_ROOT/.github/workflows"
    assert_eq "" "$stdout" "stdout when the repository workflows are scoped"
    # The default argument must resolve to the same directory when run from
    # the repository root, since the CI step passes none.
    (cd "$REPO_ROOT" && assert_succeeds "$script")
}
