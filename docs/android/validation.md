# SDK 0.1.0 validation record

Validated in the development environment on November 11, 2026. These results
refer to the local source changes and built artifacts, not to a published release
or a completed GitHub Actions run.

## Passed

- `cargo test --workspace --locked`: **96 passed, 0 failed**. Includes the existing
  86 host/transport tests, private storage tests, JNI runtime/handle ownership, and
  four new real-Iroh engine integration tests.
- Existing Jellyfin plugin test suite: **10 passed, 0 failed**.
- `cargo clippy --workspace --all-targets --locked -- -D warnings` and
  `cargo fmt --all --check`.
- Existing CLI binary build and `scripts/ci/e2e.sh`: pairing, single-use invite,
  full/Range/404 requests, local HTTP proxy, control API, policy, revocation and
  graceful host stop passed after the CLI refactor.
- Engine integration tests exercise concurrent starts/requests, ephemeral listener
  reuse, loopback-only bind rejection, capability rejection, byte ranges, identity
  persistence, duplicate-directory protection, repeated stop/start, forgetting
  pairing without host revocation, WebSocket upgrade/bytes/shutdown, server denial,
  session reconnect, authorization/cookie headers and upstream stream cancellation.
- Redirect and manifest passthrough checks confirm that contents/Location are not
  rewritten. A root-relative URL without the capability is rejected; this is a
  documented compatibility limitation rather than universal media-player support.
- Android shared libraries compiled for arm64-v8a, armeabi-v7a and x86_64 using
  Rust 1.99.1, NDK 27.2.12479018 and Android API 24.
- Each packaged library passed ELF machine, JNI entrypoint, OS-only shared-library
  dependency and 16 KiB PT_LOAD alignment checks. The SDK does not require separate
  dependency .so downloads. Unused dependency cdylibs are excluded from packaging.
- `scripts/android/build-aar.sh` executed successfully and was repeated with cached
  dependencies; the final versioned AAR passed exact ABI inventory, Kotlin class,
  manifest and bundled TLS support checks. Its SHA-256 file verified successfully.
- `:sample:assembleDebug` compiled an application that consumes the standalone AAR
  as a file dependency. Its APK contains all three native libraries and passed
  `zipalign -c -P 16 4`.
- `:iroh-client:assembleDebugAndroidTest` compiled the Android instrumentation APK.
- Gradle wrapper checksum/version, workflow YAML, Android XML and shell syntax
  checks passed. Existing host/plugin version consistency check passed.

## Not run on Android

No physical device is attached (`adb devices` returned an empty list), and this
machine has no `/dev/kvm` for accelerated emulation. The instrumentation tests were
**compiled, not executed** here. The new Android SDK workflow configures an x86_64
API-35 emulator to run JNI lifecycle and real-host pairing/Range/restart tests.
That workflow has not been pushed or executed as part of local validation.

Actual Media3/ExoPlayer playback/seek, HLS/DASH player behavior, phone/Android TV
hardware, Wi-Fi/mobile transitions, public relay behavior, foreground/background
service behavior, Android backup exclusions and hardware-assisted key wrapping
remain application/device integration work. Existing Rust tests establish native
HTTP/WebSocket transport behavior, not Android player compatibility.

The full Jellyfin Docker image smoke test was not rerun for this SDK change.
Host protocol, host implementation and Jellyfin plugin source remain unchanged.
