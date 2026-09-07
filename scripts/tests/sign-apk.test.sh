#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Contract of scripts/sign-apk.sh <unsigned.apk> <out.apk>: zipalign, sign with
# apksigner, verify. The keystore and its secrets come from the environment —
# ANDROID_KEYSTORE_FILE (a path) or ANDROID_KEYSTORE_B64 (its base64), plus
# ANDROID_KEYSTORE_PASSWORD, ANDROID_KEY_ALIAS, ANDROID_KEY_PASSWORD.
# EXPECTED_CERT_SHA256, when set, must match the signing certificate.
#
# These tests run the real keytool, zipalign and apksigner (the runner refuses
# to start without them). Each test generates its own PKCS12 keystore, and
# points the script's TMPDIR at an empty directory so that "no decoded keystore
# remains" is checked by listing that directory, not by guessing a file name.
# Sourced by scripts/tests/run.sh; each test_* runs in its own subshell.

script="$SCRIPTS_DIR/sign-apk.sh"

# Generates $tmp/test.p12 (store and key password `testpass`, alias `test`)
# and records its certificate fingerprint in the two forms the tools print:
#   cert_sha256_keytool   — `C7:41:EB:...`, colon-separated upper-case
#   cert_sha256_apksigner — `c741eb...`, bare lower-case hex
# The -J locale flags keep keytool's labels in English: under a French locale
# the `SHA256:` line is spelled differently and the parse below finds nothing.
make_keystore() {
    "$KEYTOOL" -genkeypair -storetype PKCS12 -keyalg RSA -keysize 2048 -validity 1 \
        -dname 'CN=test' -alias test -keystore "$tmp/test.p12" \
        -storepass testpass -keypass testpass >/dev/null 2>&1
    cert_sha256_keytool="$("$KEYTOOL" -list -v -J-Duser.language=en -J-Duser.country=US \
        -keystore "$tmp/test.p12" -storepass testpass -alias test 2>/dev/null \
        | grep -m1 'SHA256:' | sed 's/.*SHA256: *//')"
    [[ "$cert_sha256_keytool" =~ ^([0-9A-F]{2}:){31}[0-9A-F]{2}$ ]] \
        || fail "could not read the fixture certificate fingerprint from keytool: '$cert_sha256_keytool'"
    cert_sha256_apksigner="$(echo "$cert_sha256_keytool" | tr -d ':' | tr '[:upper:]' '[:lower:]')"
}

# The complete, valid environment; tests remove or blank one entry at a time.
# TMPDIR is the scratch directory the script decodes into, empty before and
# expected empty after.
setup_fixture() {
    make_keystore
    make_unsigned_apk "$tmp/unsigned.apk"
    mkdir "$tmp/scratch"
    unsigned_sha="$(sha256sum "$tmp/unsigned.apk" | cut -d' ' -f1)"
    out="$tmp/signed.apk"
    full_env=(
        "ANDROID_KEYSTORE_FILE=$tmp/test.p12"
        "ANDROID_KEYSTORE_PASSWORD=testpass"
        "ANDROID_KEY_ALIAS=test"
        "ANDROID_KEY_PASSWORD=testpass"
        "TMPDIR=$tmp/scratch"
    )
}

# sign_with <env assignments...>: runs the script with exactly the given
# environment on top of the runner's (ANDROID_HOME and friends stay). A
# variable not listed is unset for the script — `env` starts from the runner's
# environment, which never carries these secrets.
sign_with() {
    run env -u ANDROID_KEYSTORE_FILE -u ANDROID_KEYSTORE_B64 -u ANDROID_KEYSTORE_PASSWORD \
        -u ANDROID_KEY_ALIAS -u ANDROID_KEY_PASSWORD -u EXPECTED_CERT_SHA256 \
        "$@" "$script" "$tmp/unsigned.apk" "$out"
}

assert_signed_output_is_valid() {
    assert_file_exists "$out"
    assert_succeeds "$APKSIGNER" verify --print-certs "$out"
    assert_contains "$stdout" "$cert_sha256_apksigner" "output signed by the fixture certificate"
    assert_succeeds "$ZIPALIGN" -c 4 "$out"
    assert_eq "$unsigned_sha" "$(sha256sum "$tmp/unsigned.apk" | cut -d' ' -f1)" \
        "the unsigned input is left untouched"
}

# Guards the fixture's meaning: the tests below prove nothing if the
# unsigned input already verifies and is already aligned.
test_sign_apk_fixture_is_neither_signed_nor_aligned_before_signing() {
    setup_fixture
    run "$APKSIGNER" verify "$tmp/unsigned.apk"
    [[ "$status" -ne 0 ]] || fail "the unsigned fixture must not pass apksigner verify"
    run "$ZIPALIGN" -c 4 "$tmp/unsigned.apk"
    [[ "$status" -ne 0 ]] || fail "the unsigned fixture must not already be 4-byte aligned"
}

# Criterion: with ANDROID_KEYSTORE_FILE, the output verifies, is aligned, and
# was signed by the supplied certificate.
test_sign_apk_signs_aligns_and_verifies_with_keystore_file() {
    setup_fixture
    sign_with "${full_env[@]}"
    assert_eq 0 "$status" "exit status"$'\n'"stderr: $stderr"
    assert_signed_output_is_valid
}

# Criterion: with ANDROID_KEYSTORE_B64 instead of a path, the keystore is
# decoded, used, and the decoded copy is gone once the script returns.
test_sign_apk_accepts_keystore_as_base64_and_removes_the_decoded_copy() {
    setup_fixture
    local b64
    b64="$(base64 -w0 "$tmp/test.p12")"
    sign_with "${full_env[@]:1}" "ANDROID_KEYSTORE_B64=$b64"
    assert_eq 0 "$status" "exit status"$'\n'"stderr: $stderr"
    assert_signed_output_is_valid
    assert_dir_empty "$tmp/scratch"
}

# Criterion: each of the three secrets, when unset, is refused before any
# file is created, and the message names the variable.
test_sign_apk_refuses_each_unset_secret_before_creating_any_file() {
    setup_fixture
    local name entry
    for name in ANDROID_KEYSTORE_PASSWORD ANDROID_KEY_ALIAS ANDROID_KEY_PASSWORD; do
        local without=()
        for entry in "${full_env[@]}"; do
            [[ "$entry" == "$name="* ]] || without+=("$entry")
        done
        sign_with "${without[@]}"
        assert_eq 1 "$status" "exit status with $name unset"
        assert_contains "$stderr" "$name" "refusal names the unset variable"
        assert_file_absent "$out"
        assert_dir_empty "$tmp/scratch"
    done
}

# Criterion: each of the three secrets, when set but empty, is refused the
# same way — an empty password must never reach apksigner as `pass:`.
test_sign_apk_refuses_each_empty_secret_before_creating_any_file() {
    setup_fixture
    local name entry
    for name in ANDROID_KEYSTORE_PASSWORD ANDROID_KEY_ALIAS ANDROID_KEY_PASSWORD; do
        local blanked=()
        for entry in "${full_env[@]}"; do
            if [[ "$entry" == "$name="* ]]; then
                blanked+=("$name=")
            else
                blanked+=("$entry")
            fi
        done
        sign_with "${blanked[@]}"
        assert_eq 1 "$status" "exit status with $name empty"
        assert_contains "$stderr" "$name" "refusal names the empty variable"
        assert_file_absent "$out"
        assert_dir_empty "$tmp/scratch"
    done
}

# Criterion: neither keystore variable set is refused, naming both, so the
# message tells the caller which two ways exist to supply it.
test_sign_apk_refuses_when_no_keystore_variable_is_set() {
    setup_fixture
    sign_with "${full_env[@]:1}"
    assert_eq 1 "$status" "exit status without a keystore"
    assert_contains "$stderr" "ANDROID_KEYSTORE_FILE" "refusal names the path variable"
    assert_contains "$stderr" "ANDROID_KEYSTORE_B64" "refusal names the base64 variable"
    assert_file_absent "$out"
    assert_dir_empty "$tmp/scratch"
}

# Criterion: both keystore variables empty is the same refusal — empty is not
# "unset the other way", it is still nothing to sign with.
test_sign_apk_refuses_when_both_keystore_variables_are_empty() {
    setup_fixture
    sign_with "${full_env[@]:1}" "ANDROID_KEYSTORE_FILE=" "ANDROID_KEYSTORE_B64="
    assert_eq 1 "$status" "exit status with empty keystore variables"
    assert_file_absent "$out"
    assert_dir_empty "$tmp/scratch"
}

# Criterion: a keystore path that does not exist is refused, naming it.
test_sign_apk_refuses_missing_keystore_file() {
    setup_fixture
    sign_with "${full_env[@]:1}" "ANDROID_KEYSTORE_FILE=$tmp/absent.p12"
    assert_eq 1 "$status" "exit status with a missing keystore file"
    assert_contains "$stderr" "$tmp/absent.p12" "refusal names the missing keystore"
    assert_file_absent "$out"
}

# Criterion: a missing input APK is refused before any file is created, and
# the message names the path.
test_sign_apk_refuses_missing_input_before_creating_any_file() {
    setup_fixture
    rm "$tmp/unsigned.apk"
    sign_with "${full_env[@]}"
    assert_eq 1 "$status" "exit status with a missing input"
    assert_contains "$stderr" "$tmp/unsigned.apk" "refusal names the missing input"
    assert_file_absent "$out"
    assert_dir_empty "$tmp/scratch"
}

# Criterion: when signing fails midway (wrong password), the decoded keystore
# is removed anyway and no output file is left behind. The base64 route is
# used so a decoded copy actually existed before the failure. A bad secret
# is a refused input, hence exit 1 — not apksigner's own status passed
# through.
test_sign_apk_leaves_no_keystore_or_output_after_a_wrong_password() {
    setup_fixture
    local b64
    b64="$(base64 -w0 "$tmp/test.p12")"
    sign_with "ANDROID_KEYSTORE_B64=$b64" "ANDROID_KEYSTORE_PASSWORD=wrong" \
        "ANDROID_KEY_ALIAS=test" "ANDROID_KEY_PASSWORD=wrong" "TMPDIR=$tmp/scratch"
    assert_eq 1 "$status" "exit status with a wrong password"
    assert_file_absent "$out"
    assert_dir_empty "$tmp/scratch"
}

# Criterion: EXPECTED_CERT_SHA256 equal to the signing certificate succeeds.
# The form tested is the one `apksigner verify --print-certs` prints —
# bare lower-case hex — which is what scripts/android-release-cert.sha256
# carries.
test_sign_apk_accepts_expected_cert_in_apksigner_form() {
    setup_fixture
    sign_with "${full_env[@]}" "EXPECTED_CERT_SHA256=$cert_sha256_apksigner"
    assert_eq 0 "$status" "exit status with a matching fingerprint"$'\n'"stderr: $stderr"
    assert_signed_output_is_valid
}

# Criterion: the form `keytool -list -v` prints — colon-separated upper-case —
# is the same fingerprint and must be accepted too; normalising is the
# script's job, not the maintainer's when copying from one tool or the other.
test_sign_apk_accepts_expected_cert_in_keytool_form() {
    setup_fixture
    sign_with "${full_env[@]}" "EXPECTED_CERT_SHA256=$cert_sha256_keytool"
    assert_eq 0 "$status" "exit status with a keytool-form fingerprint"$'\n'"stderr: $stderr"
    assert_signed_output_is_valid
}

# Criterion: a different fingerprint exits non-zero and leaves no output
# file, so a later step cannot pick up an APK signed by the wrong key. The
# message names the fingerprint found and the one expected.
test_sign_apk_refuses_mismatching_expected_cert_and_removes_output() {
    setup_fixture
    local other
    # Same length and alphabet, one digit away: a mismatch that only a real
    # comparison catches, not a format check.
    if [[ "${cert_sha256_apksigner:0:1}" == "0" ]]; then
        other="1${cert_sha256_apksigner:1}"
    else
        other="0${cert_sha256_apksigner:1}"
    fi
    sign_with "${full_env[@]}" "EXPECTED_CERT_SHA256=$other"
    assert_eq 1 "$status" "exit status with a mismatching fingerprint"
    assert_contains "$stderr" "$other" "refusal names the expected fingerprint"
    assert_contains "$stderr" "$cert_sha256_apksigner" "refusal names the fingerprint found"
    assert_file_absent "$out"
    assert_dir_empty "$tmp/scratch"
}

# Criterion: EXPECTED_CERT_SHA256 set but empty is an error, not a skip —
# an empty scripts/android-release-cert.sha256 would otherwise accept any key.
test_sign_apk_refuses_empty_expected_cert() {
    setup_fixture
    sign_with "${full_env[@]}" "EXPECTED_CERT_SHA256="
    assert_eq 1 "$status" "exit status with an empty expected fingerprint"
    assert_contains "$stderr" "EXPECTED_CERT_SHA256" "refusal names the variable"
    assert_file_absent "$out"
    assert_dir_empty "$tmp/scratch"
}

# Criterion: missing positional arguments are refused, naming the usage; a
# script that signs "" into "" must not get as far as decoding a keystore.
test_sign_apk_refuses_missing_arguments() {
    setup_fixture
    run env "${full_env[@]}" "$script" "$tmp/unsigned.apk"
    assert_eq 1 "$status" "exit status with one argument"
    run env "${full_env[@]}" "$script"
    assert_eq 1 "$status" "exit status with no argument"
    assert_dir_empty "$tmp/scratch"
}

# Criterion: no `<out>.idsig` next to the output. apksigner writes that APK
# Signature Scheme v4 sidecar by default; it only serves adb's incremental
# install, and a release would have to attest and publish one more file for
# nothing. v2/v3 stay on, so `apksigner verify` still accepts the output.
test_sign_apk_leaves_no_v4_idsig_sidecar() {
    setup_fixture
    sign_with "${full_env[@]}"
    assert_eq 0 "$status" "exit status"$'\n'"stderr: $stderr"
    assert_signed_output_is_valid
    assert_file_absent "$out.idsig"
}

# Criterion: the output's parent directory must already exist. The caller
# owns `dist/<version>/`; a script that created it would also create
# whatever a mistyped path names. Refused before any file exists, naming
# the directory. apksigner would also fail on that path, after zipalign ran
# and a scratch directory was made, with a message that names it too — so
# TMPDIR points at a directory that does not exist: had the script got as
# far as `mktemp`, it would have died with mktemp's error, not the guard's.
test_sign_apk_refuses_missing_output_directory_before_creating_any_file() {
    setup_fixture
    out="$tmp/absent/signed.apk"
    sign_with "${full_env[@]:0:4}" "TMPDIR=$tmp/nowhere"
    assert_eq 1 "$status" "exit status with a missing output directory"
    assert_contains "$stderr" "output directory" "refusal is the script's own guard"
    assert_contains "$stderr" "$tmp/absent" "refusal names the missing directory"
    assert_file_absent "$tmp/absent"
    assert_file_absent "$tmp/nowhere"
}
