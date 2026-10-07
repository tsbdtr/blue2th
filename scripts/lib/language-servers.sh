#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Sourced, never run: defines `stop_language_servers <dir>`, which stops the
# rust-analyzer processes whose working directory is <dir> or below it, waits
# until they are gone, and prints one line per process it stopped.
#
# Serena starts a rust-analyzer for each project it activates, and the /tdd
# cycle activates every feature worktree. Switching back to the base checkout
# does not stop the worktree's, and its flycheck kept writing target/ while
# tdd/cleanup.sh removed the worktree: `git worktree remove` unregistered it,
# then failed on a directory that was no longer empty (#160).
#
# Only rust-analyzer and its proc-macro server are stopped, and only those
# working in that directory: anything else there — a shell, an editor, a
# `dx serve` — belongs to someone. Reads /proc, so on a system without it
# nothing matches and nothing is stopped.
#
# Returns 0 (stopped, or nothing to stop) or 2 (misuse: an empty directory, or /).

stop_language_servers() {
    local dir="${1:-}" pid cwd remaining _
    local -a stopped=()

    # An empty value would turn the "$dir"/* pattern below into /*, which every
    # working directory matches; / means the same thing.
    if [[ -z "$dir" || "$dir" == "/" ]]; then
        echo "stop_language_servers: refusing '${dir}' — pass the worktree's directory" >&2
        return 2
    fi
    dir="${dir%/}"

    # Matched on the process name, not the command line: an editor opened on a
    # file called rust-analyzer.toml is not a server. The kernel keeps 15
    # characters of the name, so the proc-macro server reads `rust-analyzer-p`.
    for pid in $(pgrep -u "$(id -u)" '^rust-analyzer' || true); do
        cwd="$(readlink "/proc/$pid/cwd" 2>/dev/null)" || continue
        case "$cwd" in
            "$dir" | "$dir"/*)
                if kill "$pid" 2>/dev/null; then
                    stopped+=("$pid")
                    echo "Stopped rust-analyzer $pid, still serving $cwd."
                fi
                ;;
        esac
    done

    (( ${#stopped[@]} > 0 )) || return 0

    # Gone before returning: the caller removes the directory next, and a
    # server still shutting down could write into it once more.
    for _ in $(seq 50); do
        remaining=0
        for pid in "${stopped[@]}"; do
            kill -0 "$pid" 2>/dev/null && remaining=1
        done
        (( remaining == 0 )) && return 0
        sleep 0.1
    done
    echo "stop_language_servers: a server in $dir had not exited after 5 s" >&2
    return 0
}
