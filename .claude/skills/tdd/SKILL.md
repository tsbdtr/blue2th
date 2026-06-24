# /tdd — TDD Workflow Skill

Orchestrates the Red → Green → Refactor TDD cycle using three sub-agents,
each working in an isolated git worktree and receiving only the context relevant to its phase.

This is a **cargo workspace** with three layers (see `docs/ROADMAP.md`):
- **mobile** — `blue2th` (Dioxus/Android app, the root crate): `src/`, `tests/`, `assets/`, `locales/`.
- **server** — `blue2th-server` (Axum/Tokio Linux backend, BlueZ + PipeWire): `blue2th-server/`.
- **proto** — `blue2th-proto` (serde DTOs shared by both, target-agnostic): `blue2th-proto/`.

A feature may touch one, two, or all three layers. The quality gates always run
across the whole workspace; the Android NDK cross-build runs **only** when the
mobile layer is affected.

## Usage
`/tdd [test|impl|review|all|done]`

## Steps

### 1. Determine the phase
From the skill args:
- `test`          → RED phase only
- `impl`          → GREEN phase only
- `review`        → REFACTOR phase only
- `all` or no arg → all three phases sequentially
- `done`          → cleanup after merge (remove worktree, reset spec)
- anything else   → show usage

### 2. Validate the feature spec (skip if phase is `done`)
If the phase is **not** `done`: read `tdd/feature.md`.
If any line equals exactly `PENDING`, stop immediately and tell the user:
> "`tdd/feature.md` still has PENDING sections. Describe the feature and I will fill the file."

### 3. Create or reuse the feature worktree

Run the following using Bash:

a. Read the **Feature Name** line from `tdd/feature.md`.

b. Derive a **slug**: lowercase, strip accents, replace spaces and non-alphanumeric characters with hyphens, collapse consecutive hyphens, strip leading/trailing hyphens.
   Example: `"Filter devices by name"` → `filter-devices-by-name`

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

### 4. Spawn agents with targeted context

Spawn each phase with its **dedicated** agent type — `tdd-test-writer` (RED),
`tdd-implementer` (GREEN), `tdd-reviewer` (REFACTOR). These agents carry the right
tool grants (`Read, Edit, Write, Bash`); `general-purpose` is denied `Read`/`Bash`
by the permission hooks and will stall.

Because the dedicated agent's `.md` body is already its system prompt, **do not**
re-inject it into the prompt. Pass only the contextual sections shown below (the
parts after the first `---`: Worktree, Affected Layers, and the phase-specific
context). The leading `<...-body>` placeholder in each structure is therefore
omitted when spawning a dedicated agent.

#### 4.0 Determine the affected layers

Build a `LAYERS` set from `tdd/feature.md`:

1. Read the **Layers touched** section — it is the source of truth (checkboxes
   for `mobile`, `server`, `proto`).
2. Cross-check against the **Technical Scope** file paths, mapping each path to a layer:
   - `blue2th-server/...` → **server**
   - `blue2th-proto/...` → **proto**
   - `src/...`, top-level `tests/...`, `assets/...`, `locales/...` → **mobile**
3. If the two disagree, trust the file paths and warn the user.
4. If nothing is decidable, default to all three layers (safest).

Render `LAYERS` as a comma-separated list (e.g. `server, proto`) and inject it into
every agent prompt under an `## Affected Layers` heading. Agents use it to decide
whether to run the Android NDK cross-build (mobile only) and which crates to focus on.

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

---

### 5. Sequencing for `all`

Spawn RED → wait → run build check → spawn GREEN → wait → spawn REFACTOR.
Print a separator between phases: `\n--- [Phase] complete ---\n`.

### 6. Final report

After all agents complete:

1. If the REFACTOR phase ran, read `<WORKTREE_PATH>/tdd/REVIEW.md` and display its full contents.

2. Then output:
   - Worktree: `<WORKTREE_PATH>` — branch: `<BRANCH>`
   - Next steps:
     ```bash
     git -C <WORKTREE_PATH> push -u origin <BRANCH>   # open a PR
     # once merged, run: /tdd done
     ```

### 7. `done` — cleanup after merge

Run the following using Bash:

a. Read the **Feature Name** from `tdd/feature.md` and derive the slug (same rule as step 3b).

b. Set:
   - `ROOT=$(git rev-parse --show-toplevel)`
   - `WORKTREE_PATH=$(dirname "$ROOT")/blue2th-<slug>`
   - `BRANCH=feat/<slug>`

c. Verify the worktree exists:
   ```bash
   git worktree list | grep "$WORKTREE_PATH"
   ```
   If not found, tell the user "No worktree found for this feature — nothing to clean up." and stop.

d. Remove the worktree and delete the local branch:
   ```bash
   git worktree remove --force "$WORKTREE_PATH"
   git branch -d "$BRANCH"
   ```
   `--force` is required because the Rust `target/` directory is always present and untracked.
   If `git branch -d` fails (branch not yet merged), warn the user and do **not** force-delete.

e. Reset the feature spec:
   ```bash
   git checkout HEAD -- tdd/feature.md
   ```

f. Update the knowledge graph (AST-only, no API cost):
   ```bash
   graphify update .
   ```
   If `graphify` is not found, skip this step and warn the user.

g. If any files in `graphify-out/` changed, commit them:
   ```bash
   git add graphify-out/
   git diff --cached --quiet || git commit -m "chore(graph): update knowledge graph after <slug>"
   ```
   Replace `<slug>` with the actual feature slug.

h. Print: `Cleaned up: worktree and branch <BRANCH> removed. tdd/feature.md reset. Knowledge graph updated.`
