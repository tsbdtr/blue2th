# Test fixtures for the release scripts

## `AndroidManifest.xml`

A **binary** (compiled) Android manifest, 1004 bytes. `apksigner` refuses to
sign a zip that carries no compiled manifest — it reads `minSdkVersion` from it
to pick the signature schemes — so the signing tests need one, and compiling it
at test time would require a platform `android.jar` on every machine that runs
the tests. Build-tools alone do not ship one. The compiled form is committed
instead; `sign-apk.test.sh` zips it into the unsigned APK it signs.

Its declared values are what the tests assert on: package
`io.github.tsbdtr.blue2th.fixture`, `versionCode="1000"`, `versionName="0.1.0"`,
`minSdkVersion="24"`. The `.fixture` suffix keeps it from ever being mistaken for
the real application identifier, which is frozen (see `CLAUDE.md`).

To regenerate it from the source below, with any platform `android.jar`:

```bash
aapt package -f -M AndroidManifest.src.xml \
  -I "$ANDROID_HOME/platforms/android-34/android.jar" -F fixture.apk
unzip -o fixture.apk AndroidManifest.xml
```

where `AndroidManifest.src.xml` is:

```xml
<?xml version="1.0" encoding="utf-8"?>
<manifest xmlns:android="http://schemas.android.com/apk/res/android"
    package="io.github.tsbdtr.blue2th.fixture"
    android:versionCode="1000"
    android:versionName="0.1.0">
    <uses-sdk android:minSdkVersion="24" />
</manifest>
```
