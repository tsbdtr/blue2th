#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# Contract of scripts/apk-version-code.sh: `--parse` reads `aapt dump badging`
# output on stdin and prints the versionCode alone; `<apk>` runs aapt itself
# and parses the same way. Exit 1 when the attribute is absent or empty.
# Sourced by scripts/tests/run.sh; each test_* runs in its own subshell.

script="$SCRIPTS_DIR/apk-version-code.sh"

# Criterion: the nominal badging line yields `1000`.
test_apk_version_code_parses_nominal_badging_line() {
    assert_succeeds "$script" --parse <<'EOF'
package: name='io.github.tsbdtr.blue2th' versionCode='1000' versionName='0.1.0' platformBuildVersionName='15'
EOF
    assert_eq 1000 "$stdout" "versionCode parsed from the package line"
}

# Criterion: a full `aapt dump badging` output — many lines, the package line
# first, other quoted numbers everywhere — still yields the one versionCode.
test_apk_version_code_parses_full_badging_output() {
    assert_succeeds "$script" --parse <<'EOF'
package: name='io.github.tsbdtr.blue2th' versionCode='1002003' versionName='1.2.3' platformBuildVersionName='15' platformBuildVersionCode='35' compileSdkVersion='35' compileSdkVersionCodename='15'
sdkVersion:'24'
targetSdkVersion:'34'
uses-permission: name='android.permission.INTERNET'
uses-permission: name='android.permission.CHANGE_WIFI_MULTICAST_STATE'
application-label:'blue2th'
application: label='blue2th' icon='res/mipmap-anydpi-v26/ic_launcher.xml'
launchable-activity: name='dev.dioxus.main.MainActivity'  label='' icon=''
feature-group: label=''
  uses-feature: name='android.hardware.faketouch'
  uses-implied-feature: name='android.hardware.faketouch' reason='default feature for all apps'
supports-screens: 'small' 'normal' 'large' 'xlarge'
supports-any-density: 'true'
locales: '--_--'
densities: '160' '240' '320' '480' '640' '65534'
native-code: 'arm64-v8a'
EOF
    assert_eq 1002003 "$stdout" "versionCode parsed from full badging output"
}

# Criterion: a package line without `versionCode` is refused, not read as 0
# or as the versionName.
test_apk_version_code_refuses_line_without_version_code() {
    assert_fails "$script" --parse <<'EOF'
package: name='io.github.tsbdtr.blue2th' versionName='0.1.0' platformBuildVersionName='15'
EOF
    assert_eq "" "$stdout" "stdout on refusal"
    assert_contains "$stderr" "versionCode" "refusal names the missing attribute"
}

# Criterion: `versionCode=''` is refused — an empty attribute is not a code,
# and printing an empty line would satisfy a careless `[ "$x" = "$y" ]`.
test_apk_version_code_refuses_empty_version_code() {
    assert_fails "$script" --parse <<'EOF'
package: name='io.github.tsbdtr.blue2th' versionCode='' versionName='0.1.0' platformBuildVersionName='15'
EOF
    assert_eq "" "$stdout" "stdout on refusal"
}

# Criterion: empty stdin is refused — aapt printing nothing (a corrupt APK,
# a crash) must not read as a versionCode.
test_apk_version_code_refuses_empty_stdin() {
    assert_fails "$script" --parse </dev/null
    assert_eq "" "$stdout" "stdout on refusal"
}

# Criterion: a versionCode that is not a positive integer is refused, so a
# mangled badging line cannot pass the `== derived code` check by accident.
test_apk_version_code_refuses_non_numeric_version_code() {
    assert_fails "$script" --parse <<'EOF'
package: name='io.github.tsbdtr.blue2th' versionCode='abc' versionName='0.1.0'
EOF
    assert_eq "" "$stdout" "stdout on refusal"
}

# Criterion: given an APK path, the script runs aapt on it and prints the
# manifest's versionCode — the fixture declares 1000.
test_apk_version_code_reads_fixture_apk() {
    make_unsigned_apk "$tmp/fixture.apk"
    assert_succeeds "$script" "$tmp/fixture.apk"
    assert_eq 1000 "$stdout" "versionCode read from the fixture APK"
}

# Criterion: an APK that does not exist is refused, naming its path.
test_apk_version_code_refuses_missing_apk() {
    assert_fails "$script" "$tmp/missing.apk"
    assert_contains "$stderr" "$tmp/missing.apk" "refusal names the path"
}

# Criterion: no argument at all is refused — the script must not silently
# fall into parse mode and wait on stdin.
test_apk_version_code_refuses_missing_argument() {
    assert_fails "$script" </dev/null
}
