# Iroh Client Android SDK 0.1.0

Transport for existing Android HTTP clients and players. The SDK pairs with the
unchanged protocol-v1 host and exposes its HTTP origin through an embedded loopback
proxy. It does not decode media, transcode, host Jellyfin, or require a terminal.

## Artifact and requirements

- Artifact: `iroh-client-android-0.1.0.aar`; namespace `io.github.xaque8787.iroh`.
- Kotlin 2.1.20+ consumer toolchain; compiled JVM target 17.
- Minimum Android API 24, arm64-v8a, armeabi-v7a and x86_64. No 32-bit x86 build:
  modern emulators use x86_64, and real 32-bit ARM devices use armeabi-v7a.
- SDK versions are independent of host releases. SDK 0.1.0 speaks protocol v1 to
  the existing host 0.3.0. No invitation or authorization format has changed.
- Kotlin suspend APIs use coroutines; all JNI/network work runs on Dispatchers.IO.
  A bare AAR does not carry Maven transitive dependency metadata. Add the coroutine
  runtime below. Kotlin's normal Android plugin supplies the Kotlin standard library.
  No Rust installation, JNA, native pointer handling or separate .so download is needed.

Download the `iroh-client-android-0.1.0` artifact from the **Android SDK** Actions
workflow and extract it. Check the included SHA-256 file. Place the AAR in `app/libs`:

```kotlin
plugins { id("org.jetbrains.kotlin.android") }
dependencies {
    implementation(files("libs/iroh-client-android-0.1.0.aar"))
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.10.2")
}
```

A generated POM accompanies the AAR, and the module has `maven-publish` configuration
for a future Maven repository. A flat file dependency does not read that POM.
The AAR also embeds the pinned rustls Android certificate-verifier support JAR;
initialization passes the application context to that verifier before networking.
No second TLS support AAR needs to be installed.

The library manifest declares INTERNET and contains no Activity or service. Consumer
R8 rules preserve the private JNI class/symbol mapping. The sample consumes the AAR
as a file dependency rather than using the SDK's Gradle project directly.

## Initialize, pair and connect

```kotlin
import io.github.xaque8787.iroh.*

// In your application's coroutine scope; keep the client with your playback owner.
val client = IrohClient.initialize(applicationContext)
// Defaults to noBackupFilesDir/iroh-client. Or supply a dedicated private directory:
// IrohClient.initialize(applicationContext, IrohConfig(
//     storageDirectory = File(applicationContext.noBackupFilesDir, "transport"),
//     deviceName = "Living room TV"
// ))
client.pair(invitationFromUser)  // existing jellyfin-iroh://invite/... link
val connection = client.startProxy()
val baseUrl = connection.proxyUrl // includes a trailing slash and secret URL prefix
```

Initialize once per storage directory. A second live client on the same directory
is rejected using an exclusive file lock. Never put different user accounts into
one directory. `relayUrl = null` uses Iroh defaults; `"none"` disables relays for
local testing. Do not disable relays in a production Internet client by accident.

`pair` validates the invitation, redeems it using the existing host protocol and
atomically stores `host.json`. It retains the existing `client.key` identity and
stops a running proxy before changing pairing. A failed pairing does not erase a
previous saved host. If the host accepted pairing but writing local storage fails,
the invitation may have been consumed: fix storage and obtain a fresh invitation.

Errors are `IrohException` with a typed `IrohErrorCode` and optional `serverCode`.
Malformed local links produce INVALID_INVITATION. Server refusals produce
AUTHORIZATION_DENIED, preserving such codes as `pairing_token_invalid`,
`pairing_rate_limited` and `not_authorized`. **Protocol v1 does not distinguish an
expired token from an already-used or otherwise invalid token**; the SDK cannot
reliably tell those apart without a future host protocol change.

## HTTP clients and playback

Keep the entire returned base URL. It resembles
`http://127.0.0.1:<ephemeral-port>/<random-capability>/`.

```kotlin
// OkHttp (add your application's normal OkHttp dependency):
val request = Request.Builder()
    .url(baseUrl + "System/Info/Public")
    .header("Authorization", appSessionAuthorization)
    .build()
// Execute asynchronously or on Dispatchers.IO, as usual.

// Retrofit: .baseUrl(baseUrl); endpoint paths must be relative, without a leading '/'.
// Media3 (use your application's normal player and media dependencies):
player.setMediaItem(MediaItem.fromUri(baseUrl + "Videos/$videoId/stream?static=true"))
player.prepare()
```

The localhost capability is stripped before forwarding. Query strings, ordinary
headers (including Authorization, Cookie and Range), response headers and response
bodies pass through the existing transport. Large response bodies are streamed;
request bodies retain the transport's existing bounded buffering limit. Dropping
a response cancels its Iroh receive stream. Concurrent HTTP requests use separate
Iroh streams. WebSocket upgrades are supported at the same protected base URL
(use `ws://` instead of `http://`); active upgrades belong to the proxy lifecycle.

Android blocks cleartext HTTP by default in modern target SDKs. Your application
must explicitly permit loopback HTTP in its own network security configuration;
the library does not globally enable cleartext traffic:

```xml
<network-security-config>
  <base-config cleartextTrafficPermitted="false" />
  <domain-config cleartextTrafficPermitted="true">
    <domain>127.0.0.1</domain>
  </domain-config>
</network-security-config>
```

Reference it from `<application android:networkSecurityConfig="@xml/network_security_config">`.
Merge this with your existing application rules, preserving rules for other services.
The embedded Android TLS verifier may also retrieve certificate revocation lists
over HTTP. The SDK packages an optional `@xml/iroh_network_security_config` template
containing upstream's specific CRL domain allowlist plus 127.0.0.1. You may opt into
that template or merge the relevant domains into your own rules; the SDK does not
set it in your application manifest automatically.

### URL compatibility limits

The proxy is not a media-aware URL rewriter. Headers and manifests are forwarded
without rewriting. Applications must evaluate their server/player combination:

| Feature | Behavior and integration requirement |
|---|---|
| Range/seeking | HTTP byte ranges are forwarded; tested on the native proxy. Actual player seek behavior still needs device testing. |
| Relative redirects | Relative paths can stay under the capability prefix; root-relative `/path` loses it and is rejected. |
| Absolute redirects/URLs | May bypass Iroh and target an unreachable LAN origin. Configure server public base URLs or adapt URLs in the application. |
| HLS/DASH | Relative resource URLs may work; root-relative and absolute manifest resources need application/server handling. Not a manifest rewriting SDK. |
| Subtitles | Ordinary subtitle HTTP resources are forwarded; external/root-relative subtitle URLs have the same restrictions. |
| Cookies | Cookie headers pass through. Remote Domain, Path, Secure and SameSite attributes may not match localhost HTTP; the SDK does not rewrite cookie scope. |
| Authentication | Preserve your existing media-server authentication; Iroh device pairing does not replace it. Avoid leaking Authorization to redirect destinations. |
| WebSockets | Native proxy upgrades tested; application-specific handshake/auth still needs integration validation. |

An application that requires origin-root URLs should not strip the capability to
make them work. Adapt its URL construction or use an appropriate application-side
HTTP data source. Do not claim universal HLS/DASH/player support from HTTP tests.

## Status, network changes and recovery

```kotlin
val status = client.getStatus()
// state, authorization, path, rttMs, transportPolicy, policyEnforcement,
// proxyUrl and lastError. RTT/path may be unknown until Iroh observes a path.
client.notifyNetworkChange()
```

The selected `path` is observed independently from the host's configured transport
policy. Existing `p2p_only`/`relay_only` enforcement remains best-effort; the SDK
does not promise a strict routing guarantee. Idle/disconnected authorization is
unknown unless a host rejection was observed; saved pairing alone does not prove
current authorization.

The application owns Android `ConnectivityManager` callbacks and invokes
`notifyNetworkChange` in its scope. This calls the pinned Iroh version's
`Endpoint.network_change()` API; the SDK does not implement NAT/path selection.
For example, register a default network callback, notify on available/lost, and
unregister the callback when your owner is destroyed. ACCESS_NETWORK_STATE is
needed by the application if it registers these callbacks (the sample does).

Iroh may migrate a live connection across paths. When a session has closed, the
next proxy request attempts a new authorized session with the saved host. The SDK
does not replay requests whose bodies may already have reached the server. An
interrupted media stream may fail; your HTTP client/player must retry or reopen
with Range. There is no promise of gapless recovery through Wi-Fi/mobile changes.

## Stop, forget and shut down

```kotlin
client.stopProxy()       // cancels active streams and upgrades; keeps endpoint and pairing
client.startProxy()      // new ephemeral listener and new capability URL
client.forgetPairing()   // stops proxy and removes host.json; RETAINS client.key
client.shutdown()        // closes endpoint and releases the native runtime and directory lock
```

Repeated start calls reuse the running listener; repeated stop/shutdown are safe.
Lifecycle operations are serialized. Native operations complete off the UI thread
and are not interrupted midway by coroutine cancellation; connection attempts are
bounded. Shutdown waits for proxy task cancellation. Do not call shutdown while
expecting a player to continue reading. Create a new client after shutdown.

Forgetting local pairing **does not revoke authorization on the host**. Use the
host's devices/revoke controls to revoke a device. To rotate local identity, shut
down, revoke the old identity on the host, then remove the dedicated SDK directory
and initialize/pair again. Losing the seed creates a new device identity.

## Android lifecycle and security

The application owns the UI, player, callbacks and lifecycle. The SDK starts no
Android service and requests no notification permissions. A foreground Android TV
playback Activity can retain the SDK in its playback owner. For phone background
audio, keep it with your existing playback/MediaSessionService and follow Android's
foreground-service and notification requirements. Process death stops all native
work; initialize again and start a new proxy URL on the next launch. Existing URLs
are not durable and should not be persisted or shared.

Identity is an exportable 32-byte Ed25519 seed consumed directly by Iroh. Android
Keystore non-exportable signing keys cannot simply replace that representation.
This implementation stores the seed in an app-private 0600 file under a 0700
directory, defaulting to noBackupFilesDir. It does not claim hardware-backed key
protection. Rooted/debuggable processes, compromised application code, or unsafe
backup configuration can expose it. An application-specific Keystore AES wrapping
scheme is possible but would still require the plaintext seed in native memory;
it is not implemented here. Exclude custom storage directories from Android
backup/data-transfer rules.

Other apps can connect to TCP loopback on Android: 127.0.0.1 alone is not isolation.
The SDK rejects requests missing the random per-proxy capability path. Treat the
full URL as a bearer secret: do not log it, put it in analytics, export it through
Intents, or share it with a WebView containing untrusted content. Capability URLs
reduce casual cross-app access but do not protect a compromised/rooted device or
an app that leaks the URL. They are not server-side authentication. The CLI retains
its historical unprefixed local URL and should be used only for development.

## Build, test and distribute

See [implementation-plan.md](implementation-plan.md) for the extraction design.
Prerequisites: Rust 1.99.1, Java 17+, Android SDK 35/build-tools 35.0.0, NDK
27.2.12479018, Python 3, and `cargo-ndk` 4.1.2. Gradle 8.11.1 is pinned with its
SHA-256 in the wrapper. Downloaded Rust dependencies are pinned by Cargo.lock.

```sh
rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
cargo install cargo-ndk --version 4.1.2 --locked
export ANDROID_NDK_HOME="$ANDROID_HOME/ndk/27.2.12479018"
bash scripts/android/build-aar.sh
cd android
./gradlew :sample:assembleDebug :iroh-client:assembleDebugAndroidTest
```

Outputs: `dist/iroh-client-android-0.1.0.aar`, POM and checksum; native libraries
remain in `android/iroh-client/src/main/jniLibs/<abi>/libiroh_client.so`. ELF
architecture, JNI export and 16 KiB load alignment are checked before packaging;
AAR contents are checked afterward. Do not distribute an AAR built before all ABI
checks pass. Future SDK version changes must update the Rust adapter, Gradle module,
sample artifact reference, build scripts and artifact workflow together.

Run native validation from `iroh`: `cargo test --workspace --locked` and
`cargo clippy --workspace --all-targets --locked -- -D warnings`. Run the existing
CLI e2e script too. With a booted Android emulator, run
`bash scripts/android/emulator-e2e.sh` from the repository root. It starts a real
local host/origin and passes a single-use invite to instrumentation. The test
validates JNI initialization/storage, pairing, HTTP Range, diagnostics and repeated
start/stop, preserving the host's advertised addresses and adding emulator host alias 10.0.2.2
as a fallback, with no public relay.

The sample deliberately has no playback engine dependency; the Media3 integration
snippet above is illustrative. Media3 playback, physical Android TV devices,
real mobile network changes and public relay operation require additional device
validation. See [validation.md](validation.md) for the checks actually performed.
