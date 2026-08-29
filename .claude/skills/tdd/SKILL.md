# /tdd — TDD Workflow Skill

Orchestrates the Red → Green → Refactor TDD cycle using three sub-agents, each
working in an isolated git worktree and receiving only the context relevant to its
phase — then opens the pull request and, once a human has merged it, cleans up.

This is a **cargo workspace** with three layers (see `docs/ROADMAP.md`):
- **mobile** — `blue2th-frontend` (Dioxus/Android app): `blue2th-frontend/`. The
  workspace root holds no package of its own.
- **server** — `blue2th-server` (Axum/Tokio Linux backend, BlueZ + PipeWire): `blue2th-server/`.
- **proto** — `blue2th-proto` (serde DTOs shared by both, target-agnostic): `blue2th-proto/`.

A feature may touch one, two, or all three layers. The quality gates always run
across the whole workspace; the Android NDK cross-build runs **only** when the
mobile layer is affected.

**Scope: features only.** The cycle branches from `HEAD`, so it always starts from
`develop`. A hotfix branches from `main` and is a different skill.

## Usage

`/tdd [issue|test|impl|review|pr|all|cleanup]`

| Phase     | Does                                                              |
|-----------|-------------------------------------------------------------------|
| `issue`   | Open the tracking issue from the spec (idempotent)                |
| `test`    | RED — write failing tests                                         |
| `impl`    | GREEN — implement until they pass                                 |
| `review`  | REFACTOR — improve without breaking them                          |
| `pr`      | Push the branch, open the PR, post the review report as a comment |
| `all`     | `issue` → `test` → `impl` → `review` → `pr`                       |
| `cleanup` | Verify the PR merged, then remove the worktree and reset the spec |

## Where `gh` may run

**Every `gh` call stays in this orchestrator. Never put one in a sub-agent prompt.**

The three sub-agents run headless. A `gh` command that would raise a permission
prompt gets no prompt there — it hangs or fails with nothing to show for it. The
agents keep to Read/Edit/Write/Bash inside their worktree, which removes the
problem rather than working around it.

## Steps

### 1. Determine the phase

From the skill args: `issue`, `test`, `impl`, `review`, `pr`, `cleanup`, or
`all` / no arg. Anything else → show the usage table and stop.

### 2. `gh` preflight — for `issue`, `pr`, `cleanup` and `all`

```bash
command -v gh >/dev/null || echo "MISSING"
gh auth status >/dev/null 2>&1 || echo "UNAUTHENTICATED"
```

If either fails, stop and say which command failed and how to fix it
(`gh auth login`). Do not proceed and discover it three phases later.

### 3. Validate the feature spec — skipped for `cleanup`

Read `tdd/feature.md`. If any line equals exactly `PENDING`, stop and tell the user:

> "`tdd/feature.md` still has PENDING sections. Describe the feature and I will fill the file."

### 4. Create or reuse the feature worktree — skipped for `cleanup`

a. Read the **Feature Name** line from `tdd/feature.md`.

b. Derive a **slug**: lowercase, strip accents, replace spaces and non-alphanumeric
   characters with hyphens, collapse consecutive hyphens, strip leading/trailing
   hyphens. Example: `"Filter devices by name"` → `filter-devices-by-name`

c. Set:
   - `BRANCH=feat/<slug>`
   - `ROOT=$(git rev-parse --show-toplevel)`
   - `WORKTREE_PATH=$(dirname "$ROOT")/blue2th-<slug>`

d. Check whether the worktree already exists:
   ```bash
   git worktree list | grep "$WORKTREE_PATH"
   ```

   **Not found** → create it and persist the base SHA:
   ```bash
   BASE_SHA=$(git rev-parse HEAD)
   git worktree add "$WORKTREE_PATH" -b "$BRANCH"
   echo "$BASE_SHA" > "$WORKTREE_PATH/.tdd-base-sha"
   ```

   **Found** → reuse it, recover the base SHA from the file:
   ```bash
   BASE_SHA=$(cat "$WORKTREE_PATH/.tdd-base-sha")
   ```

e. Print: `Worktree ready: $WORKTREE_PATH (branch: $BRANCH, base: $BASE_SHA)`

### 5. `issue` — open the tracking issue

Runs first in `all`, and standalone as `/tdd issue`. **Idempotent**: the issue
number is persisted so a re-run never opens a second one.

a. If `$WORKTREE_PATH/.tdd-issue` exists, read `ISSUE_NUMBER` from it, print
   `Tracking issue: #$ISSUE_NUMBER (already open)` and skip the rest of this step.

b. Otherwise create it:
   - **Title** — the **Feature Name** line, verbatim.
   - **Body** — the **Description** section only.

   The nominal and non-nominal scenarios are working material for the agents, not
   tracker content. Leave them in `feature.md`.

   ```bash
   gh issue create --title "<Feature Name>" --body "<Description>"
   ```

c. Persist the number, which has to survive to the `pr` phase so the pull request
   can carry `Closes #N`:
   ```bash
   echo "<N>" > "$WORKTREE_PATH/.tdd-issue"
   ```
   `.tdd-issue` is gitignored, like `.tdd-base-sha`, and dies with the worktree.

### 6. Determine the affected layers

Build a `LAYERS` set from `tdd/feature.md`:

1. Read the **Layers touched** section — it is the source of truth (checkboxes
   for `mobile`, `server`, `proto`).
2. Cross-check against the **Technical Scope** file paths, mapping each path to a layer:
   - `blue2th-server/...` → **server**
   - `blue2th-proto/...` → **proto**
   - `blue2th-frontend/...` → **mobile**
3. If the two disagree, trust the file paths and warn the user.
4. If nothing is decidable, default to all three layers (safest).

Render `LAYERS` as a comma-separated list (e.g. `server, proto`) and inject it into
every agent prompt under an `## Affected Layers` heading. Agents use it to decide
whether to run the Android NDK cross-build (mobile only) and which crates to focus on.

### 7. Spawn agents with targeted context

Spawn each phase with its **dedicated** agent type — `tdd-test-writer` (RED),
`tdd-implementer` (GREEN), `tdd-reviewer` (REFACTOR). These agents carry the right
tool grants (`Read, Edit, Write, Bash`); `general-purpose` is denied `Read`/`Bash`
by the permission hooks and will stall.

Because the dedicated agent's `.md` body is already its system prompt, **do not**
re-inject it into the prompt. Pass only the contextual sections shown below (the
parts after the first `---`: Worktree, Affected Layers, and the phase-specific
context). The leading `<...-body>` placeholder in each structure is therefore
omitted when spawning a dedicated agent.

---

#### RED — tdd-test-writer

Prompt structure:
```
<tdd-test-writer body>

---

## Worktree
Work exclusively inside: `<WORKTREE_PATH>`
Prefix every Bash command with: `cd <WORKTREE_PATH> &&`
Branch: `<BRANCH>`

---

## Affected Layers
<LAYERS>

---

## Feature Specification

<full contents of tdd/feature.md>
```

---

#### GREEN — tdd-implementer

Before spawning, verify the RED phase output compiles across the whole workspace:
```bash
cd "$WORKTREE_PATH" && cargo build --workspace --tests 2>&1 | tail -20
```
If the build fails, **abort** and tell the user: "RED phase produced non-compiling tests — fix them before running impl."

Then gather changed files:
```bash
git -C "$WORKTREE_PATH" diff "$BASE_SHA" --name-only | grep -E '\.(rs|toml)$'
```

Prompt structure:
```
<tdd-implementer body>

---

## Worktree
Work exclusively inside: `<WORKTREE_PATH>`
Prefix every Bash command with: `cd <WORKTREE_PATH> &&`
Branch: `<BRANCH>`

---

## Affected Layers
<LAYERS>

---

## Test Files to Make Pass

Read each of these files, focusing on `#[cfg(test)]` blocks and files under `tests/`:

<output of: git -C WORKTREE_PATH diff BASE_SHA --name-only | grep -E '\.(rs|toml)$'>

---

## Feature Name
<Feature Name from tdd/feature.md>

## Acceptance Criteria
<Acceptance Criteria section from tdd/feature.md>
```

---

#### REFACTOR — tdd-reviewer

Before spawning, gather the diff:
```bash
DIFF=$(git -C "$WORKTREE_PATH" diff "$BASE_SHA")
DIFF_LINES=$(echo "$DIFF" | wc -l)
if [ "$DIFF_LINES" -le 300 ]; then
  CONTEXT="$DIFF"
else
  STAT=$(git -C "$WORKTREE_PATH" diff "$BASE_SHA" --stat)
  FILES=$(git -C "$WORKTREE_PATH" diff "$BASE_SHA" --name-only)
  CONTEXT="$STAT

Changed files (diff exceeds 300 lines — read each file individually):
$FILES"
fi
```

Prompt structure:
```
<tdd-reviewer body>

---

## Worktree
Work exclusively inside: `<WORKTREE_PATH>`
Prefix every Bash command with: `cd <WORKTREE_PATH> &&`
Branch: `<BRANCH>`

---

## Affected Layers
<LAYERS>

---

## Changes Since Branch Creation (base: <BASE_SHA>)

<CONTEXT>

---

## Feature Name
<Feature Name from tdd/feature.md>

## Acceptance Criteria
<Acceptance Criteria section from tdd/feature.md>
```

### 8. Sequencing for `all`

`issue` → RED → wait → build check → GREEN → wait → REFACTOR → wait → `pr`.
Print a separator between phases: `\n--- [Phase] complete ---\n`.

### 9. `pr` — push and open the pull request

a. Push the branch:
   ```bash
   git -C "$WORKTREE_PATH" push -u origin "$BRANCH"
   ```

b. **Idempotent** — if a pull request already exists for this branch, do not open a
   second one; report the existing one and go to step (d):
   ```bash
   gh pr list --head "$BRANCH" --state open --json number,url
   ```

c. Create it. The body is built from `feature.md`'s **Description**, plus
   `Closes #<ISSUE_NUMBER>` read from `$WORKTREE_PATH/.tdd-issue`:
   ```bash
   gh pr create --base develop --head "$BRANCH" \
     --title "<Feature Name>" --body "<Description>

   Closes #<ISSUE_NUMBER>"
   ```
   Same source as the issue body, so the two cannot drift. **Not `--fill`**, which
   composes the body from commit messages instead.

   Then add, in the body, anything a reviewer would miss on a green CI run — a new
   dependency, a change to a shared DTO, a file touched outside the stated scope, a
   test deleted rather than fixed. `CLAUDE.md` requires it, and only this
   orchestrator has the diff in view.

d. Post the review report as a **comment**, not a commit:
   ```bash
   gh pr comment <N> --body-file "$WORKTREE_PATH/tdd/REVIEW.md"
   ```
   It stays attached to the change and readable at review time, without adding a
   `docs(tdd)` commit to the branch. That leaves exactly three commits — `test:`,
   `feat:`, `refactor:` — which are the feature *and* the proof the tests came
   first. **Do not squash them.** Reverting the whole feature is already
   `git revert -m 1 <merge-commit>`.

e. **Never `gh pr merge`.** This skill opens the pull request; a human merges it.
   That is the last human checkpoint in the cycle, and it is a governance rule, not
   a statement about capability.

f. Print the pull request URL and: `Once it is merged, run: /tdd cleanup`

### 10. `cleanup` — after the merge

Delegate to the script, which holds the whole logic so a `post-merge` hook can call
it too (#17):

```bash
tdd/cleanup.sh
```

Report its output as-is. It exits non-zero and touches nothing when the pull
request is not merged — open, or closed without merging — and says which. Do not
work around a refusal: an open PR means the work is not done, and a closed one
means it was abandoned.

### 11. Final report

After the phases complete:

1. If REFACTOR ran, read `<WORKTREE_PATH>/tdd/REVIEW.md` and display its full contents.
2. Print the worktree, the branch, the tracking issue and the pull request URL.
