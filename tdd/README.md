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
2. Claude fills `tdd/feature.md`, **including the `Layers touched` checkboxes**.
3. Run `/tdd all`.

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
├── feature.md                ← Current feature spec (filled by Claude)
├── REVIEW.md                 ← Review report template (overwritten in the worktree)
├── cleanup.sh                ← The cleanup phase, callable on its own
└── README.md
```

`.tdd-base-sha` and `.tdd-issue` are gitignored markers inside the worktree,
carrying the branch point and the issue number between phases. They die with the
worktree.

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
