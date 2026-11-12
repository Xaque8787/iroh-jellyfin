# Pinned Versions

| Component | Version | Where pinned | Why |
|---|---|---|---|
| Host / CLI / Android SDK release | `0.4.0` | `iroh/Cargo.toml`, `iroh/Cargo.lock` | The Docker host is built from this workspace, not downloaded. Android SDK 0.4.0 uses the same protocol v1. |
| Iroh Jellyfin plugin release | `0.4.0.0` | plugin `.csproj`, `jellyfin-plugin/meta.json` | Built from local source. Docker runs `check-version.sh` before publishing it, and startup replaces persistent older plugin copies. |
| Iroh | `=1.4.1` | `iroh/Cargo.toml` | Current stable 1.x. The APIs used (`presets::Minimal`, `RelayMode`, `RelayMap`, `SecretKey`, `Connection::paths()`, `TransportAddr::is_relay`) were checked against the 1.4.1 source and docs.rs. The exact pin stops a minor update from silently changing behavior. |
| Rust toolchain | `1.99.1` | `iroh/rust-toolchain.toml`, CI, Dockerfile | Current stable at time of writing. Builds are reproducible with one compiler. |
| Rust MSRV | `1.91` | `rust-version` in `iroh/Cargo.toml`, CI `msrv` job | Iroh 1.4.1 needs edition 2024 and Rust 1.91. The host uses `File::try_lock` (stable since 1.89). |
| Crate graph | exact | `iroh/Cargo.lock` (committed) | CI and Docker build with `--locked`. |
| Rust build image | `rust:1.99.1-slim-bookworm@sha256:24707e70…f1d400` | Dockerfile | Same compiler as CI. Bookworm's older glibc keeps the binary compatible with the newer Debian in the Jellyfin image. |
| Jellyfin server image | `jellyfin/jellyfin:12.4.20261026-211555@sha256:fea863d0…1b771` | Dockerfile | Current stable 12.4 release, pinned by tag and multi-arch index digest (amd64 and arm64). |
| Jellyfin plugin SDK | `Jellyfin.Controller` / `Jellyfin.Model` `12.4.0` | `Jellyfin.Plugin.Iroh.csproj` | Must match the server's ABI (`targetAbi` `12.4.0.0` in `meta.json`). The 12.4.0 packages target `net10.0`. |
| .NET SDK | `10.0.403` | `jellyfin-plugin/global.json`, Dockerfile (`mcr.microsoft.com/dotnet/sdk:10.0.403@sha256:de4fe90a…dc02b`) | Latest .NET 10 SDK; Jellyfin 12.4 runs on .NET 10. `global.json` allows newer patch releases only. |
| Plugin test packages | xunit `2.9.3`, xunit.runner.visualstudio `4.0.1`, Microsoft.NET.Test.Sdk `18.11.0` | test `.csproj` | Current stable releases. |

Full digests are in the Dockerfile.

## Policy

- No `latest` tags.
- Upgrading Iroh means re-verifying the API, then re-running the transport tests and the real-network checks in `docs/transport-policies.md`.
- Upgrading Jellyfin means changing the image, the two SDK packages and `targetAbi` together.
- `cargo update` is a deliberate change reviewed in its own commit.
- NuGet packages are pinned to exact versions, but there is no NuGet lock file yet, so transitive packages are resolved at build time. A `packages.lock.json` can be added once it can be generated with the SDK.

## Docker verification

The current source produces host `0.4.0` and plugin `0.4.0.0`. The pinned Rust
Docker stage builds successfully; the plugin publishes with the pinned .NET
10.0.403 SDK and all 10 plugin tests pass. A runtime audit using those artifacts
and the Dockerfile's pinned Jellyfin base passed the smoke test: healthy server,
plugin loaded, installed DLL matches the bundle, stale plugin removed, control
API available, host restart preserves identity, and clean shutdown.

The complete multi-stage build could not be verified in the managed environment:
the .NET registry redirects downloads to `westcentralus.data.mcr.microsoft.com`,
which its network policy blocks. The runtime audit used the already installed
matching .NET SDK to publish the plugin, rather than substituting an unpinned
registry image. No production installation was modified.
