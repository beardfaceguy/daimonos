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

This first slice contains the Compose shell, complete protocol-v2 models,
remote-auth models, Ed25519 known-answer verification, and the bounded OkHttp
WebSocket seam. Pairing, secure device-key persistence, and controller screens
are implemented in the next slice.
