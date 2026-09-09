#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Contract of scripts/pr-type-label.sh <title>: the Conventional Commits type
# of a pull-request title becomes the label the workflow applies — `feat` →
# `enhancement`, `fix` → `bug`, `docs` → `documentation`, `ci` → `ci`,
# `refactor` → `refactor` — printed alone on stdout with exit 0. A valid type
# outside that table prints nothing and exits 3; a title that is not
# Conventional Commits exits 1 with the title named on stderr; a hook that
# cannot be read exits 2. `--all` lists the five mapped labels in a fixed
# order, which the workflow turns into its remove list. The title check reads
# the `PATTERN` line of `.githooks/commit-msg`, so the hook and the script
# cannot drift apart (#82).
# Sourced by scripts/tests/run.sh; each test_* runs in its own subshell.

script="$SCRIPTS_DIR/pr-type-label.sh"
hook="$REPO_ROOT/.githooks/commit-msg"
workflow="$REPO_ROOT/.github/workflows/pr-title.yml"
release_config="$REPO_ROOT/.github/release.yml"

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

# assert_unreadable_hook <command...>: exit status exactly 2 and nothing on
# stdout — the workflow fails its step on 2, so a script that answered 1
# would make a missing hook read as a refused title, and one that answered 3
# would strip the labels as if the title were valid.
assert_unreadable_hook() {
    run "$@"
    [[ "$status" -eq 2 ]] \
        || fail "expected an unreadable hook (exit 2), got exit $status for: $*"$'\n'"stdout: $stdout"$'\n'"stderr: $stderr"
    assert_eq "" "$stdout" "stdout when the hook cannot be read"
}

# relocate_script: copies the script under test to $tmp/scripts, so that the
# hook it resolves — `<script dir>/../.githooks/commit-msg` — is whatever the
# test puts at $tmp/.githooks/commit-msg, or nothing. Prints the copy's path.
# A missing script fails here, before any assertion on its behaviour could
# pass vacuously.
relocate_script() {
    [[ -f "$script" ]] || fail "script under test does not exist: $script"
    mkdir -p "$tmp/scripts" "$tmp/.githooks"
    cp "$script" "$tmp/scripts/pr-type-label.sh"
    echo "$tmp/scripts/pr-type-label.sh"
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

# Criterion: refuses an empty title, and says so — "empty title" on stderr
# rather than the generic refusal. The empty string is passed explicitly: a
# prefix match on "" fails open, and a label printed for an empty title is
# exactly the wildcard bug this project keeps meeting. The message is what
# proves the guard is its own check and not a side effect of the pattern.
test_pr_type_label_refuses_empty_title() {
    assert_fails "$script" ""
    assert_eq "" "$stdout" "stdout on refusal"
    assert_contains "$stderr" "empty title" "refusal message"
}

# Criterion: a title spanning two lines is refused, even when one of the
# lines is a valid title on its own. `grep` judges lines, so without an
# explicit guard the second line passes the check by itself and the script
# answers 3 — "valid title, no label" — for a title that is not valid.
test_pr_type_label_refuses_multi_line_title() {
    assert_fails "$script" $'feat: add a thing\nfix: repair a thing'
    assert_eq "" "$stdout" "stdout on refusal"
    assert_fails "$script" $'random words\nfeat: add a thing'
    assert_eq "" "$stdout" "stdout on refusal"
}

# Criterion: refuses a missing argument as well — the workflow passing
# nothing is a different bug from passing an empty expansion.
test_pr_type_label_refuses_missing_argument() {
    assert_fails "$script"
    assert_eq "" "$stdout" "stdout on refusal"
}

# Criterion: refuses a second argument. The workflow quotes `"$TITLE"`; an
# unquoted one would split a title on its spaces, and the first word of a
# title is never a label.
test_pr_type_label_refuses_extra_argument() {
    assert_fails "$script" "feat:" "add a thing"
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
# round on both.
test_pr_type_label_reads_pattern_from_the_commit_msg_hook() {
    local relocated
    relocated="$(relocate_script)"
    cat >"$tmp/.githooks/commit-msg" <<'HOOK'
#!/usr/bin/env bash
FIRST_LINE=$(head -1 "$1")
PATTERN='^(zzz)(\([a-z0-9/_-]+\))?!?: .{1,100}$'
echo "$FIRST_LINE" | grep -qE "$PATTERN"
HOOK

    run "$relocated" "zzz: x"
    [[ "$status" -eq 0 || "$status" -eq 3 ]] \
        || fail "expected the relocated script to accept 'zzz: x' under the fake hook, got exit $status"$'\n'"stderr: $stderr"

    run "$relocated" "feat: x"
    [[ "$status" -eq 1 ]] \
        || fail "expected the relocated script to refuse 'feat: x' under the fake hook (exit 1), got exit $status"$'\n'"stdout: $stdout"
    assert_eq "" "$stdout" "stdout on refusal under the fake hook"
}

# Criterion: no hook beside the script is exit 2, not a refusal — the title
# was never judged — and stderr names the path that was looked for.
test_pr_type_label_exits_2_when_the_hook_is_missing() {
    local relocated
    relocated="$(relocate_script)"
    assert_file_absent "$tmp/.githooks/commit-msg"
    assert_unreadable_hook "$relocated" "feat: add a thing"
    assert_contains "$stderr" ".githooks/commit-msg" "missing-hook message"
}

# Criterion: a hook with no `PATTERN='…'` line is exit 2 as well. The
# alternative — an empty pattern — would match every title, the wildcard
# case again, so the script must stop rather than judge with nothing.
test_pr_type_label_exits_2_when_the_hook_has_no_pattern_line() {
    local relocated
    relocated="$(relocate_script)"
    printf '#!/usr/bin/env bash\nexit 0\n' >"$tmp/.githooks/commit-msg"
    assert_unreadable_hook "$relocated" "feat: add a thing"
    assert_contains "$stderr" "PATTERN" "unreadable-pattern message"
}

# Criterion: the script and pr-title.yml read the hook with the same sed
# expression, character for character. Each reads the `PATTERN` line on its
# own, so a change to one of the two expressions — or to the hook's line
# shape — would let CI and the script judge the same title differently.
test_pr_type_label_reads_pattern_with_the_workflow_sed_expression() {
    local in_script in_workflow
    in_script="$(grep -o 'sed -n "[^"]*"' "$script")"
    in_workflow="$(grep -o 'sed -n "[^"]*"' "$workflow")"
    [[ -n "$in_script" ]] || fail "no sed expression found in $script"
    assert_eq "$in_workflow" "$in_script" "sed expression reading PATTERN"
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

# Criterion: `--all` and the mapping are one table. Every type the hook
# accepts is tried; the set of labels those types produce must be exactly
# the set `--all` prints — a label `--all` lists that no type produces would
# be removed for ever and never added, and one a type produces that `--all`
# omits would survive a retitle. The types come from the hook's own
# `PATTERN`, so the list tried is the list the script can be given.
test_pr_type_label_all_is_exactly_what_the_hook_types_map_to() {
    local pattern types type all mapped=""
    pattern="$(sed -n "s/^PATTERN='\(.*\)'$/\1/p" "$hook")"
    types="$(printf '%s' "$pattern" | sed -nE 's/^\^\(([a-z|]+)\).*$/\1/p' | tr '|' '\n')"
    [[ -n "$types" ]] || fail "could not read the type alternation from $hook: '$pattern'"

    assert_succeeds "$script" --all
    all="$stdout"
    [[ -n "$all" ]] || fail "--all printed nothing"

    while IFS= read -r type; do
        run "$script" "$type: x"
        [[ "$status" -eq 0 || "$status" -eq 3 ]] \
            || fail "hook type '$type' neither labelled nor unmapped: exit $status"$'\n'"stderr: $stderr"
        if [[ "$status" -eq 0 ]]; then
            [[ -n "$stdout" ]] || fail "type '$type' exited 0 with no label"
            mapped+="$stdout"$'\n'
        fi
    done <<<"$types"

    assert_eq "$(printf '%s' "$all" | sort)" "$(printf '%s' "$mapped" | sort)" \
        "labels produced by the hook's types vs --all"
}

# Criterion: every label `--all` prints is one .github/release.yml knows —
# in a category or in `exclude` — so no labelled pull request falls into
# *Other changes* for want of a line there. The script's header claims the
# two files agree; this is what makes the claim checkable.
test_pr_type_label_every_label_is_classified_by_release_yml() {
    local label
    assert_file_exists "$release_config"
    assert_succeeds "$script" --all
    [[ -n "$stdout" ]] || fail "--all printed nothing"
    while IFS= read -r label; do
        grep -qE "labels: \[.*\b$label\b.*\]" "$release_config" \
            || fail "label '$label' is not listed in $release_config"
    done <<<"$stdout"
}

# ── Usage ────────────────────────────────────────────────────────────────────

# Criterion: `-h` prints a usage text naming the script and exits 0.
test_pr_type_label_help_prints_usage() {
    assert_succeeds "$script" -h
    assert_contains "$stdout" "pr-type-label.sh" "usage text"
}
