#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Contract of scripts/clippy.sh (#167), the one place the lint runs. It takes
# no argument, resolves the repository root from its own path, then:
#   - reads `rustup target list --installed` before any `cargo` call and
#     requires an exact line for `aarch64-linux-android` and for
#     `wasm32-unknown-unknown`; a missing one, or `rustup` itself missing,
#     exits 2 with the fix on stderr and never calls `cargo`;
#   - runs exactly three `cargo clippy`, host → Android → wasm, each ending in
#     `--` and the seven CLAUDE.md flags, each announced on stdout by a
#     `--- clippy (<target>) ---` header;
#   - stops at the first one that fails and exits non-zero.
# The pre-commit hook, the CI `quality` job, CLAUDE.md, the spec template and
# the /tdd agents all delegate to it, and the frontend keeps only the five
# dead-code allowances no target can do without.
# Sourced by scripts/tests/run.sh; each test_* runs in its own subshell.
#
# `cargo` and `rustup` are fakes put first on PATH: neither a real lint nor the
# real toolchain list is ever reached. Each fake appends its argv to
# $tmp/calls.log, one call per line, every argument in angle brackets so a
# merged or split argument shows: `cargo <clippy> <--workspace> …`.

script="$SCRIPTS_DIR/clippy.sh"
hook="$REPO_ROOT/.githooks/pre-commit"
workflow="$REPO_ROOT/.github/workflows/ci.yml"
frontend_src="$REPO_ROOT/blue2th-frontend/src"

# The host-only command every document carried before this change, verbatim.
host_only_command='cargo clippy --workspace --all-targets -- -D warnings -W clippy::unwrap_used -W clippy::expect_used -W clippy::panic -W clippy::todo -W clippy::unreachable -W clippy::unimplemented'

clippy_flags=(
    -D warnings
    -W clippy::unwrap_used
    -W clippy::expect_used
    -W clippy::panic
    -W clippy::todo
    -W clippy::unreachable
    -W clippy::unimplemented
)
host_argv=(clippy --workspace --all-targets -- "${clippy_flags[@]}")
android_argv=(clippy -p blue2th-frontend --target aarch64-linux-android --all-targets -- "${clippy_flags[@]}")
wasm_argv=(clippy -p blue2th-frontend --target wasm32-unknown-unknown --no-default-features --features web -- "${clippy_flags[@]}")

# `rustup target list --installed` on the dev PC (rustup, toolchain 1.98.1),
# captured 2026-10-07.
captured_targets='aarch64-linux-android
armv7-linux-androideabi
i686-linux-android
wasm32-unknown-unknown
x86_64-linux-android
x86_64-unknown-linux-gnu'

# ── Fakes ────────────────────────────────────────────────────────────────────

# call_line <program> <arg...>: one log line, in the fakes' own format.
call_line() {
    local program="$1"
    shift
    printf '%s' "$program"
    printf ' <%s>' "$@"
}

# install_fake_cargo: a `cargo` that logs its argv and its physical working
# directory, and exits 101 — cargo's own status for a failed lint — when it is
# called with `--target` followed by the content of $tmp/cargo-fails-on. An
# empty or absent file fails nothing: an empty target name must not match.
install_fake_cargo() {
    mkdir -p "$tmp/bin"
    cat >"$tmp/bin/cargo" <<'FAKE'
#!/usr/bin/env bash
{ printf 'cargo'; printf ' <%s>' "$@"; printf '\n'; } >>"$FAKE_DIR/calls.log"
pwd -P >>"$FAKE_DIR/cargo-cwd.log"
fails_on=""
[[ -f "$FAKE_DIR/cargo-fails-on" ]] && fails_on="$(cat "$FAKE_DIR/cargo-fails-on")"
if [[ -n "$fails_on" ]]; then
    previous=""
    for arg in "$@"; do
        if [[ "$previous" == "--target" && "$arg" == "$fails_on" ]]; then
            echo "fake cargo: lint failed for $fails_on" >&2
            exit 101
        fi
        previous="$arg"
    done
fi
exit 0
FAKE
    chmod +x "$tmp/bin/cargo"
}

# install_fake_rustup <installed-targets>: a `rustup` that logs its argv and
# answers `rustup target list --installed` with exactly <installed-targets>
# (nothing at all when it is empty). Any other call prints nothing and
# succeeds.
install_fake_rustup() {
    mkdir -p "$tmp/bin"
    if [[ -n "$1" ]]; then
        printf '%s\n' "$1" >"$tmp/installed-targets"
    else
        : >"$tmp/installed-targets"
    fi
    cat >"$tmp/bin/rustup" <<'FAKE'
#!/usr/bin/env bash
{ printf 'rustup'; printf ' <%s>' "$@"; printf '\n'; } >>"$FAKE_DIR/calls.log"
if [[ "$*" == "target list --installed" ]]; then
    cat "$FAKE_DIR/installed-targets"
fi
exit 0
FAKE
    chmod +x "$tmp/bin/rustup"
}

# use_fakes <installed-targets>: both fakes, first on PATH.
use_fakes() {
    install_fake_cargo
    install_fake_rustup "$1"
    export FAKE_DIR="$tmp"
    export PATH="$tmp/bin:$PATH"
}

# require_script: the script must exist and be executable before any assertion
# on its behaviour. Without this, "cargo never called" would pass vacuously on
# a script that is not there.
require_script() {
    [[ -x "$script" ]] || fail "script under test does not exist or is not executable: $script"
}

# all_calls / cargo_calls: the fakes' log, whole or cargo only. Empty when
# nothing was called.
all_calls() {
    [[ -f "$tmp/calls.log" ]] && cat "$tmp/calls.log"
    true
}

cargo_calls() {
    all_calls | grep '^cargo ' || true
}

# assert_lint_failure <command...>: a non-zero exit that is neither 2 (the
# code for a missing target or rustup — a lint failure is not that) nor 126/127
# (the command never ran at all).
assert_lint_failure() {
    run "$@"
    case "$status" in
        0) fail "expected a failing lint, got exit 0 for: $*"$'\n'"stdout: $stdout"$'\n'"stderr: $stderr" ;;
        2) fail "expected a failing lint, got exit 2 (missing target/tool) for: $*"$'\n'"stderr: $stderr" ;;
        126 | 127) fail "the command did not run (exit $status): $*"$'\n'"stderr: $stderr" ;;
    esac
}

# assert_refused_before_cargo: exit exactly 2 and no `cargo` call at all.
assert_refused_before_cargo() {
    run "$script"
    assert_eq 2 "$status" "exit status (stderr: $stderr)"
    assert_eq "" "$(cargo_calls)" "cargo calls after a refusal"
}

# add_hint: the `rustup target add …` command printed on stderr, cut after its
# last target name so surrounding quotes or punctuation do not count.
add_hint() {
    grep -oE 'rustup target add( [a-z0-9_-]+)+' <<<"$stderr" | head -1 || true
}

# ── The three invocations ────────────────────────────────────────────────────

# Criterion: exactly three `cargo clippy` calls, host → Android → wasm, with the
# exact argv of the spec — the seven flags after `--` on every one of them.
# Near-miss: the CI wasm step before this change, `-- -D warnings` alone.
# Also: `rustup target list --installed` is read before the first cargo call,
# and each run is announced by its header on stdout, in the same order.
test_clippy_runs_host_android_wasm_in_order_with_the_flags() {
    require_script
    use_fakes "$captured_targets"

    assert_succeeds "$script"

    local expected
    expected="$(call_line cargo "${host_argv[@]}")
$(call_line cargo "${android_argv[@]}")
$(call_line cargo "${wasm_argv[@]}")"
    assert_eq "$expected" "$(cargo_calls)" "cargo calls"

    local list_at cargo_at
    list_at="$(all_calls | grep -nxF "$(call_line rustup target list --installed)" | head -1 | cut -d: -f1 || true)"
    cargo_at="$(all_calls | grep -n '^cargo ' | head -1 | cut -d: -f1 || true)"
    [[ -n "$list_at" ]] || fail "rustup target list --installed was never read"$'\n'"calls: $(all_calls)"
    ((list_at < cargo_at)) || fail "the target list was read (call $list_at) after cargo (call $cargo_at)"

    local host_at android_at wasm_at
    host_at="$(grep -nxF -- '--- clippy (host) ---' <<<"$stdout" | head -1 | cut -d: -f1 || true)"
    android_at="$(grep -nxF -- '--- clippy (android) ---' <<<"$stdout" | head -1 | cut -d: -f1 || true)"
    wasm_at="$(grep -nxF -- '--- clippy (wasm) ---' <<<"$stdout" | head -1 | cut -d: -f1 || true)"
    [[ -n "$host_at" && -n "$android_at" && -n "$wasm_at" ]] \
        || fail "expected the three '--- clippy (host|android|wasm) ---' headers on stdout, got: $stdout"
    ((host_at < android_at && android_at < wasm_at)) \
        || fail "headers out of order (host $host_at, android $android_at, wasm $wasm_at)"
}

# Criterion: the script resolves the repository root from its own path, so it
# works from any working directory — a /tdd worktree included. The argv is
# pinned above, so no --manifest-path can stand in: cargo must run in the root.
# Near-miss: a script that leaves the caller's directory (here $tmp) alone.
test_clippy_runs_cargo_from_the_repo_root_whatever_the_cwd() {
    require_script
    use_fakes "$captured_targets"

    cd "$tmp"
    assert_succeeds "$script"

    local root expected
    root="$(cd "$REPO_ROOT" && pwd -P)"
    expected="$(printf '%s\n%s\n%s' "$root" "$root" "$root")"
    assert_eq "$expected" "$(cat "$tmp/cargo-cwd.log" 2>/dev/null || true)" "cargo working directories"
}

# Criterion (guard, every exit status counts): the middle invocation fails →
# non-zero, the step is named on stdout, and the wasm run never happens.
# Near-miss: a script that runs all three and keeps only the last status,
# which here would be wasm's 0.
test_clippy_stops_at_the_first_failing_target() {
    require_script
    use_fakes "$captured_targets"
    echo aarch64-linux-android >"$tmp/cargo-fails-on"

    assert_lint_failure "$script"

    local expected
    expected="$(call_line cargo "${host_argv[@]}")
$(call_line cargo "${android_argv[@]}")"
    assert_eq "$expected" "$(cargo_calls)" "cargo calls up to the failing one"
    assert_contains "$stdout" "--- clippy (android) ---" "the failing step is named"
    [[ "$stdout" != *"--- clippy (wasm) ---"* ]] || fail "the wasm step was announced after android failed"
}

# Criterion (guard, every exit status counts): only the last invocation fails →
# non-zero. Near-miss: a script that reports the first status, or pipes the
# runs through `tail` without pipefail.
test_clippy_fails_when_only_the_last_target_fails() {
    require_script
    use_fakes "$captured_targets"
    echo wasm32-unknown-unknown >"$tmp/cargo-fails-on"

    assert_lint_failure "$script"

    local expected
    expected="$(call_line cargo "${host_argv[@]}")
$(call_line cargo "${android_argv[@]}")
$(call_line cargo "${wasm_argv[@]}")"
    assert_eq "$expected" "$(cargo_calls)" "cargo calls"
}

# ── Required targets ─────────────────────────────────────────────────────────

# Criterion (guard, exact target match): three real Android-family and wasm
# targets, installed side by side on the dev PC, but not aarch64-linux-android.
# Near-miss: a substring check on `android` or `linux-android` accepts this list.
test_clippy_refuses_without_the_android_target() {
    require_script
    use_fakes 'armv7-linux-androideabi
x86_64-linux-android
wasm32-unknown-unknown'

    assert_refused_before_cargo
    assert_eq "rustup target add aarch64-linux-android" "$(add_hint)" "the fix printed on stderr"
}

# Criterion (guard, both targets required): aarch64-linux-android is there and
# wasm32-unknown-unknown is not. Near-misses: a check that returns after the
# first target it finds; and, through wasm32-wasip1 (a real target, synthetic
# in this list), a substring check on `wasm32`.
test_clippy_refuses_without_the_wasm_target() {
    require_script
    use_fakes 'aarch64-linux-android
wasm32-wasip1
x86_64-unknown-linux-gnu'

    assert_refused_before_cargo
    assert_eq "rustup target add wasm32-unknown-unknown" "$(add_hint)" "the fix printed on stderr"
}

# Criterion (guard, the empty value): rustup lists nothing → both targets are
# missing, not "no target missing". The hint names both, Android first, as
# CLAUDE.md writes the command.
test_clippy_refuses_on_an_empty_target_list() {
    require_script
    use_fakes ''

    assert_refused_before_cargo
    assert_eq "rustup target add aarch64-linux-android wasm32-unknown-unknown" "$(add_hint)" \
        "the fix printed on stderr"
}

# Criterion: rustup absent from PATH → exit 2, the message names rustup, cargo
# is never called. Every PATH entry holding a rustup is dropped; the fake
# cargo stays first, so a call to it would show in the log.
test_clippy_refuses_without_rustup() {
    require_script
    install_fake_cargo
    export FAKE_DIR="$tmp"

    local dir kept="" entries
    IFS=: read -ra entries <<<"$PATH"
    for dir in "${entries[@]}"; do
        [[ -n "$dir" && ! -e "$dir/rustup" ]] && kept="${kept:+$kept:}$dir"
    done
    export PATH="$tmp/bin:$kept"
    if command -v rustup >/dev/null; then
        fail "test setup: rustup still reachable at $(command -v rustup)"
    fi
    command -v bash >/dev/null || fail "test setup: no bash left on PATH ($PATH)"

    assert_refused_before_cargo
    assert_contains "$stderr" "rustup" "the refusal names rustup"
}

# ── The pre-commit hook ──────────────────────────────────────────────────────

# Criterion: the hook runs `cargo fmt --check`, then scripts/clippy.sh, then
# `cargo test --workspace` — read from the calls it actually makes, run from
# the repository root as git runs it.
test_pre_commit_runs_fmt_clippy_script_then_test() {
    require_script
    use_fakes "$captured_targets"

    assert_succeeds bash -c 'cd "$REPO_ROOT" && exec .githooks/pre-commit'

    local expected
    expected="$(call_line cargo fmt --check)
$(call_line cargo "${host_argv[@]}")
$(call_line cargo "${android_argv[@]}")
$(call_line cargo "${wasm_argv[@]}")
$(call_line cargo test --workspace)"
    assert_eq "$expected" "$(cargo_calls)" "cargo calls of the hook"
}

# Criterion: the inline clippy command is gone from the hook — it calls the
# script, so the lint has one definition. Near-miss: a hook that inlines the
# three invocations, which the call log above alone would accept.
test_pre_commit_delegates_clippy_to_the_script() {
    local text
    text="$(grep -v '^[[:space:]]*#' "$hook")"
    [[ "$text" == *"scripts/clippy.sh"* ]] || fail "the hook never calls scripts/clippy.sh"
    [[ "$text" != *"cargo clippy"* ]] || fail "the hook still runs cargo clippy itself"
}

# Criterion: a failing scripts/clippy.sh fails the hook, and `cargo test`
# never runs after it.
test_pre_commit_fails_when_clippy_fails() {
    require_script
    use_fakes "$captured_targets"
    echo aarch64-linux-android >"$tmp/cargo-fails-on"

    assert_lint_failure bash -c 'cd "$REPO_ROOT" && exec .githooks/pre-commit'

    local calls
    calls="$(cargo_calls)"
    assert_contains "$calls" "$(call_line cargo "${android_argv[@]}")" "the hook reached the Android lint"
    [[ "$calls" != *"$(call_line cargo "${wasm_argv[@]}")"* ]] || fail "the wasm lint ran after android failed"
    [[ "$calls" != *"$(call_line cargo test --workspace)"* ]] || fail "cargo test ran after a failed lint"
}

# ── CI ───────────────────────────────────────────────────────────────────────

# job_block <job>: the lines of one job of ci.yml, from its key to the next
# line at the jobs' own indentation (a comment there belongs to the next job).
job_block() {
    awk -v key="  $1:" '
        $0 == key { inside = 1; print; next }
        inside && /^  [^ ]/ { exit }
        inside { print }
    ' "$workflow"
}

# Criterion: the `quality` job lints through the script, and its toolchain step
# installs both cross targets. Near-miss: the job as it was, with the host-only
# command inline and no target.
test_ci_quality_job_runs_the_clippy_script_with_both_cross_targets() {
    local block
    block="$(job_block quality)"
    [[ -n "$block" ]] || fail "no quality job in $workflow"

    grep -qE '^[[:space:]]+run: (\./)?scripts/clippy\.sh[[:space:]]*$' <<<"$block" \
        || fail "the quality job never runs scripts/clippy.sh"
    grep -qE '^[[:space:]]+targets: aarch64-linux-android, wasm32-unknown-unknown[[:space:]]*$' <<<"$block" \
        || fail "the quality job's toolchain step does not install both cross targets"
    if grep -v '^[[:space:]]*#' <<<"$block" | grep -q 'cargo clippy'; then
        fail "the quality job still runs cargo clippy inline"
    fi
}

# Criterion: the `web` job drops its wasm clippy — moved into `quality`, not
# lost — but keeps `dx build (web)` and the wasm target that build needs.
test_ci_web_job_keeps_its_build_but_drops_its_clippy() {
    local block
    block="$(job_block web)"
    [[ -n "$block" ]] || fail "no web job in $workflow"

    if grep -qF 'clippy (wasm)' <<<"$block"; then
        fail "the web job still has its clippy (wasm) step"
    fi
    if grep -v '^[[:space:]]*#' <<<"$block" | grep -q 'cargo clippy'; then
        fail "the web job still runs cargo clippy"
    fi
    grep -qE '^[[:space:]]+run: dx build --platform web --package blue2th-frontend[[:space:]]*$' <<<"$block" \
        || fail "the web job lost its dx build (web)"
    grep -qE '^[[:space:]]+targets:.*wasm32-unknown-unknown' <<<"$block" \
        || fail "the web job lost the wasm32-unknown-unknown target its build needs"
}

# ── Documents ────────────────────────────────────────────────────────────────

# Criterion: CLAUDE.md's Quality Commands name scripts/clippy.sh in place of
# the inline host command, and the one-time setup it needs.
test_claude_md_quality_commands_name_the_clippy_script() {
    local section
    section="$(awk '/^## Quality Commands/ { inside = 1; next } inside && /^## / { exit } inside { print }' \
        "$REPO_ROOT/CLAUDE.md")"
    [[ -n "$section" ]] || fail "CLAUDE.md has no Quality Commands section"

    [[ "$section" == *"scripts/clippy.sh"* ]] || fail "Quality Commands never name scripts/clippy.sh"
    [[ "$section" == *"rustup target add aarch64-linux-android wasm32-unknown-unknown"* ]] \
        || fail "Quality Commands never give the setup: rustup target add aarch64-linux-android wasm32-unknown-unknown"
    [[ "$section" != *"$host_only_command"* ]] || fail "Quality Commands still list the host-only clippy command"
}

# Criterion: the spec template and the implementer and reviewer agents name
# scripts/clippy.sh instead of the host-only command.
test_tdd_template_and_agents_name_the_clippy_script() {
    local file text
    for file in tdd/feature.template.md .claude/agents/tdd-implementer.md .claude/agents/tdd-reviewer.md; do
        text="$(cat "$REPO_ROOT/$file")"
        [[ "$text" == *"scripts/clippy.sh"* ]] || fail "$file never names scripts/clippy.sh"
        [[ "$text" != *"$host_only_command"* ]] || fail "$file still carries the host-only clippy command"
    done
}

# ── Dead-code allowances in the frontend ─────────────────────────────────────

# dead_code_items: for every `allow(dead_code)` under blue2th-frontend/src, the
# name of the item it is attached to — the first line after it that is neither
# an attribute nor a comment — one per line, sorted.
dead_code_items() {
    find "$frontend_src" -name '*.rs' -print0 | sort -z | xargs -0 awk '
        /allow\(dead_code\)/ { pending = 1; next }
        pending && /^[[:space:]]*(#\[|\/\/)/ { next }
        pending { print; pending = 0 }
    ' | sed -E 's/^[[:space:]]+//; s/^pub(\([^)]*\))? //; s/^((async|unsafe) )*fn //; s/^(enum|struct|static|const|type|trait|mod) //; s/^([A-Za-z_][A-Za-z0-9_]*).*/\1/' \
        | LC_ALL=C sort
}

# Criterion: exactly five `allow(dead_code)` remain in blue2th-frontend/src,
# and they are the ones the census found still needed — group 3
# (`browser_settings`, `PageEvent`, `presence_for`, `browser_presence_post`)
# and `ClientKind::Phone`. Near-miss: the 37 of develop, the 27 obsolete ones
# of backend.rs among them; clippy alone cannot tell, since an unneeded allow
# never warns.
test_frontend_keeps_exactly_the_five_needed_dead_code_allowances() {
    local expected
    expected="$(printf '%s\n' PageEvent Phone browser_presence_post browser_settings presence_for | LC_ALL=C sort)"
    assert_eq "$expected" "$(dead_code_items)" "items carrying allow(dead_code)"
}

# attributes_of <file> <declaration-regex>: the attribute lines directly above
# the first line matching the regex (doc comments in between are skipped).
# Exits 1 when no line matches. The regex goes through the environment, not
# `awk -v`, which would eat its backslashes.
attributes_of() {
    DECLARATION="$2" awk '
        $0 ~ ENVIRON["DECLARATION"] { for (i = 1; i <= n; i++) print attrs[i]; found = 1; exit }
        /^[[:space:]]*#\[/ { attrs[++n] = $0; next }
        /^[[:space:]]*\/\/\// { next }
        { n = 0 }
        END { if (!found) exit 1 }
    ' "$1"
}

# has_android_cfg <attributes>: a line that is exactly
# `#[cfg(target_os = "android")]`. Near-misses: the
# `cfg_attr(not(target_os = "android"), allow(dead_code))` these items carry
# today, and `cfg(not(target_os = "android"))` — both contain the predicate.
has_android_cfg() {
    sed -E 's/^[[:space:]]+//; s/[[:space:]]+$//' <<<"$1" | grep -qxF '#[cfg(target_os = "android")]'
}

# assert_android_only <file> <declaration-regex> <label>
assert_android_only() {
    local attrs
    attrs="$(attributes_of "$frontend_src/$1" "$2")" || fail "$3: declaration not found in $1"
    has_android_cfg "$attrs" || fail "$3 is not gated by #[cfg(target_os = \"android\")]; attributes: $attrs"
    [[ "$attrs" != *dead_code* ]] || fail "$3 still carries a dead_code allowance: $attrs"
}

# Criterion: group 2 moves to `#[cfg(target_os = "android")]` and carries no
# `allow(dead_code)`. `JniError::new` counts as gated when the `impl JniError`
# block holding it is.
test_android_only_items_are_gated_not_allowed() {
    assert_android_only lifecycle.rs '^pub fn report\(' 'lifecycle::report'
    assert_android_only lifecycle.rs '^pub fn report_gone_blocking\(' 'lifecycle::report_gone_blocking'
    assert_android_only lifecycle.rs '^(pub(\([a-z]+\))? )?const GONE_REPORT_TIMEOUT:' 'lifecycle::GONE_REPORT_TIMEOUT'
    assert_android_only backend.rs '^pub async fn report_presence\(' 'backend::report_presence'
    assert_android_only jni_util.rs '^pub struct JniError\(' 'jni_util::JniError'

    local fn_attrs impl_attrs
    fn_attrs="$(attributes_of "$frontend_src/jni_util.rs" '^[[:space:]]+pub fn new\(')" \
        || fail "JniError::new not found in jni_util.rs"
    impl_attrs="$(attributes_of "$frontend_src/jni_util.rs" '^impl JniError \{')" \
        || fail "impl JniError not found in jni_util.rs"
    has_android_cfg "$fn_attrs" || has_android_cfg "$impl_attrs" \
        || fail "JniError::new is not gated by #[cfg(target_os = \"android\")]"
    [[ "$fn_attrs" != *dead_code* ]] || fail "JniError::new still carries a dead_code allowance: $fn_attrs"
}
