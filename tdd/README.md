# TDD Workflow — blue2th

Red → Green → Refactor cycle driven by three Claude agents.

## Usage

```
/tdd [test|impl|review|all]
```

| Command      | Phase    | Agent            | Role                                 |
|--------------|----------|------------------|--------------------------------------|
| `/tdd test`  | RED      | tdd-test-writer  | Write failing tests                  |
| `/tdd impl`  | GREEN    | tdd-implementer  | Implement until tests pass           |
| `/tdd review`| REFACTOR | tdd-reviewer     | Refactor without breaking tests      |
| `/tdd all`   | Full     | all 3 in sequence| Red → Green → Refactor pipeline      |

## Before running

1. Describe the feature to Claude (in conversation)
2. Claude fills `tdd/feature.md` (user story, acceptance criteria, technical scope)
3. Run `/tdd all`

A git worktree is automatically created at `../blue2th-<feature-slug>` on branch `feat/<feature-slug>`.
Each feature is isolated — multiple features can run in parallel without conflicts.

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
└── README.md
```

## After the cycle

```bash
git -C ../blue2th-<slug> push -u origin feat/<slug>   # open a PR
git worktree remove ../blue2th-<slug>                  # clean up when merged
```

## Rust / Dioxus notes

- Async tests: `#[tokio::test]` (add `tokio` to dev-dependencies if needed)
- Unit tests: `#[cfg(test)]` module inside the source file
- Integration tests: in each crate's `tests/`
- Server functions: testable without the Dioxus runtime
- Run with: `cargo test --no-default-features --features server`
