#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Contract of scripts/pr-type-label.sh <title>: the Conventional Commits type
# of a pull-request title becomes the label the workflow applies — `feat` →
# `enhancement`, `fix` → `bug`, `docs` → `documentation`, `ci` → `ci`,
# `refactor` → `refactor` — printed alone on stdout with exit 0. A valid type
# outside that table prints nothing and exits 3; a title that is not
# Conventional Commits exits 1 with the title named on stderr. `--all` lists
# the five mapped labels in a fixed order, which the workflow turns into its
# remove list. The title check reads the `PATTERN` line of
# `.githooks/commit-msg`, so the hook and the script cannot drift apart (#82).
# Sourced by scripts/tests/run.sh; each test_* runs in its own subshell.

script="$SCRIPTS_DIR/pr-type-label.sh"

# assert_unmapped <title>: exit status exactly 3 and nothing on stdout — the
# workflow reads 3 as "valid title, no label to apply". assert_fails cannot
# express this: it wants exactly 1, the code for a refused title, and a script
# that confused the two would make the workflow strip labels from a valid
# `test:` pull request as if its title were broken.
assert_unmapped() {
    run "$script" "$1"
    [[ "$status" -eq 3 ]] \
        || fail "expected no label (exit 3), got exit $status for: $1"$'\n'"stdout: $stdout"$'\n'"stderr: $stderr"
    assert_eq "" "$stdout" "stdout for unmapped type '$1'"
}

# assert_label <expected-label> <title>: exit 0 and exactly that label.
assert_label() {
    assert_succeeds "$script" "$2"
    assert_eq "$1" "$stdout" "label for '$2'"
}

# ── Mapped types ─────────────────────────────────────────────────────────────

# Criterion: `feat` → `enhancement`.
test_pr_type_label_maps_feat_to_enhancement() {
    assert_label enhancement "feat: add a thing"
}

# Criterion: `fix` → `bug`.
test_pr_type_label_maps_fix_to_bug() {
    assert_label bug "fix: repair a thing"
}

# Criterion: `docs` → `documentation`.
test_pr_type_label_maps_docs_to_documentation() {
    assert_label documentation "docs: explain a thing"
}

# Criterion: `ci` → `ci`.
test_pr_type_label_maps_ci_to_ci() {
    assert_label ci "ci: gate a thing"
}

# Criterion: `refactor` → `refactor`.
test_pr_type_label_maps_refactor_to_refactor() {
    assert_label refactor "refactor: move a thing"
}

# Criterion: a scope changes nothing — the type is read before it.
test_pr_type_label_ignores_scope() {
    assert_label enhancement "feat(audio): drive PipeWire from a pw_main_loop"
    assert_label bug "fix(ui): handle connection error state"
    assert_label documentation "docs(releasing): describe the labels"
    assert_label ci "ci(labels): apply the type label"
    assert_label refactor "refactor(server): split audio.rs"
}

# Criterion: a scope may carry `/`, `_`, `-` and digits, exactly what the
# hook's pattern allows — a stricter reading here would refuse titles the
# hook lets through.
test_pr_type_label_accepts_scope_with_hook_allowed_punctuation() {
    assert_label bug "fix(a/b_c-1): keep the scope charset in step with the hook"
}

# Criterion: the breaking marker `!` changes nothing, bare or after a scope.
test_pr_type_label_ignores_breaking_marker() {
    assert_label enhancement "feat!: drop the old route"
    assert_label bug "fix!: change the error shape"
    assert_label enhancement "feat(proto)!: rename a DTO field"
    assert_label refactor "refactor(server)!: move the audio engine"
}

# ── Valid types without a label ──────────────────────────────────────────────

# Criterion: `test` gets no label — exit 3, empty stdout.
test_pr_type_label_gives_no_label_for_test() {
    assert_unmapped "test(ci): pin the type-to-label contract"
}

# Criterion: `chore` gets no label.
test_pr_type_label_gives_no_label_for_chore() {
    assert_unmapped "chore: bump a dependency"
}

# Criterion: `style` gets no label.
test_pr_type_label_gives_no_label_for_style() {
    assert_unmapped "style: run rustfmt"
}

# Criterion: `perf` gets no label.
test_pr_type_label_gives_no_label_for_perf() {
    assert_unmapped "perf: avoid a clone"
}

# Criterion: `build` gets no label.
test_pr_type_label_gives_no_label_for_build() {
    assert_unmapped "build: bump the NDK"
}

# Criterion: `revert` gets no label.
test_pr_type_label_gives_no_label_for_revert() {
    assert_unmapped "revert: undo the route change"
}

# Criterion: the scope and the marker do not turn an unmapped type into a
# mapped one, and do not turn "no label" into a refusal.
test_pr_type_label_keeps_unmapped_type_unmapped_with_scope_and_marker() {
    assert_unmapped "chore(deps)!: drop a dependency"
    assert_unmapped "test(audio): cover the offset math"
}

# ── Refused titles ───────────────────────────────────────────────────────────

# Criterion: a capitalised type is not Conventional Commits — the hook would
# refuse the commit, so the script refuses the title.
test_pr_type_label_refuses_capitalised_type() {
    assert_fails "$script" "Feat: add a thing"
    assert_eq "" "$stdout" "stdout on refusal"
}

# Criterion: a type without the `: ` separator is refused.
test_pr_type_label_refuses_missing_separator() {
    assert_fails "$script" "feat add a thing"
    assert_eq "" "$stdout" "stdout on refusal"
}

# Criterion: an empty scope `()` is refused — the hook's pattern wants at
# least one character between the parentheses.
test_pr_type_label_refuses_empty_scope() {
    assert_fails "$script" "feat(): add a thing"
    assert_eq "" "$stdout" "stdout on refusal"
}

# Criterion: a title with no type at all is refused.
test_pr_type_label_refuses_title_without_type() {
    assert_fails "$script" "random words about a thing"
    assert_eq "" "$stdout" "stdout on refusal"
}

# Criterion: refuses an empty title. The empty string is passed explicitly:
# a prefix match on "" fails open, and a label printed for an empty title is
# exactly the wildcard bug this project keeps meeting.
test_pr_type_label_refuses_empty_title() {
    assert_fails "$script" ""
    assert_eq "" "$stdout" "stdout on refusal"
}

# Criterion: refuses a missing argument as well — the workflow passing
# nothing is a different bug from passing an empty expansion.
test_pr_type_label_refuses_missing_argument() {
    assert_fails "$script"
    assert_eq "" "$stdout" "stdout on refusal"
}

# Criterion: the refusal names the title on stderr, so a failed workflow
# step reads without opening the script.
test_pr_type_label_names_the_refused_title_on_stderr() {
    assert_fails "$script" "Feat: add a thing"
    assert_contains "$stderr" "Feat: add a thing" "refusal message"
}

# ── One pattern, read from the hook ──────────────────────────────────────────

# Criterion: the title check is the `PATTERN` line of `.githooks/commit-msg`,
# read from the hook at `<script dir>/../.githooks/commit-msg` rather than
# copied into the script. Proof: the script relocated next to a hook whose
# pattern accepts only the type `zzz` must follow that hook — `zzz: x` passes
# the check (a label or none, but not a refusal) and `feat: x` is refused.
# A script carrying its own copy of the regex would answer the other way
# round on both, and a missing script fails here before anything runs.
test_pr_type_label_reads_pattern_from_the_commit_msg_hook() {
    [[ -f "$script" ]] || fail "script under test does not exist: $script"
    mkdir -p "$tmp/scripts" "$tmp/.githooks"
    cp "$script" "$tmp/scripts/pr-type-label.sh"
    cat >"$tmp/.githooks/commit-msg" <<'HOOK'
#!/usr/bin/env bash
FIRST_LINE=$(head -1 "$1")
PATTERN='^(zzz)(\([a-z0-9/_-]+\))?!?: .{1,100}$'
echo "$FIRST_LINE" | grep -qE "$PATTERN"
HOOK
    local relocated="$tmp/scripts/pr-type-label.sh"

    run "$relocated" "zzz: x"
    [[ "$status" -eq 0 || "$status" -eq 3 ]] \
        || fail "expected the relocated script to accept 'zzz: x' under the fake hook, got exit $status"$'\n'"stderr: $stderr"

    run "$relocated" "feat: x"
    [[ "$status" -eq 1 ]] \
        || fail "expected the relocated script to refuse 'feat: x' under the fake hook (exit 1), got exit $status"$'\n'"stdout: $stdout"
    assert_eq "" "$stdout" "stdout on refusal under the fake hook"
}

# Criterion: with the real hook, the type set is exactly the hook's — a type
# the hook does not know is refused even if it looks plausible.
test_pr_type_label_refuses_type_unknown_to_the_hook() {
    assert_fails "$script" "hotfix: patch a thing"
    assert_eq "" "$stdout" "stdout on refusal"
}

# ── --all ────────────────────────────────────────────────────────────────────

# Criterion: `--all` prints the five mapped labels, one per line, in this
# order and nothing else. The workflow subtracts the label it adds and passes
# the rest to `--remove-label`, so an extra line would strip a label the
# mapping never owned, and a missing one would leave a stale label behind.
test_pr_type_label_all_lists_the_five_mapped_labels_in_order() {
    assert_succeeds "$script" --all
    assert_eq $'enhancement\nbug\ndocumentation\nci\nrefactor' "$stdout" "--all output"
}

# Criterion: `--all` says nothing on stderr — the workflow captures stdout
# only, and a warning there would go unread.
test_pr_type_label_all_is_silent_on_stderr() {
    assert_succeeds "$script" --all
    assert_eq "" "$stderr" "stderr of --all"
}

# ── Usage ────────────────────────────────────────────────────────────────────

# Criterion: `-h` prints a usage text naming the script and exits 0.
test_pr_type_label_help_prints_usage() {
    assert_succeeds "$script" -h
    assert_contains "$stdout" "pr-type-label.sh" "usage text"
}
