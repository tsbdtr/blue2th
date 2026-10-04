# Task completion

A coding task is done when the gate in `CLAUDE.md` § *Quality Commands* passes, run from the workspace root:

1. `cargo fmt --check`
2. the full clippy line from `CLAUDE.md` (with `-D warnings` and every `-W` flag)
3. `cargo test --workspace`
4. only if `blue2th-frontend/` changed: the Android `dx build`

Then:

- `graphify update .` so the knowledge graph matches the code.
- New `.rs` file: SPDX header on line 1.
- New dependency: check `deny.toml`'s licence allowlist; never a GPL crate.
- Do not commit, push or open a pull request unless asked; never merge one.
- Inside a `/tdd` cycle the skill owns the sequence (`test:` → implementation → `refactor:` commits, pull request, cleanup): follow `.claude/skills/tdd/SKILL.md` instead of this list.
- Anything rendered or behind hardware cannot be claimed verified from tests: say what was not checked.
