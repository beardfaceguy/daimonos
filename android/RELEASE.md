# Android release and sideloading

## CI debug artifact

Every pull request and `master` build runs Android unit tests and lint, assembles
the debug APK, and uploads `daimonos-remote-debug-apk` to the GitHub Actions run
for 14 days. The debug APK is suitable for private testing only.

Install it with ADB:

```bash
adb install -r app-debug.apk
```

Debug APKs from different CI runs may use different ephemeral debug keys. If
Android reports an incompatible signature, uninstall the previous debug app
first (`adb uninstall dev.daimonos.remote`), which also deletes its local
pairing and transcript state. Debug-signed and release-signed APKs cannot
upgrade each other.

Or transfer it to the phone and allow **Install unknown apps** for the specific
app opening the APK. Android grants this permission per installer source; it
does not require publishing through Google Play.

## Create and protect a release key

Generate the key once on an offline or otherwise controlled machine:

```bash
keytool -genkeypair \
  -keystore daimonos-remote-release.jks \
  -alias daimonos-remote \
  -keyalg RSA -keysize 4096 -validity 10000
```

Back up the keystore and passwords separately. Losing the key prevents future
APK upgrades from being installed over the existing app. Never commit the
keystore, passwords, or encoded copies of either.

## Build a signed release APK

```bash
export DAIMONOS_ANDROID_KEYSTORE=/absolute/path/daimonos-remote-release.jks
export DAIMONOS_ANDROID_STORE_PASSWORD='...'
export DAIMONOS_ANDROID_KEY_ALIAS=daimonos-remote
export DAIMONOS_ANDROID_KEY_PASSWORD='...'

cd android
./gradlew clean testDebugUnitTest lintDebug assembleRelease
```

Release packaging fails closed if any signing variable is absent. Debug builds,
unit tests, lint, and IDE synchronization do not require release credentials.

The signed APK is written to:

```text
app/build/outputs/apk/release/app-release.apk
```

Verify it before distribution:

```bash
apksigner verify --verbose --print-certs \
  app/build/outputs/apk/release/app-release.apk
```

Distribute the APK and its SHA-256 digest through an authenticated channel. The
app should connect only to the host's public `wss://` endpoint; never expose the
daemon's loopback WebSocket directly.

## Versioning

Before each release, increase `versionCode` and update `versionName` in
`android/app/build.gradle.kts`. Android requires every installed upgrade to
have a higher `versionCode` and the same signing identity.
