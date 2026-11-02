# Pinned Versions

| Component | Version | Where pinned | Why |
|---|---|---|---|
| Iroh | `=1.4.1` | `iroh/Cargo.toml` | Current stable 1.x. The APIs used (`presets::Minimal`, `RelayMode`, `RelayMap`, `SecretKey`, `Connection::paths()`) were checked against the 1.4.1 source and docs.rs. The exact pin stops a minor update from silently changing behavior. |
| Rust toolchain | `1.99.1` | `iroh/rust-toolchain.toml`, CI | Current stable at time of writing. Builds are reproducible with one compiler. |
| Rust MSRV | `1.91` | `rust-version` in `iroh/Cargo.toml`, CI `msrv` job | Iroh 1.4.1 needs edition 2024 and Rust 1.91. The host uses `File::try_lock` (stable since 1.89). |
| Crate graph | exact | `iroh/Cargo.lock` (committed) | CI and Docker build with `--locked`. |
| Jellyfin server image | `jellyfin/jellyfin:12.4.20261026-211555` | Dockerfile (Milestone 3) | Current stable 12.4 release. Digests were recorded during research and will be pinned with the tag. |
| Jellyfin plugin SDK | `Jellyfin.Controller` / `Jellyfin.Model` `12.4.0` | plugin `.csproj` (Milestone 3) | Must match the server's ABI. |
| .NET SDK | 10.0 (exact patch pinned in Milestone 3) | plugin build image | Jellyfin 12.4 runs on .NET 10. |

## Policy

- No `latest` tags.
- Upgrading Iroh means re-verifying the API, then re-running the transport tests and the real-network checks in `docs/transport-policies.md`.
- `cargo update` is a deliberate change reviewed in its own commit.
