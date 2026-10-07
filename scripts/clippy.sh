#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# The one definition of the lint (#167), run by the pre-commit hook, the CI
# `quality` job and every /tdd phase.
#
# A host-only `cargo clippy` never compiles the frontend's Android or browser
# code, so an item used only there reads as dead on the host — and the answer
# used to be an `allow(dead_code)` that then outlived its reason. Linting all
# three targets lets that code be gated with `cfg` instead, and keeps
# native-only code from reaching the wasm build as a warning.
#
#   host:    the whole workspace, tests included;
#   android: the frontend for aarch64-linux-android, tests included. clippy
#            does not link, so no NDK is needed;
#   wasm:    the frontend's browser build. No --all-targets: the
#            dev-dependencies pull tokio `net` → mio, which does not compile
#            for wasm32.
#
# Exit codes: 0 every target is clean, 2 rustup or a required target is
# missing (checked before any cargo call), otherwise the failing cargo's own.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

FLAGS=(
    -D warnings
    -W clippy::unwrap_used
    -W clippy::expect_used
    -W clippy::panic
    -W clippy::todo
    -W clippy::unreachable
    -W clippy::unimplemented
)

REQUIRED_TARGETS=(aarch64-linux-android wasm32-unknown-unknown)

if ! command -v rustup >/dev/null 2>&1; then
    echo "clippy.sh: rustup not found on PATH; it is needed to check the cross targets" >&2
    exit 2
fi

installed="$(rustup target list --installed)"
missing=()
for target in "${REQUIRED_TARGETS[@]}"; do
    # Whole-line match: armv7-linux-androideabi or wasm32-wasip1 must not
    # stand in for the target they merely contain.
    grep -qxF "$target" <<<"$installed" || missing+=("$target")
done
if ((${#missing[@]} > 0)); then
    echo "clippy.sh: missing Rust target(s): ${missing[*]}" >&2
    echo "Install with: rustup target add ${missing[*]}" >&2
    exit 2
fi

echo "--- clippy (host) ---"
cargo clippy --workspace --all-targets -- "${FLAGS[@]}"

echo "--- clippy (android) ---"
cargo clippy -p blue2th-frontend --target aarch64-linux-android --all-targets -- "${FLAGS[@]}"

echo "--- clippy (wasm) ---"
cargo clippy -p blue2th-frontend --target wasm32-unknown-unknown --no-default-features --features web -- "${FLAGS[@]}"
