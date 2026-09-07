#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# sign-apk.sh <unsigned.apk> <out.apk>: zipalign, sign with apksigner, verify.
#
# The keystore comes from ANDROID_KEYSTORE_FILE (a path) or ANDROID_KEYSTORE_B64
# (its base64, as a GitHub secret carries it); ANDROID_KEYSTORE_PASSWORD,
# ANDROID_KEY_ALIAS and ANDROID_KEY_PASSWORD are required. EXPECTED_CERT_SHA256,
# when set, must equal the signing certificate's SHA-256, in either the form
# `apksigner --print-certs` prints or the one `keytool -list -v` prints.
#
# Why the guards: the signing key is the one irreversible item of the release
# (docs/PUBLISHING.md, section 03). Every input is checked before any file is
# created, because a partially-run signing leaves a decoded keystore behind;
# an empty secret is refused rather than passed on, because an empty value is
# a wildcard in every predicate downstream (CLAUDE.md); and the fingerprint
# check exists because an APK signed by the wrong key installs fine and only
# refuses to update the one already on the phone. Passwords reach apksigner
# through `env:` so they never appear in argv or in a process listing.
#
# Exit codes: 0 signed and verified, 1 refused input or signing failure,
# 2 zipalign or apksigner missing.
set -euo pipefail

usage() {
    echo "usage: $(basename "$0") <unsigned.apk> <out.apk>" >&2
    echo "  env: ANDROID_KEYSTORE_FILE | ANDROID_KEYSTORE_B64, ANDROID_KEYSTORE_PASSWORD," >&2
    echo "       ANDROID_KEY_ALIAS, ANDROID_KEY_PASSWORD, [EXPECTED_CERT_SHA256], [BUILD_TOOLS_VERSION]" >&2
}

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib/build-tools.sh
source "$here/lib/build-tools.sh"

if [[ "${1:-}" == "-h" ]]; then
    usage
    exit 0
fi
if [[ $# -ne 2 ]]; then
    usage
    exit 1
fi

input="$1"
out="$2"

if [[ -z "$input" || -z "$out" ]]; then
    echo "sign-apk.sh: empty path argument" >&2
    usage
    exit 1
fi

# ── Inputs, all of them, before any file exists ──────────────────────────────

if [[ ! -f "$input" ]]; then
    echo "sign-apk.sh: input APK not found: '$input'" >&2
    exit 1
fi

# The caller owns the output directory (`dist/<version>/` in the workflow):
# creating it here would also create whatever a mistyped path names.
out_dir="$(dirname "$out")"
if [[ ! -d "$out_dir" ]]; then
    echo "sign-apk.sh: output directory not found: '$out_dir'" >&2
    exit 1
fi

for name in ANDROID_KEYSTORE_PASSWORD ANDROID_KEY_ALIAS ANDROID_KEY_PASSWORD; do
    if [[ -z "${!name:-}" ]]; then
        echo "sign-apk.sh: $name is unset or empty" >&2
        exit 1
    fi
done

keystore_file="${ANDROID_KEYSTORE_FILE:-}"
keystore_b64="${ANDROID_KEYSTORE_B64:-}"
if [[ -n "$keystore_file" ]]; then
    if [[ ! -f "$keystore_file" ]]; then
        echo "sign-apk.sh: ANDROID_KEYSTORE_FILE not found: '$keystore_file'" >&2
        exit 1
    fi
elif [[ -z "$keystore_b64" ]]; then
    echo "sign-apk.sh: no keystore: set ANDROID_KEYSTORE_FILE (a path) or ANDROID_KEYSTORE_B64 (its base64)" >&2
    exit 1
fi

# Set-but-empty is an error, not "no check": an empty
# scripts/android-release-cert.sha256 would otherwise accept any key.
expected_cert=""
if [[ -n "${EXPECTED_CERT_SHA256+x}" ]]; then
    expected_cert="$(echo "$EXPECTED_CERT_SHA256" | tr -d ': \n' | tr '[:upper:]' '[:lower:]')"
    if [[ ! "$expected_cert" =~ ^[0-9a-f]{64}$ ]]; then
        echo "sign-apk.sh: EXPECTED_CERT_SHA256 is empty or not a SHA-256 fingerprint: '${EXPECTED_CERT_SHA256}'" >&2
        exit 1
    fi
fi

zipalign="$(locate_build_tool zipalign)" || exit 2
apksigner="$(locate_build_tool apksigner)" || exit 2

# ── Temporary files, removed whatever happens next ───────────────────────────

workdir="$(mktemp -d)"
remove_out=0
cleanup() {
    rm -rf "$workdir"
    if [[ "$remove_out" -eq 1 ]]; then
        rm -f "$out"
    fi
}
trap cleanup EXIT

if [[ -z "$keystore_file" ]]; then
    keystore_file="$workdir/keystore.p12"
    (umask 077 && echo "$keystore_b64" | base64 -d >"$keystore_file") || {
        echo "sign-apk.sh: ANDROID_KEYSTORE_B64 is not valid base64" >&2
        exit 1
    }
    chmod 600 "$keystore_file"
fi

aligned="$workdir/aligned.apk"
if ! "$zipalign" -p 4 "$input" "$aligned"; then
    echo "sign-apk.sh: zipalign failed on '$input'" >&2
    exit 1
fi

# ── Sign ─────────────────────────────────────────────────────────────────────

# From here on a failure leaves a partial or wrongly-signed output, which the
# trap removes: a later step must never pick it up.
#
# v4 is off: that scheme lives in a `<out>.idsig` sidecar that only serves
# adb's incremental install, and a release would have to attest and publish
# one more file for nothing. v2 and v3 stay on, and they are what the phone
# and `apksigner verify` check.
remove_out=1
if ! "$apksigner" sign \
    --ks "$keystore_file" \
    --ks-type PKCS12 \
    --ks-pass env:ANDROID_KEYSTORE_PASSWORD \
    --key-pass env:ANDROID_KEY_PASSWORD \
    --ks-key-alias "$ANDROID_KEY_ALIAS" \
    --v4-signing-enabled false \
    --out "$out" \
    "$aligned"; then
    echo "sign-apk.sh: apksigner sign failed (wrong password, alias or keystore?)" >&2
    exit 1
fi

# ── Verify ───────────────────────────────────────────────────────────────────

if ! certs="$("$apksigner" verify --print-certs "$out")"; then
    echo "sign-apk.sh: apksigner verify failed on '$out'" >&2
    exit 1
fi

found_cert="$(echo "$certs" | sed -n 's/^Signer #1 certificate SHA-256 digest: *//p' | head -n1 | tr -d ' \r')"
if [[ ! "$found_cert" =~ ^[0-9a-f]{64}$ ]]; then
    echo "sign-apk.sh: could not read the signer certificate from apksigner verify output" >&2
    exit 1
fi

if [[ -n "$expected_cert" && "$found_cert" != "$expected_cert" ]]; then
    echo "sign-apk.sh: certificate mismatch: expected SHA-256 $expected_cert, found $found_cert" >&2
    exit 1
fi

remove_out=0
echo "signed '$out' with certificate SHA-256 $found_cert" >&2
