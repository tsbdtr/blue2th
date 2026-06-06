## Language

All files in this repository must be written in **English**: source code, comments, commit messages, configuration files, agent definitions, skill files, and documentation.

Commit messages must follow the **Conventional Commits** specification:
`<type>(<scope>): <description>` — e.g. `feat(bluetooth): add device signal strength`, `fix(ui): handle connection error state`, `test(scan): add unit tests for deduplication`.
Common types: `feat`, `fix`, `refactor`, `test`, `chore`, `docs`, `style`, `perf`.

## Quality Commands

```bash
cargo fmt --check
cargo clippy -- -D warnings -W clippy::unwrap_used -W clippy::expect_used -W clippy::panic -W clippy::todo -W clippy::unreachable -W clippy::unimplemented
cargo test
```

Install the pre-commit hook once with: `git config core.hooksPath .githooks`

## Code Guidelines

### Error Handling
- **No `unwrap()` or `expect()` outside `#[cfg(test)]`** — use `?` to propagate, or handle `None`/`Err` explicitly
- **Server functions** always return `Result<T, ServerFnError>` — propagate with `?`, never panic
- **UI event handlers** that call server functions must handle both `Ok` and `Err` — surface errors to the user via state (e.g. `Signal<Option<String>>` for an error message), never silently drop them
- **`panic!`, `todo!`, `unreachable!`** are forbidden outside test code

### Naming
- Functions, variables, modules: `snake_case`
- Types, traits, enums, variants: `PascalCase`
- Constants, statics: `SCREAMING_SNAKE_CASE`
- Test functions: `test_<subject>_<expected_outcome>`

### Rust Style
- Prefer `?` over `match`/`if let` for error propagation
- No `clone()` without a comment explaining why it is necessary
- No `println!` or `dbg!` in production code
- Use owned types (`String`, `Vec<T>`) for Dioxus props; use `&str`, `&[T]` in pure functions

## tdd
- **tdd** (`.claude/skills/tdd/SKILL.md`) — orchestrate the TDD cycle (red/green/refactor). Trigger: `/tdd`
- Before launching agents, describe the feature to Claude so it fills `tdd/feature.md`.
- Agents are defined in `.claude/agents/`: `tdd-test-writer`, `tdd-implementer`, `tdd-reviewer`.
When the user types `/tdd`, invoke the Skill tool with `skill: "tdd"` before doing anything else.

## graphify

This project has a knowledge graph at graphify-out/ with god nodes, community structure, and cross-file relationships.

Rules:
- For codebase questions, first run `graphify query "<question>"` when graphify-out/graph.json exists. Use `graphify path "<A>" "<B>"` for relationships and `graphify explain "<concept>"` for focused concepts. These return a scoped subgraph, usually much smaller than GRAPH_REPORT.md or raw grep output.
- If graphify-out/wiki/index.md exists, use it for broad navigation instead of raw source browsing.
- Read graphify-out/GRAPH_REPORT.md only for broad architecture review or when query/path/explain do not surface enough context.
- After modifying code, run `graphify update .` to keep the graph current (AST-only, no API cost).
