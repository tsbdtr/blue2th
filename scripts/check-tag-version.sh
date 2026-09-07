#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# check-tag-version.sh <tag> <Cargo.toml>: exit 0 when the tag is exactly
# `v` + the `[workspace.package] version` of that manifest.
#
# The tag names the release and the manifest names what was built; the two
# come from different hands (a `git tag` and a version bump commit) and drift
# silently. A release whose APK says 0.1.0 under a v0.2.0 tag would install
# but never update, so the workflow stops here rather than on the phone.
#
# Exit codes: 0 match, 1 mismatch or refused input.
set -euo pipefail

usage() {
    echo "usage: $(basename "$0") <tag> <Cargo.toml>" >&2
    echo "  exits 0 when <tag> equals v<[workspace.package] version>" >&2
}

if [[ "${1:-}" == "-h" ]]; then
    usage
    exit 0
fi
if [[ $# -ne 2 ]]; then
    usage
    exit 1
fi

tag="$1"
manifest="$2"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Checked before reading the manifest: an empty tag would compare equal to
# nothing else, but it must be refused by name rather than as a mismatch.
if [[ -z "$tag" ]]; then
    echo "check-tag-version.sh: empty tag" >&2
    exit 1
fi

version="$("$here/workspace-version.sh" "$manifest")"

if [[ "$tag" != "v$version" ]]; then
    echo "check-tag-version.sh: tag '$tag' does not match [workspace.package] version '$version' (expected 'v$version') in '$manifest'" >&2
    exit 1
fi
