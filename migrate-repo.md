# Future repository and product organization

Status: discussion draft. This document proposes future work; it does not change
the current architecture, release process, protocol, or supported platforms.

## Goal

Evolve the existing implementation into a general-purpose HTTP-over-Iroh transport
project centered on multi-platform host binaries and Docker, a shared Rust client
engine, Android and iOS SDKs, and clear developer integration documentation. Keep Jellyfin as a reference integration
rather than making its assumptions part of the transport engine.

Build on the working host, shared client engine, protocol v1, and Android SDK.
Moving repositories should not require users to lose identities or pair again.

The clean repository is not a production client-application project. The Jellyfin
Android TV fork lives in its own repository as a working proof of concept. Any
small applications retained here are developer examples and test fixtures, not
separately maintained production products.

## Product boundaries

- **Transport:** framing, pairing, authorization, streaming, WebSockets and path
  policies. No media decoding, transcoding or application-specific authentication.
- **Host:** exposes one configured origin and offers the existing control API.
  Standalone operation and server integrations use the same host implementation.
- **Client engine:** owns identity, authorized sessions, proxy and lifecycle.
  Android/iOS SDKs and the development CLI use this same implementation.
- **SDKs:** native-platform APIs and packaging. Android starts with the existing
  typed Kotlin API; iOS is a planned Swift-facing SDK over the same Rust engine,
  not currently implemented or validated.
- **Examples and tools:** minimal integration examples and the existing CLI for
  development/testing. A production standalone client app is outside this scope.
- **Integrations:** optional adapters for existing servers and clients. Jellyfin
  user authentication remains separate from Iroh device authorization.

## Suggested layout

Keep one source repository initially so protocol, engines, SDKs, tests and releases
can evolve together. Do not split into many repositories before there is a clear
maintenance or ownership reason.

```text
crates/
  transport/                 Shared protocol and networking
  client/                    Platform-independent client engine
  host/                      Host engine and control API
  client-android/            Android native binding adapter
  client-ios/                Planned iOS binding adapter
bins/
  host/                      Host executable for embedding or standalone use
  test-client/               Development/protocol validation CLI
sdks/
  android/                   Android library, publishing and API tests
  ios/                       Planned Swift package/XCFramework and API tests
integrations/
  jellyfin-plugin/           Reference host/control API integration
examples/
  android-basic/             Small SDK consumer
  android-media3/            Focused playback/URL-routing example
  ios-basic/                 Planned minimal native iOS SDK consumer
  host-control-api/          Minimal server administration adapter
deploy/
  docker/                    Images, Compose and persistent-volume examples
docs/
  getting-started/
  architecture/
  protocol/
  sdk/android/
  sdk/ios/
  host-integration/
  security/
  operations/
scripts/
.github/workflows/
```

This is a target structure, not a requirement to rename every crate immediately.
Extract host engine/CLI boundaries only where useful. Keep application forks such
as a full Jellyfin Android TV client in separate repositories; link them from the
examples and compatibility documentation instead of vendoring entire applications.

## Distribution

Distribute the SDK libraries through both package-manager and direct-download
channels. Publish the same validated version/build through those channels rather
than maintaining separate implementations or feature sets.

| Deliverable | Intended distribution |
|---|---|
| Android SDK | Maven Central for Gradle dependency integration; the same AAR plus POM/checksums on GitHub Releases for local inclusion |
| iOS SDK (planned) | Swift Package Manager integration, likely using a binary XCFramework hosted on GitHub Releases; direct XCFramework downloads from the same release |
| Host binaries | Versioned archives for explicitly supported OS/architecture combinations |
| Host Docker image | Dedicated image for running the host beside an existing server |
| Development client CLI | Retained as a testing tool; optional executable release assets |
| Jellyfin plugin | Reference integration package demonstrating server-side host administration |
| Example applications | Source and CI validation; optional clearly labeled demonstration APKs/builds, with no production-app commitment |

Maven Central is the Android/JVM distribution channel. Native iOS developers would
normally use Swift Package Manager/XCFrameworks, not Maven Central. Exact iOS
binding, packaging and simulator/device targets still require investigation.

Direct SDK downloads remain useful for vendoring, controlled and offline builds.
A local AAR consumer must declare runtime dependencies manually; Maven metadata
handles those automatically. Document the equivalent dependency requirements for
both distribution paths. Keep signing and publishing credentials outside source
control. Do not make app-store publication part of the clean repository's scope.

Define a support matrix before promising "all major platforms." Candidate targets
are Linux x86_64/ARM64, Windows x86_64, macOS Apple Silicon/x86_64, and the existing
Android ABIs. Each target needs build, smoke-test and runtime coverage appropriate
to its claimed support level. Cross-compilation alone does not establish support.

Separate a build-only workflow from explicit release publication. Keep artifact
names, checksums, source revisions, notices and release notes consistent. Retain
the coordinated project version initially; reconsider separate SDK versions only
when their release cadence or compatibility requirements justify it.

## Host settings integration and optional UI

The primary administration path is a settings page inside the developer's server
application, backed by a server-side adapter to the host control API. The Jellyfin
plugin is the reference example. Document this pattern independently of Jellyfin.

A generic host administration UI remains an optional future convenience, especially for pairing invitations, device revocation,
transport diagnostics, origin configuration and readable startup errors. Use the
existing control API where it supports those operations; document and version any
necessary extensions rather than reaching directly into host state files.

Clarify two deployment models before implementation:

1. A local UI bundled with or served alongside the standalone host.
2. A server application's own administration UI, using a server-side control API
   adapter, as the Jellyfin plugin does.

The first model removes the need to install a media-server plugin. It does not
remove the origin server or its own user authentication.

Keep the control API and UI local by default. Never embed the administrative
control token in downloadable JavaScript or return it to the browser. A separate
UI service should keep that token server-side and expose authenticated, limited
administration operations. A host-served UI needs an equivalent browser-session
design. Both need CSRF protection, safe invitation handling and a clear remote
administration policy. Do not expose the existing privileged API publicly merely
to make the UI reachable.

Specify which configuration can change live and which changes require a restart.
Use persistent, versioned configuration with validation and actionable errors;
separate configured transport preferences from observed paths.

## Docker and operational behavior

Provide a small standalone host image without requiring Jellyfin in the image.
Document persistent identity/configuration volumes, ownership, graceful shutdown,
health checks, logs, origin connectivity and control API access. Keep the existing
combined Jellyfin deployment as an optional example.

For embedding the host binary in a developer's server, document:

- Selecting and bundling the correct platform binary, its license notices and
  supported version; verify checksums when downloading during a build or install.
- Launching it as a supervised child process with explicit origin, private data
  directory and control API configuration. Also describe a sidecar/service
  deployment where the server framework cannot reliably supervise children.
- Waiting for readiness, capturing useful logs, detecting unexpected exits and
  restarting with bounded backoff instead of a crash loop.
- Graceful shutdown and child reaping, permissions and avoiding orphaned or
  duplicate host processes during application upgrades/restarts.
- Preserving identity, paired devices, configuration and tokens across updates.
- Keeping administrative tokens server-side while the server's authenticated
  settings page offers invitations, diagnostics and device revocation.

The host remains a separate executable; the server developer need not incorporate
Rust into their own application. The client SDK is embedded into the native client
application and does not require that application to launch a CLI executable.

## Documentation and examples

Offer three clear entry points:

1. **Deploy the host:** binaries/Docker, origins, pairing, upgrades, backups and troubleshooting.
2. **Embed the client:** dependency setup, storage, lifecycle, diagnostics, HTTP
   clients, playback integration, network changes and shutdown for Android/iOS.
3. **Integrate the host:** origin setup, control API authentication, invitations,
   devices, revocation, diagnostics, supervised child processes and settings-page adapters.

Examples should compile and run in CI where feasible. Explain capability-prefixed
URLs, redirects, HLS/DASH resources, cookies and authentication explicitly. Publish
a compatibility table that separates tested behavior from expected behavior.
Do not imply that a successful browser test proves every native player works.

Keep protocol/control API specifications, threat model, key-storage policy,
best-effort transport policy semantics and upgrade guarantees near the relevant
developer documentation. Preserve existing invitation links, identity formats and
authorization behavior unless a deliberate, documented migration is required.

## Suggested migration sequence

1. Validate the released SDK in the forked Jellyfin Android TV client: login,
   browsing, direct play, seeking, subtitles, transcoding and reconnection.
2. Use those findings to stabilize SDK lifecycle, proxy routing and error contracts.
3. Decide project name, repository ownership, package namespace and public API
   stability policy before creating permanent Maven/Swift package coordinates.
4. Write the support matrix and integration guides; inventory current workflows,
   generated files, credentials, release assets and persistent data requirements.
5. Create the clean destination repository. Preserving the existing Git commit
   history is optional: the owner explicitly accepts starting with a fresh initial
   commit. Choose the approach that best supports a clean, maintainable structure;
   a transfer/rename is not required. Preserve applicable license notices and
   attribution regardless of the history strategy. Inventory release assets,
   external links, permissions and secrets rather than assuming they transfer with
   source files. Keep the original repository intact during migration and validation;
   archiving or deleting it is a separate decision.
6. Reorganize directories in a separate mechanical change. Validate all paths,
   workflows, Gradle/Rust manifests, Docker contexts, fixtures and examples afterward.
7. Configure Android Maven Central and dual GitHub Release publication; prove a
   clean external application can consume either distribution without source checkouts.
8. Implement and validate the iOS binding and Swift/XCFramework distribution over
   the shared engine. Add a minimal integration example, not a full client app.
9. Add further host OS support and, optionally, a generic host administration UI,
   each with acceptance tests and documentation.

Do not combine repository relocation, protocol changes, a host rewrite, package
renaming and a new UI into one migration. Keep an accessible last-known-good release
and document the upgrade path, including identity and pairing preservation.

## Decisions still open

- Project/repository name and organization ownership.
- Maven group, Kotlin namespace and whether existing names remain compatible.
- Precise OS/architecture support tiers and access to real test devices.
- iOS binding mechanism, Swift package layout and device/simulator support.
- Host UI deployment/session model and remote-administration scope.
- Supported server child-process and host-sidecar deployment patterns.
- Signing, release cadence and long-term API compatibility commitments.

The next practical milestone remains real Jellyfin Android TV integration. That
will tell us which improvements the reusable SDK needs before a repository move.
