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
#            does not link, but `ring`'s build script compiles C for the
#            target (#196): the NDK's clang and llvm-ar are handed to `cc`, from
#            ANDROID_NDK_HOME, or else the highest version under
#            $ANDROID_HOME/ndk/;
#   wasm:    the frontend's browser build. No --all-targets: the
#            dev-dependencies pull tokio `net` → mio, which does not compile
#            for wasm32.
#
# Exit codes: 0 every target is clean, 2 rustup, a required target or the NDK
# is missing (checked before any cargo call), otherwise the failing cargo's own.
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

# The NDK: ANDROID_NDK_HOME when set and not empty — an empty value would build
# "/toolchains/…" — otherwise the highest version under $ANDROID_HOME/ndk/,
# compared as versions (`sort -V`: "30.0.14904198" above "30.0.9" and "9.0.1").
ndk="${ANDROID_NDK_HOME:-}"
if [[ -z "$ndk" && -n "${ANDROID_HOME:-}" && -d "$ANDROID_HOME/ndk" ]]; then
    highest="$(find "$ANDROID_HOME/ndk" -mindepth 1 -maxdepth 1 -type d -printf '%f\n' | sort -V | tail -n 1)"
    # An empty ndk/ yields no version, and no version is no NDK.
    [[ -n "$highest" ]] && ndk="$ANDROID_HOME/ndk/$highest"
fi
if [[ -z "$ndk" ]]; then
    echo "clippy.sh: no Android NDK found; ring's build script needs its C compiler for aarch64-linux-android" >&2
    echo "Set ANDROID_NDK_HOME to the NDK, or install one under \$ANDROID_HOME/ndk/" >&2
    exit 2
fi
ndk_bin="$ndk/toolchains/llvm/prebuilt/linux-x86_64/bin"

echo "--- clippy (host) ---"
cargo clippy --workspace --all-targets -- "${FLAGS[@]}"

echo "--- clippy (android) ---"
# API 24 in the compiler's name: the app's minSdk.
CC_aarch64_linux_android="$ndk_bin/aarch64-linux-android24-clang" \
    AR_aarch64_linux_android="$ndk_bin/llvm-ar" \
    cargo clippy -p blue2th-frontend --target aarch64-linux-android --all-targets -- "${FLAGS[@]}"

echo "--- clippy (wasm) ---"
cargo clippy -p blue2th-frontend --target wasm32-unknown-unknown --no-default-features --features web -- "${FLAGS[@]}"
