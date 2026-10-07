#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Contract of scripts/lib/language-servers.sh, which tdd/cleanup.sh sources:
# `stop_language_servers <dir>` stops the rust-analyzer processes whose working
# directory is <dir> or below it, and nothing else. Serena starts one per
# project it activates and leaves the /tdd worktree's running when it switches
# back to the base checkout; its flycheck then recreated target/ while
# `git worktree remove` emptied the directory, and the removal failed half-way.
# Sourced by scripts/tests/run.sh; each test_* runs in its own subshell.
#
# The servers are stand-ins: a copy of `sleep` named after the real binary, so
# the process name the function matches on is the real one. Each test sources
# the helper in its own `bash -c`, as cleanup.sh does, so a missing helper fails
# the tests one by one instead of aborting the runner.

# start_named <name> <dir>: runs a copy of `sleep` named <name> with <dir> as
# its working directory, and sets $started to its pid once the process carries
# that name. Every process started here is stopped when the test ends, whether
# it passed or not.
start_named() {
    local name="$1" dir="$2" pid comm _
    mkdir -p "$tmp/bin" "$dir"
    [[ -x "$tmp/bin/$name" ]] || cp "$(command -v sleep)" "$tmp/bin/$name"
    (cd "$dir" && exec "$tmp/bin/$name" 60) </dev/null >/dev/null 2>&1 &
    pid=$!
    started_pids+=("$pid")
    # `|| true`: `set -e` holds inside the trap, and a stand-in the test already
    # stopped makes `kill` fail — which ended the test with status 1, silently,
    # before the temporary directory was removed.
    # shellcheck disable=SC2064  # the list and $tmp are meant to expand now
    trap "kill ${started_pids[*]} 2>/dev/null || true; rm -rf '$tmp'" EXIT
    # The name changes at `exec`; until then the process is still the subshell.
    # The kernel keeps 15 characters of it.
    for _ in $(seq 50); do
        comm="$(cat "/proc/$pid/comm" 2>/dev/null || true)"
        [[ "$comm" == "${name:0:15}" ]] && break
        sleep 0.1
    done
    [[ "$comm" == "${name:0:15}" ]] || fail "stand-in $name never started (comm: '$comm')"
    started="$pid"
}
started_pids=()

alive() {
    kill -0 "$1" 2>/dev/null
}

# Criterion: a rust-analyzer working in the worktree, and its proc-macro server
# working in a directory below it, are both stopped — and gone by the time the
# function returns, so `git worktree remove` cannot race them.
test_language_servers_stops_the_servers_working_in_the_directory() {
    local server macros
    start_named rust-analyzer "$tmp/worktree"
    server="$started"
    start_named rust-analyzer-proc-macro-srv "$tmp/worktree/blue2th-server"
    macros="$started"

    assert_succeeds bash -c 'source "$SCRIPTS_DIR/lib/language-servers.sh" && stop_language_servers "$1"' \
        _ "$tmp/worktree"

    alive "$server" && fail "rust-analyzer in the worktree still runs"
    alive "$macros" && fail "the proc-macro server below the worktree still runs"
    assert_contains "$stdout" "$server" "the report names the stopped server"
    true
}

# Criterion (guard): a server working anywhere else is left alone — including a
# sibling directory whose path starts with the worktree's. Near-miss: a plain
# prefix match, which would stop `…/worktree-2` along with `…/worktree`.
test_language_servers_leaves_a_server_working_elsewhere() {
    local sibling other
    mkdir -p "$tmp/worktree"
    start_named rust-analyzer "$tmp/worktree-2"
    sibling="$started"
    start_named rust-analyzer "$tmp/elsewhere"
    other="$started"

    assert_succeeds bash -c 'source "$SCRIPTS_DIR/lib/language-servers.sh" && stop_language_servers "$1"' \
        _ "$tmp/worktree"

    alive "$sibling" || fail "a server in the sibling worktree-2 was stopped"
    alive "$other" || fail "a server elsewhere was stopped"
}

# Criterion (guard): only language servers are stopped. A shell, an editor or a
# `dx serve` someone left in the worktree is theirs. Near-miss: matching on the
# working directory alone.
test_language_servers_leaves_other_programs_in_the_directory() {
    local dx
    start_named dx "$tmp/worktree"
    dx="$started"

    assert_succeeds bash -c 'source "$SCRIPTS_DIR/lib/language-servers.sh" && stop_language_servers "$1"' \
        _ "$tmp/worktree"

    alive "$dx" || fail "a program that is not a language server was stopped"
}

# Criterion (guard): an empty directory, or the root, is refused with exit 2
# and stops nothing. Near-miss: "" turns the `"$dir"/*` pattern into `/*`,
# which every working directory matches.
test_language_servers_refuses_an_empty_or_root_directory() {
    local anywhere dir
    start_named rust-analyzer "$tmp/elsewhere"
    anywhere="$started"

    for dir in "" "/"; do
        run bash -c 'source "$SCRIPTS_DIR/lib/language-servers.sh" && stop_language_servers "$1"' \
            _ "$dir"
        assert_eq 2 "$status" "exit status for directory '$dir'"
        assert_contains "$stderr" "stop_language_servers" "the refusal names the function"
    done
    alive "$anywhere" || fail "a refused call stopped a server"
}

# Criterion: tdd/cleanup.sh stops the servers before it removes the worktree.
# Read from the script's text: running cleanup.sh needs gh and a merged pull
# request, which this suite has neither of.
test_cleanup_stops_the_language_servers_before_removing_the_worktree() {
    local script="$REPO_ROOT/tdd/cleanup.sh" stop remove
    assert_contains "$(cat "$script")" 'lib/language-servers.sh' "cleanup.sh sources the helper"
    stop="$(grep -n 'stop_language_servers "$WORKTREE_PATH"' "$script" | head -1 | cut -d: -f1)"
    remove="$(grep -n 'git worktree remove "$WORKTREE_PATH"' "$script" | head -1 | cut -d: -f1)"
    [[ -n "$stop" ]] || fail "cleanup.sh never calls stop_language_servers on the worktree"
    [[ -n "$remove" ]] || fail "cleanup.sh no longer removes the worktree"
    (( stop < remove )) || fail "cleanup.sh stops the servers (line $stop) after the removal (line $remove)"
}
