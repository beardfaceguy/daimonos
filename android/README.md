# Daimonos Remote for Android

Native Kotlin/Jetpack Compose controller for a host-owned Daimonos session.

## Requirements

- JDK 17
- Android SDK 36
- Android Studio Quail or a compatible command-line SDK

## Build

```bash
cd android
./gradlew testDebugUnitTest assembleDebug
```

The app accepts only `wss://` daemon endpoints. The reverse proxy terminates
TLS; Daimonos itself remains bound to loopback. Protocol contract tests read
the canonical fixtures from `contracts/android/v2` directly so Rust and Kotlin
wire models cannot drift independently.

See [RELEASE.md](RELEASE.md) for CI artifacts, release signing, verification,
and sideloading instructions.

The controller includes:

- single-use claim pairing with local fingerprint approval;
- an Ed25519 device identity encrypted by an Android Keystore AES-GCM key;
- encrypted ticket storage, WSS authentication, reconnect, replay, and snapshot
  recovery;
- session switching, transcript/tool rendering, prompts, interrupt, stop, and
  approval controls with capability gating;
- cross-language protocol fixtures and Ed25519 known-answer tests.

Runtime/model controls remain hidden until the daemon implements the existing
`set_config` wire command. QR claim scanning and an instrumented
host/device end-to-end test remain follow-up work.
