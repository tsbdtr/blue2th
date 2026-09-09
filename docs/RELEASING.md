# Releasing blue2th

Reference for how a release is made and why it is made that way. **Findings and
procedure, not tasks**: anything with a closing condition lives in
[the issue tracker](https://github.com/tsbdtr/blue2th/issues).

## Branching model

`main` is what has been delivered, one `vX.Y.Z` tag per delivery. `develop` is the
development line. Feature branches (`feat/*`, `fix/*`, `docs/*`, …, see
`CLAUDE.md`) branch off `develop` and merge back into it.

`main` receives `develop` through a **delivery pull request**, merged with a
merge commit. That commit is not on `develop`, but its tree is `develop`'s, so
the next delivery cannot conflict. `hotfix/*` branches off `main`, goes through
a PR into `main` (plus a tag), and is then propagated with **`git merge main` on
`develop`** — not a second `hotfix → develop` PR: absorbing `main`'s merge
commits is what keeps the two lines from diverging. Merging only the fix leaves
them diverged.

No `release/*` branches: the tag on `main` already triggers the build.

**Never squash or rebase a branch-to-branch merge.** A squash-merge of
`develop → main` creates a commit with no ancestry link, a rebase rewrites the
commits, and either way the two lines diverge permanently and every subsequent
delivery starts in conflict. The repository only offers the merge-commit
method, so the interface enforces this.

### Repository rules

Three rulesets, with no bypass actor on the branches — on a solo repository a
bypass is the same as no rule:

- **`develop`** and **`main`**: a pull request is required, merged with a merge
  commit; the five CI checks must pass (`fmt, clippy, test`, the SPDX header,
  the release scripts' shell tests, `cargo deny`, the PR title); force pushes
  and deletion are blocked. A direct push is refused with `GH013`.
- **Tags `v*`**: nobody deletes or moves a release tag, and only the repository
  owner creates one. A release that needs redoing is a new patch version, never
  a re-pointed tag: once public, someone may already have downloaded it.

They are managed on GitHub under *Settings → Rules*, or through
`gh api repos/tsbdtr/blue2th/rulesets`.

## Making a release

A `vX.Y.Z` tag on `main` produces a GitHub Release carrying a signed APK and a
server binary, each with its SHA-256 and, once the repository is public, a
build-provenance attestation. The
workflow is `.github/workflows/release.yml`; the logic that can be tested lives
in the shell scripts under `scripts/`, with their tests under `scripts/tests/`
(run by `scripts/tests/run.sh`, which CI runs on every pull request).

By hand, in this order:

1. Bump `[workspace.package] version` in the root `Cargo.toml`, through a pull
   request like any other change. The tag has to designate exactly that value:
   the workflow refuses a tag that is not `v` + the workspace version, because a
   `v0.2.0` tag on a tree still at `0.1.0` would otherwise ship a `versionName`
   of `0.1.0` with the `versionCode` of `0.2.0`, green all the way.
2. Rehearse from `develop`, before any tag:
   ```bash
   gh workflow run release.yml --ref develop && gh run watch
   ```
   The same build jobs run, from the workspace version, with the real secrets;
   the two files are uploaded as workflow artifacts; the publish job is skipped.
   This is the only way to exercise the workflow file itself, and it costs
   nothing if it fails. (`workflow_dispatch` only reaches a workflow that
   exists on the default branch, so a change to `release.yml` is rehearsed
   after its pull request lands, not from its branch.)
3. Deliver `develop` to `main` through a pull request, and merge it — with a
   merge commit, the only method the repository allows. The merge is the human
   step; the CI checks run on this pull request like on any other:
   ```bash
   gh pr create --base main --head develop --title "chore(release): deliver vX.Y.Z" \
     --body "Delivery of vX.Y.Z; rehearsed on develop by run <id>."
   ```
4. Tag the merge commit, from any checkout, and push the tag — the tag is the
   act of publishing, and it stays a hand-made gesture on purpose:
   ```bash
   git fetch origin
   git tag -a vX.Y.Z -m "blue2th X.Y.Z" origin/main
   git push origin vX.Y.Z
   ```
   The tag push is not covered by the branch rules; it triggers `release.yml`.
5. Check the Release: download the APK and run the verification commands its
   notes carry (below). Install it on a phone that already has the previous
   release: it must update in place.

What the tag triggers:

- **Version**: one job derives everything the others name. On a tag,
  `scripts/check-tag-version.sh` refuses a tag that is not `v` + the
  `[workspace.package] version`; `scripts/version-code.sh` derives the Android
  `versionCode` from it. Pre-release tags (`v0.1.0-rc1`) are refused: the
  `versionCode` formula cannot encode them.
- **Server binary**: `cargo build --release -p blue2th-server` on `ubuntu-latest`,
  with the same native packages as `ci.yml`. Packed with both licence texts as
  `dist/<version>/blue2th-server-<version>-x86_64-linux-gnu.tar.gz`.
- **APK**: `dx build --platform android --package blue2th-frontend --release --target aarch64-linux-android`
  with `dioxus-cli` pinned at `0.7.10` (the pin exists because the frozen
  Android templates do not follow `dx` upgrades — see `CLAUDE.md`), then
  `scripts/set-version-code.sh` on the generated `build.gradle.kts` and
  `./gradlew assembleRelease` for the unsigned APK, then `scripts/sign-apk.sh`
  into `dist/<version>/blue2th-<version>.apk`. A last guard reads the
  `versionCode` back out of the signed APK with `scripts/apk-version-code.sh`
  and compares it with the derived one. The `--target` is explicit because
  `dx` otherwise builds for the host architecture — x86_64 on the runner, the
  emulator ABI — and the phone refuses the APK.
- **Publish**: only on a tag ref. Both artifacts are downloaded into
  `dist/<version>/`, `sha256sum` writes `SHA256SUMS` there with bare file
  names, build provenance is attested on the signed files, then
  `gh release create` uploads both artifacts and the checksums, with release
  notes carrying the verification commands and the certificate fingerprint.
  The generated part of the notes is grouped by labels that `pr-title.yml`
  derives from each pull request's Conventional Commits type
  (`.github/release.yml`), so a `feat:` lands under *New features* without
  anyone labelling by hand.
  A failure here publishes nothing: the two artifacts stay on the run, and the
  tag can be deleted and posed again once the cause is fixed.

Every artifact is written under **`dist/<version>/`** and nothing at the
repository root. `.gitignore` already covers `dist/`, so a local rehearsal of
the same commands leaves a clean tree; a stray output at the root would be
swept into the next `git add -A`. `scripts/tests/run.sh` fails any test that
leaves a file there for the same reason.

## Verifying a download

An APK signature attests **continuity, not identity**: Android only checks that
an update carries the same certificate as the install it replaces, so nothing
stops a third party from signing an APK that claims to be blue2th. In sideloaded
distribution the trust anchor is the channel, and the Release carries what makes
a file checkable:

```bash
gh attestation verify blue2th-<version>.apk --repo tsbdtr/blue2th
sha256sum -c SHA256SUMS
apksigner verify --print-certs blue2th-<version>.apk   # compare with scripts/android-release-cert.sha256
```

The attestation answers *did this file come out of this repository's release
workflow?* — Sigstore binds it to the workflow's identity (repository, tag,
commit, workflow file) in a public transparency log, with no key to store. The
fingerprint answers *was it signed with the project's key?* — it is derived
from the certificate, not the private key, so publishing it gives nothing away.

**Only releases made while the repository is public carry an attestation.**
GitHub's attestation store refuses a user-owned private repository, so the
workflow skips the step there and the release notes say so instead of quoting a
command that cannot succeed (#94). A release made while private cannot be
attested afterwards; its checksums and the certificate fingerprint are its
checks.

## Signing

`dx` 0.7.10 emits an **unsigned** release APK, and the workflow signs it itself.
Two routes were considered and rejected:

- `[bundle.android]` / `[android.signing]` in `Dioxus.toml`: the keys take the
  keystore password as a value, so a working configuration is a password in a
  tracked file. `[android.signing]` is also not read by `dx` 0.7.10 (checked in
  `packages/cli/src/build/android.rs`, which feeds the Gradle template from
  `[bundle.android]` only), and `Dioxus.toml` gets no environment expansion.
- Letting Gradle sign through `signingConfigs`: the generated project is
  rewritten by `dx` on every build, so the configuration would have to be
  injected after the fact, into a file `dx` owns.

`scripts/sign-apk.sh` takes the keystore and its secrets from the environment
only (`ANDROID_KEYSTORE_B64`, decoded to a file it removes on exit, plus the
three `ANDROID_KEY*` variables), never from argv, and refuses to start on any
missing or empty value. Its output path is explicit and its directory must
already exist — the workflow creates `dist/<version>/` first; a script that
created it would also create whatever a mistyped path names. It signs with
v2 and v3 only: the v4 scheme lives in a `<out>.idsig` sidecar that serves
`adb`'s incremental install and nothing a release needs.

The fingerprint of the release certificate lives in
**`scripts/android-release-cert.sha256`**, in either form the tools print —
the colon-separated upper-case of `keytool -list -v` or the bare lower-case
hex of `apksigner verify --print-certs`; the script normalises both, and the
release notes show the latter. The workflow fails when that file is empty, and
`sign-apk.sh` removes the output when the certificate that signed it is not
the one recorded.

The keystore itself (#3) is the one irreversible item: losing it means no
existing install can ever be updated. It lives outside the repository, backed up
off-machine, and reaches the workflow only as repository secrets. Should the app
ever go through the Play Store, enrol **this** key in Play App Signing (exported
with PEPK), never a key Google generates: the certificate must stay the same for
the installs made from GitHub Releases to update from the Store.

## The Android `versionCode` — `dx` does not derive it

`versionCode` is the integer Android compares to decide whether an APK is an
update. It must increase on every publication, or the phone refuses to install
over the app already there.

`dx` does not derive it from anything. In the Handlebars template embedded in the
`dx` binary, the line is a **literal**, sitting between two genuine placeholders:

```kotlin
applicationId = "{{ application_id }}"
minSdk = {{ min_sdk }}
targetSdk = {{ target_sdk }}
versionCode = 1
versionName = "{{ version }}"
```

No configuration key can reach it, because there is no substitution point to
reach. It is `1` in debug and in release alike. Verified against `dx` 0.7.10, in
the generated `target/dx/blue2th-frontend/debug/android/app/app/build.gradle.kts`.
Left alone, the first release ships `versionCode = 1` and the second one cannot
be installed over it.

Two routes look like solutions and are not:

- **Declaring `android:versionCode` in the frozen manifest.** The AGP DSL value
  takes precedence over the manifest attribute whenever it is set — and it is set,
  to `1`.
- **Patching the generated `build.gradle.kts`, then re-running `dx`.** `dx`
  rewrites that file from the template on every build, discarding the patch.

What makes it tractable is that the generated Android project is a **standalone
Gradle project**, `gradlew` included, under
`target/dx/<package>/<profile>/android/app/`. The workflow lets `dx` build once,
rewrites the line, then drives Gradle directly — the Rust `.so` files are already
staged in `jniLibs`, so only the Android packaging runs again:

```bash
dx build --platform android --package blue2th-frontend --release --target aarch64-linux-android
scripts/set-version-code.sh \
  target/dx/blue2th-frontend/release/android/app/app/build.gradle.kts "${VERSION_CODE}"
(cd target/dx/blue2th-frontend/release/android/app && ./gradlew assembleRelease)
mkdir -p "dist/${VERSION}"
EXPECTED_CERT_SHA256="$(cat scripts/android-release-cert.sha256)" \
  scripts/sign-apk.sh \
  target/dx/blue2th-frontend/release/android/app/app/build/outputs/apk/release/app-release-unsigned.apk \
  "dist/${VERSION}/blue2th-${VERSION}.apk"
scripts/apk-version-code.sh "dist/${VERSION}/blue2th-${VERSION}.apk"   # prints ${VERSION_CODE}
```

With the four `ANDROID_KEY*` variables in the environment, as in the
workflow's `Sign` step. Note that `dx build` alone, with no signing section in
`Dioxus.toml`, runs `assembleDebug` even in `--release`: its own APK is
debug-signed, and it is the second Gradle pass that produces
`app-release-unsigned.apk`.

The script, not a bare `sed`, because a `sed` that finds nothing is a silent
no-op: the APK builds, signs and verifies with `versionCode = 1`, and fails on
the phone at the second release. The script exits non-zero unless the line is
there exactly once, and the workflow reads the value back from the signed APK.

The value is **`major * 1000000 + minor * 1000 + patch`** — not a formula of our
own. It is the derivation [DioxusLabs/dioxus#5735](https://github.com/DioxusLabs/dioxus/pull/5735)
uses, so the value computed here is the value `dx` computes by itself once that
change ships, and the sequence stays monotonic across the switch (#86). It also
stays inside Play's `1..=2100000000` range, and leaves room for 999 minors and
999 patches where a tighter formula would cap them at 99.

## glibc works in our favour

Built on Ubuntu 24.04 (glibc 2.39), the server binary runs on the target Fedora
43 (glibc 2.42). The reverse would have broken — a binary built against a newer
glibc does not run on an older one. Worth stating, because it is the kind of
thing that looks like luck until it stops being true.
