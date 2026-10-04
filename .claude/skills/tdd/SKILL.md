# /tdd — TDD Workflow Skill

Orchestrates the Red → Green → Refactor TDD cycle using three sub-agents, each
working in an isolated git worktree and receiving only the context relevant to its
phase — then opens the pull request and, once a human has merged it, cleans up.

This is a **cargo workspace** with three layers (see `docs/ARCHITECTURE.md`):
- **mobile** — `blue2th-frontend` (Dioxus/Android app): `blue2th-frontend/`. The
  workspace root holds no package of its own.
- **server** — `blue2th-server` (Axum/Tokio Linux backend, BlueZ + PipeWire): `blue2th-server/`.
- **proto** — `blue2th-proto` (serde DTOs shared by both, target-agnostic): `blue2th-proto/`.

A feature may touch one, two, or all three layers. The quality gates always run
across the whole workspace; the Android NDK cross-build runs **only** when the
mobile layer is affected.

**Scope: anything that branches from `develop`.** The cycle branches from `HEAD`,
so it always starts there. A hotfix branches from `main` and is a different skill.

**The change is not always a feature.** Deleting dead code is a `refactor`, a
tooling change is a `chore`. The spec says which, through its **Change Type**, and
that one field drives both the branch prefix and the pull-request title — so a
cleanup does not reach the tracker announced as a feature. It used to be hardcoded
to `feat`, which meant every honest branch name had to be a deviation from this
file.

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
agents keep to Read/Edit/Write/Bash (and Serena, per step 7a) inside their
worktree, which removes the problem rather than working around it.

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

`tdd/feature.md` is a **working file, ignored by git**, generated from the
versioned `tdd/feature.template.md`. Create it if it is missing — a fresh clone
has only the template:

```bash
[ -f tdd/feature.md ] || cp tdd/feature.template.md tdd/feature.md
```

Then read it. If any line is `PENDING`, or ends with ` PENDING` — a list item
the template left open, such as `- PENDING`, `- [ ] PENDING` or
`- Speaker lost (Bluetooth drop, power off): PENDING` — stop and tell the user:

> "`tdd/feature.md` still has PENDING sections. Describe the feature and I will fill the file."

**Never `git add` it, in this skill or in a sub-agent prompt.** It is ignored, so
`git add -A` cannot sweep it in — which is the point: it reached a feature branch
that way once, through an agent's `git add -A`, and only a history rewrite got it
back out. The same holds for `tdd/REVIEW.md`.

### 4. Create or reuse the feature worktree — skipped for `cleanup`

a. Read the **Feature Name** and **Change Type** lines from `tdd/feature.md`.

b. Derive a **slug**: lowercase, strip accents, replace spaces and non-alphanumeric
   characters with hyphens, collapse consecutive hyphens, strip leading/trailing
   hyphens. Example: `"Filter devices by name"` → `filter-devices-by-name`

c. Set:
   - `TYPE` = the **Change Type** line. It must be one of the types
     `.githooks/commit-msg` accepts — `feat fix refactor test chore docs style
     perf build ci revert`. Anything else, or a missing line, → **stop and ask**
     rather than defaulting to `feat`: the branch name hangs off it, and unlike
     the pull-request title a branch name is not cheap to correct once pushed.
   - `BRANCH=$TYPE/<slug>`
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

e. Copy the spec into the worktree. `tdd/feature.md` is gitignored, so
   `git worktree add` does **not** bring it along — the agents would find the
   file missing:
   ```bash
   cp tdd/feature.md "$WORKTREE_PATH/tdd/feature.md"
   ```
   It stays ignored there too, so it cannot reach the branch.

f. Write the two files the optional Serena mode reads (step 7a). Both are
   gitignored and harmless when Serena is not used, so they are written always:
   ```bash
   mkdir -p "$WORKTREE_PATH/.serena/memories"
   echo "tdd worktree: <slug>" > "$WORKTREE_PATH/.serena/memories/tdd_worktree.md"
   printf 'project_name: "blue2th-%s"\nadded_modes:\n  - %s\n' \
     "<slug>" "$ROOT/.serena/modes/tdd-worktree.yml" \
     > "$WORKTREE_PATH/.serena/project.local.yml"
   ```
   The marker is how an agent tells that Serena is bound to its worktree. The
   mode is named by an absolute path because Serena resolves a relative one from
   its own working directory, not from the project it serves. The project name
   is overridden because the versioned `project.yml` says `blue2th`: without it
   every worktree registers in Serena under the base checkout's name, and an
   activation by name no longer says which checkout it means.

g. Print: `Worktree ready: $WORKTREE_PATH (branch: $BRANCH, base: $BASE_SHA)`

### 5. `issue` — open the tracking issue

Runs first in `all`, and standalone as `/tdd issue`. **Idempotent**: the issue
number is persisted so a re-run never opens a second one.

a. If `$WORKTREE_PATH/.tdd-issue` exists, read `ISSUE_NUMBER` from it, print
   `Tracking issue: #$ISSUE_NUMBER (already open)` and skip the rest of this step.

b. Otherwise, read the **Tracking Issue** line from `tdd/feature.md`.

   **It names an issue (`#N`)** — the work is already filed, which is the normal
   case when the cycle starts from the tracker rather than from a conversation.
   Do not open a second one: two issues for one change split the discussion, and
   `Closes #N` then closes the one nobody is watching. Confirm the number first,
   because **issues and pull requests share a single numbering** — `#27` can be a
   merged pull request:
   ```bash
   gh issue view <N> --json number,state,title
   ```
   `Could not resolve to an Issue`, or a `CLOSED` state → stop and ask; that
   number is not a tracking issue for work about to start. Otherwise seed the
   marker, print `Tracking issue: #N (existing, reused)`, and skip (c):
   ```bash
   echo "<N>" > "$WORKTREE_PATH/.tdd-issue"
   ```

   **It says `none`** — there is no issue yet. Create it, per (c).

c. Create it:
   - **Title** — the **Feature Name** line, verbatim.
   - **Body** — the **Description** section only.

   The nominal and non-nominal scenarios are working material for the agents, not
   tracker content. Leave them in `feature.md`.

   ```bash
   gh issue create --title "<Feature Name>" --body "<Description>"
   ```

d. Persist the number, which has to survive to the `pr` phase so the pull request
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

### 6b. Build the project map

The agents' definitions describe what does not change: the layers, their
rules, the hardware boundary. Which files a crate holds and which test doubles
stand in for the hardware do change, and every copy of them written into the
definitions went stale: six catch-ups by hand, and at the 2026-10-02 review
`FakeGraph` in none of the three and 4 of 18 server files listed. So that part is generated here, before
each agent spawn, from the worktree as it stands:

```bash
CRATES="<blue2th-server for server, blue2th-proto for proto, blue2th-frontend for mobile — the LAYERS only>"
cd "$WORKTREE_PATH" && for c in $CRATES; do
  git ls-files "$c/src/*.rs" "$c/tests/*.rs" | xargs wc -l | grep -v ' total$'
done
cd "$WORKTREE_PATH" && for c in $CRATES; do git ls-files "$c/tests" ':!*.rs'; done
cd "$WORKTREE_PATH" && git grep -nE '^\s*(pub(\([a-z]+\))? )?struct (Fake|Null|Stub|Mock)[A-Za-z]*' \
  -- $(for c in $CRATES; do echo "$c/src" "$c/tests"; done)
```

Inject the three outputs into every agent prompt under `## Project Map`, as
`### Files (lines)`, `### Fixtures` and `### Test doubles (file:line)`. Line
counts tell an agent which files to read by symbol rather than whole. The
fixtures are captures of what the system really produced, which a test builds
on before inventing data; write "none" when the list is empty.

### 7. Spawn agents with targeted context

Spawn each phase with its **dedicated** agent type — `tdd-test-writer` (RED),
`tdd-implementer` (GREEN), `tdd-reviewer` (REFACTOR). These agents carry the right
tool grants (`Read, Edit, Write, Bash`, plus Serena's, which they use only per
step 7a); `general-purpose` is denied `Read`/`Bash` by the permission hooks and
will stall. A Serena tool listed while no Serena server runs is dropped, not an
error.

Because the dedicated agent's `.md` body is already its system prompt, **do not**
re-inject it into the prompt. Pass only the contextual sections shown below (the
parts after the first `---`: Worktree, Affected Layers, and the phase-specific
context). The leading `<...-body>` placeholder in each structure is therefore
omitted when spawning a dedicated agent.

### 7a. Serena mode (optional) — around each agent

**Skip this step when `mcp__serena__activate_project` is not among your tools.**
The cycle then runs as it always has, and the agents keep to Read/Edit.

Serena serves one project at a time, and the agents share this session's
server. Bound to the base checkout, it would show the agents `develop`, and
their Serena edits would land in the base checkout while their Read/Edit work
lands in the worktree. Around **each** agent spawn:

Send each `activate_project` **alone**, and wait for its answer before the next
Serena call. Calls sent in one parallel batch reach Serena in no guaranteed
order: in the #154 cycle a switch back to the base checkout and a switch to the
worktree, batched together, were applied in reverse, and Serena stayed on the
base checkout. The check in 2 caught it; sending them one at a time avoids it.

1. `mcp__serena__activate_project` with `project: "<WORKTREE_PATH>"`. It answers
   at once and starts rust-analyzer on the worktree in the background. The
   worktree's `project.local.yml` (step 4f) turns the `tdd-worktree` mode on:
   its rules, and no memory writes.
2. Check the binding before the agent meets it:
   - `mcp__serena__read_memory` with `memory_name: "tdd_worktree"` must answer
     `tdd worktree: <slug>`. This first call waits for rust-analyzer: about
     15–20 s on a fresh worktree.
   - `mcp__serena__find_referencing_symbols` with `name_path: "AudioError"` and
     `relative_path: "blue2th-server/src/audio.rs"` must answer at least one
     reference. rust-analyzer answers an empty list while it is still loading,
     and that must reach this check, not an agent. Retry it a few times.

   If either check still fails, say so, run step 4 now, and spawn the agent
   anyway: it finds no marker and keeps to Read/Edit.
3. Spawn the agent (the prompt structures below).
4. `mcp__serena__activate_project` with `project: "<ROOT>"` — also when the
   agent failed, and before the manual-verification halt — so this session's
   own Serena calls read the base checkout again.

**Setup this mode needs**, all local to one developer:
- Serena running with a context whose `single_project` is `false`. The built-in
  `claude-code` context sets it to `true`, which removes `activate_project`: copy
  that context, flip the key, and pass the copy with `--context=<path>`.
- Serena's tools approved without a prompt for headless agents — the
  `serena-hooks auto-approve` hook, or `mcp__serena__*` in the allow list.

**The worktree stays in Serena's registry after cleanup, and that is fine.**
Each activation registers the worktree in `~/.serena/serena_config.yml`, and
nothing here removes it: Serena skips a registered project whose directory is
gone when it next starts. Until then a running server keeps it in memory and
rewrites the file on every activation, so an entry deleted by hand while a
server runs comes back. Do not edit the registry by hand.

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

## Project Map
<PROJECT_MAP from step 6b>

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

## Project Map
<PROJECT_MAP from step 6b>

---

## Test Files to Make Pass

Read each of these files, focusing on `#[cfg(test)]` blocks and files under `tests/`:

<output of: git -C WORKTREE_PATH diff BASE_SHA --name-only | grep -E '\.(rs|toml)$'>

---

## Change Type
<TYPE> — commit the implementation as `<TYPE>(<scope>): …`, not `feat(<scope>): …`.

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

## Project Map
<PROJECT_MAP from step 6b>

---

## Changes Since Branch Creation (base: <BASE_SHA>)

<CONTEXT>

---

## Feature Name
<Feature Name from tdd/feature.md>

## Acceptance Criteria
<Acceptance Criteria section from tdd/feature.md>

## Guards
<Guards section from tdd/feature.md — the rules its Mutations Checked table covers>
```

### 8. Sequencing for `all`

`issue` → RED → wait → build check → GREEN → **manual-verification halt** →
REFACTOR → wait → `pr`. Print a separator between phases:
`\n--- [Phase] complete ---\n`.

#### The halt after GREEN

Read the `## Manual verification` section of `tdd/feature.md`. If it holds
anything but `PENDING` or nothing at all, **stop after GREEN**: print the section
verbatim, say the feature is ready to try, and tell the user to run `/tdd review`
when they are done. Do not spawn the reviewer.

Two reasons, and neither is politeness:

- REFACTOR rewrites code whose only proof is the test suite. If the behaviour is
  broken where no test looks, refactoring first means debugging a moving target.
- A fix found after REFACTOR lands as a fourth commit, which breaks the
  `test:` / `feat:` / `refactor:` shape that shows the tests came first.

An empty section means everything is covered by tests, and `all` runs straight
through. On the mobile layer it is almost never empty: Dioxus components are not
test-runnable here, so anything rendered is verified by hand or not at all.

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

c. Create it. The **title must follow Conventional Commits**, or the
   `commit-convention` job fails on the pull request the moment it is opened —
   and until #40 landed, correcting the title afterwards re-ran nothing, so it
   stayed red. A Feature Name is a phrase, not a commit subject, so compose the
   title instead of passing the name through:

   ```
   TITLE="$TYPE: <Feature Name>"
   ```

   The type is the spec's **Change Type**, the same one the branch carries, so a
   `refactor/<slug>` branch opens a `refactor: …` pull request; the scope is
   optional in the pattern, which lives in `.githooks/commit-msg`. The body is built from
   `feature.md`'s **Description**, plus `Closes #<ISSUE_NUMBER>` read from
   `$WORKTREE_PATH/.tdd-issue`:
   ```bash
   gh pr create --base develop --head "$BRANCH" \
     --title "$TITLE" --body "<Description>

   Closes #<ISSUE_NUMBER>"
   ```
   Same source as the issue body, so the two cannot drift. **Not `--fill`**, which
   composes the body from commit messages instead.

   Then add, in the body, anything a reviewer would miss on a green CI run — a new
   dependency, a change to a shared DTO, a file touched outside the stated scope, a
   test deleted rather than fixed. `CLAUDE.md` requires it, and only this
   orchestrator has the diff in view. The reviewer lists the tests it deleted
   under **Tests Removed** in `tdd/REVIEW.md`: copy that section into the body,
   since the report itself only arrives as a comment.

d. Post the review report as a **comment**, not a commit:
   ```bash
   gh pr comment <N> --body-file "$WORKTREE_PATH/tdd/REVIEW.md"
   ```
   It stays attached to the change and readable at review time, without adding a
   `docs(tdd)` commit to the branch. That leaves exactly three commits — `test:`,
   the implementation's own type, then `refactor:` — which are the change *and*
   the proof the tests came first. **Do not squash them.** Reverting the whole feature is already
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

No slug is needed: the script finds the feature worktree by the `.tdd-base-sha`
marker written into it, and takes the branch from git's own worktree list. Pass a
slug only when the worktree is already gone.

Report its output as-is. It exits non-zero and touches nothing when the pull
request is not merged — open, or closed without merging — and says which. Do not
work around a refusal: an open PR means the work is not done, and a closed one
means it was abandoned.

It also refuses to remove a worktree holding untracked files, and says so rather
than deleting them. That is not an obstacle to route around either: look at the
files first.

The same script runs unattended from `.githooks/post-merge` with `--from-hook`,
for anyone who sets `blue2th.tddAutoCleanup`. Do not add logic here that the hook
would not get.

### 11. Final report

After the phases complete:

1. If REFACTOR ran, read `<WORKTREE_PATH>/tdd/REVIEW.md` and display its full contents.
2. Print the worktree, the branch, the tracking issue and the pull request URL.
3. Say whether the Serena mode ran for each agent (step 7a), and which check
   failed when it did not. The trial is measured against the timing baseline,
   so a phase run without Serena has to be told apart.
