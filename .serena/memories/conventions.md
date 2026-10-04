# Conventions

Index into `CLAUDE.md`; the rules and their reasons live there. Read the named section before writing code of that kind.

- **Language** (§ *Language*): everything in the repository is English. Commits follow Conventional Commits, enforced by `.githooks/commit-msg`.
- **Licence header** (§ *Licence header*): first line of every `.rs` file is the SPDX line; CI fails without it.
- **Error handling** (§ *Error Handling*): no `unwrap`/`expect`/`panic!`/`todo!`/`unreachable!` outside test code; Axum handlers return `Result<Json<T>, AppError>`; UI handlers surface both `Ok` and `Err` through state.
- **Naming** (§ *Naming*): tests are `test_<subject>_<expected_outcome>`.
- **Rust style** (§ *Rust Style*): every `clone()` carries a comment saying why; no `println!`/`dbg!` in production code.
- **Comments** (§ *Comments*): explain why; no future tense; a fact about values becomes a test, not a comment; cite an issue as bare provenance `(#N)`, never its state.
- **Empty values** (§ *The empty value is a wildcard*): guard emptiness in every prefix/substring predicate; reject an empty field at the parser.
- **Pull requests** (§ *Pull Requests*): branch `<type>/<slug>` off `develop`; never merge a pull request; one feature per pull request; name in the body what a diff hides.
- **Commits**: only when the user asks.
- **Mobile** (`AGENTS.md`): Dioxus 0.7 only — no `cx`, `Scope`, `use_state`. Nothing reaches the user through an HTML `title`.
- **Testing** (`docs/ARCHITECTURE.md` § *Testing philosophy*): hardware (BlueZ, PipeWire, speakers) and Dioxus components are not test-runnable; pure logic is tested, the rest goes to a spec's manual verification.
