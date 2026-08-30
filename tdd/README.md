# TDD Workflow — blue2th

Red → Green → Refactor, driven by three Claude agents, from the tracking issue to
the merged pull request.

## Usage

```
/tdd [issue|test|impl|review|pr|all|cleanup]
```

| Command        | Phase    | Agent            | Role                                              |
|----------------|----------|------------------|---------------------------------------------------|
| `/tdd issue`   | —        | orchestrator     | Open the tracking issue from the spec (idempotent) |
| `/tdd test`    | RED      | tdd-test-writer  | Write failing tests                               |
| `/tdd impl`    | GREEN    | tdd-implementer  | Implement until tests pass                        |
| `/tdd review`  | REFACTOR | tdd-reviewer     | Refactor without breaking tests                   |
| `/tdd pr`      | —        | orchestrator     | Push, open the PR, post the review report         |
| `/tdd all`     | Full     | all of the above | issue → red → green → refactor → pr               |
| `/tdd cleanup` | —        | `tdd/cleanup.sh` | Verify the PR merged, then clean up               |

**Features only.** The cycle branches from `HEAD`, so it always starts from
`develop`. A hotfix branches from `main` and is a different skill.

## Before running

1. Describe the feature to Claude, in conversation. Claude asks for the **nominal
   scenario**, then for how each obvious **non-nominal case** should behave, and runs
   a code-impact analysis (`graphify query` plus the relevant sources) — see the
   `tdd` section of `CLAUDE.md`.
2. If the feature is visible on screen, Claude asks **how it is represented** —
   which surface, what colour, where — because nothing rendered is test-runnable
   here, so the spec is the only record of it.
3. Claude fills `tdd/feature.md`, **including the `Layers touched` checkboxes** and
   the **`Manual verification`** section, then **stops** so you can read it.
4. Run `/tdd all`.

A git worktree is created at `../blue2th-<feature-slug>` on branch
`feat/<feature-slug>`. Each feature is isolated, so several can run in parallel.

## What ends up where

- **The tracking issue** — title from `Feature Name`, body from `Description`. The
  scenarios stay in `feature.md`: they are working material for the agents, not
  tracker content.
- **The pull request** — same `Description`, plus `Closes #N` so GitHub closes the
  issue on merge, plus whatever a reviewer would miss on a green CI run.
- **The review report** — a *comment* on the pull request, never a commit. The
  branch therefore ends with exactly three commits:

  ```
  test(x): add red-phase tests
  feat(x): implement
  refactor(x): ...
  ```

  **Do not squash them.** They are the feature *and* the proof the tests came
  first, which is the whole point of the workflow. Reverting a feature is already
  `git revert -m 1 <merge-commit>`.

- **`gh` runs only in the orchestrator.** The three sub-agents are headless: a `gh`
  call that would raise a permission prompt gets none there, and hangs.

## Structure

```
.claude/
├── agents/
│   ├── tdd-test-writer.md    ← Agent 1 (RED)
│   ├── tdd-implementer.md    ← Agent 2 (GREEN)
│   └── tdd-reviewer.md       ← Agent 3 (REFACTOR)
└── skills/
    └── tdd/SKILL.md          ← /tdd skill (orchestrator)

tdd/
├── feature.template.md       ← Versioned template, never filled in place
├── feature.md                ← Working spec, gitignored, copied from the template
├── REVIEW.template.md        ← Versioned template for the review report
├── REVIEW.md                 ← Working report, gitignored, written in the worktree
├── cleanup.sh                ← The cleanup phase, callable on its own
└── README.md
```

The two working files are **ignored by git**, not merely "not to be committed".
The phase agents stage with `git add -A`, so the spec once reached a feature
branch through the red phase's own commit and only a history rewrite got it out.
An ignored file cannot be swept in. `cleanup.sh` restores both by copying the
templates over them — it used to run `git checkout HEAD --`, which discards
uncommitted changes only and therefore announced a reset it had not performed the
moment a filled spec reached `develop`.

`.tdd-base-sha` and `.tdd-issue` are gitignored markers inside the worktree,
carrying the branch point and the issue number between phases. They die with the
worktree — and `.tdd-base-sha` is also how `cleanup.sh` finds which worktree to
clean, rather than rebuilding a branch name from the spec: `feature.md` is a
working file that may already describe the next feature by then.

Cleanup can also run on its own, from `.githooks/post-merge`, once you opt in
twice — `git config core.hooksPath .githooks` for the hooks at all, then
`git config --bool blue2th.tddAutoCleanup true` for this one. It is the only hook
that deletes anything, it never fails the merge that fired it, and it says nothing
unless there is exactly one open feature worktree whose pull request is merged.

## After the merge

```bash
/tdd cleanup      # or: tdd/cleanup.sh
```

It reads the pull request state before touching anything, and refuses on all three
outcomes that are not a merge:

| PR state              | What happens                                             |
|-----------------------|----------------------------------------------------------|
| `MERGED`              | Pull `develop`, remove the worktree, delete the branch, reset the templates |
| `OPEN`                | Refuses, and says whether CI is red or it waits on a human |
| `CLOSED`, not merged  | Refuses — the work was abandoned, deleting it is your call |

The order matters: `develop` is pulled **first**, otherwise `git branch -d` refuses
a branch it does not yet know is merged.

`cleanup.sh` lives outside the skill on purpose, so a `post-merge` hook can call it
too (#17).

## Rust notes

- Async tests: `#[tokio::test]`
- Unit tests: `#[cfg(test)]` module inside the source file
- Integration tests: in each crate's `tests/`
- Server route tests: `blue2th-server/tests/`, via `tower`'s `oneshot`
- Proto: serde round-trips in `lib.rs`
- Hardware (BlueZ, PipeWire, an actual audio device) is not test-runnable. Cover the
  pure logic and leave the hardware boundary to manual testing.
- Run with `cargo test --workspace`. The gates are the three commands in `CLAUDE.md`,
  plus `dx build --platform android --package blue2th-frontend` when the mobile
  layer is touched.
