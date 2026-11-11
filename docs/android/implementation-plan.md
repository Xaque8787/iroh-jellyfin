# Android SDK audit and implementation plan

The original `iroh-test-client` owns CLI parsing/output, endpoint construction, private
`client.key` / `host.json` persistence, pairing, connection reuse, and a Hyper HTTP/1
proxy with streaming responses and WebSocket upgrades. `iroh-transport` already owns
protocol v1, authorization, policy, path reporting, request streams and cancellation.
The host, invitation format, protocol and Jellyfin plugin need no changes.

1. Extract endpoint/storage/session/proxy ownership to `iroh-client`; keep CLI flags,
   stdout/stderr formatting, fetch-to-file and terminal signals in the CLI. Validate
   existing workspace tests and the existing CLI end-to-end script before Android.
2. Add `iroh-client-android` cdylib with a small JNI adapter. Kotlin exposes typed
   suspend APIs on Dispatchers.IO; native tokens never reach the public API. JNI
   avoids UniFFI's Kotlin/JNA runtime distribution dependency for this standalone
   AAR. UniFFI remains a viable future interface over the same core.
3. Serialize lifecycle mutations, reuse one endpoint, retain proxy listener/tasks
   beyond calls, use explicit shutdown and abort/join active proxy tasks. Do not
   replay interrupted requests automatically. Subsequent requests reconnect.
4. Android uses loopback port zero and an unguessable per-proxy URL path capability,
   stripped before forwarding. Reject requests outside that namespace, including
   upgrades. CLI keeps its existing unprefixed URL behavior. No wildcard listener.
5. Package arm64-v8a, armeabi-v7a and x86_64, API 24+, namespace
   `io.github.xaque8787.iroh`, SDK version 0.4.0, aligned with the coordinated host/plugin release.
6. Build AAR, independent sample, native lifecycle tests and CI; distinguish native
   Linux tests, Android compilation and actual emulator/device behavior.

Compatibility constraints: keep existing client identity filenames and invitation
links; retain authorization/revocation and best-effort host policies. Identity is a
32-byte exportable Ed25519 seed; Android Keystore signing keys cannot replace it.
Use private, non-backed-up app storage and owner-only files. Forgetting pairing
retains identity and is not server-side revocation. Pairing rejection codes are
limited by existing protocol v1; expired/used/invalid may be indistinguishable.

## Reference audit

The MediaStorm Android TV reference currently places `IrohBridgeNative.kt` under
`core/src/main/java/expo/modules/irohbridge`, rather than the linked `app` path.
It uses a blocking JSON JNI interface, including startProxy/network changes/path
diagnostics, with its own private invite and rendezvous protocol. This SDK uses
that only as an interface reference: no MediaStorm wire/pairing code was copied.
The public interface here is typed Kotlin suspend methods over this repository's
existing protocol-v1 transport.

Android compilation also revealed the existing reqwest dependency's use of
rustls-platform-verifier. The adapter initializes its Android context and bundles
the checksum-pinned upstream JVM support JAR inside the AAR. This requirement
would exist with either JNI or UniFFI and must not be bypassed by disabling TLS
verification. An optional network-security template is bundled without changing
the consuming application's manifest policy.
