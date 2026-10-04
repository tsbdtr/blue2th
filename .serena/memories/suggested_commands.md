# Suggested commands

The exact, current command lines are in `CLAUDE.md` § *Quality Commands*. Copy them from there; the clippy line carries a long list of `-W` flags that must not be shortened.

- Format check, clippy, tests: all three run on the whole workspace (`--workspace`), see `CLAUDE.md`.
- Android cross-build, only when the mobile layer changed: `dx build --platform android --package blue2th-frontend`.
- Run the backend: `cargo run -p blue2th-server`.
- One crate's tests: `cargo test -p blue2th-server` (also `-p blue2th-proto`, `-p blue2th-frontend`).
- Hooks, once per clone: `git config core.hooksPath .githooks`.
- Knowledge graph: `graphify query "<question>"`, `graphify path "<A>" "<B>"`, `graphify explain "<concept>"`; `graphify update .` after code changes or on a fresh clone.
- TDD cycle: the `/tdd` skill (`.claude/skills/tdd/SKILL.md`); `tdd/cleanup.sh` after a merge.
- GitHub: `gh` for issues and pull requests.
