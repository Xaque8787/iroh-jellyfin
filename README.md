# jellyfin-iroh

Private remote access to a home media server over [Iroh](https://www.iroh.computer/) peer-to-peer connections, without port forwarding or a hosted backend.

The core is a standalone Rust host, `iroh-host`. It knows nothing about Jellyfin. It forwards HTTP requests from paired devices to one configurable local origin. Jellyfin is the first integration, through an admin plugin. Another media server could bundle the same executable.

## Status

| Milestone | Scope | State |
|---|---|---|
| 1. Foundation | Transport protocol v1 messages and framing, policies, pairing primitives, HTTP forwarding primitives, host identity and state storage, `identity` and `doctor` commands, build validation CI | Code written. Not compiled or tested in the authoring environment; verified by CI and local runs. |
| 2. Rust host | Iroh endpoint, sessions, HTTP-over-Iroh streaming with Range and cancellation, test client | Not started |
| 3. Control API, plugin, Docker | Versioned control socket, Jellyfin plugin, Docker image, Compose | Not started |
| Later | Cross-platform workflow, diagnostics, docs, Android TV fork spec | Not started |

## Repository layout

```
iroh/                     Rust workspace (pinned toolchain, committed Cargo.lock)
  crates/iroh-transport   Reusable protocol, framing, policies, pairing, HTTP forwarding
  crates/iroh-host        Host executable: identity, device registry, commands
protocol/                 Versioned protocol specifications
docs/                     Policies, versions, design notes
.github/workflows/        Build validation
```

## Building the host without Docker

Requires Rust (the pinned toolchain installs automatically through `rustup`).

```bash
cd iroh
cargo test --workspace --locked
cargo build --release --locked -p iroh-host
./target/release/iroh-host identity --data-dir ./iroh-data
./target/release/iroh-host doctor --data-dir ./iroh-data --origin http://127.0.0.1:8096
```

Configuration (flags or environment):

| Variable | Default | Meaning |
|---|---|---|
| `IROH_DATA_DIR` | `./iroh-data` | Identity, device registry, lock file |
| `IROH_ORIGIN` | `http://127.0.0.1:8096` | Local HTTP service to expose |
| `IROH_TRANSPORT_POLICY` | stored value, else `auto` | `auto`, `relay_only`, `p2p_only` |
| `IROH_RELAY_URL` | Iroh default relays | Custom relay |

## Security notes

- The data directory holds the host's private key (`identity.key`). It is created with owner-only permissions on Unix. Back it up: if it is lost, every device must pair again.
- Invitation tokens are stored only as SHA-256 digests. They are single use and expire. Failed attempts are rate limited, and the limit is saved across restarts.
- `relay_only` and `p2p_only` are **best-effort**, not guaranteed. See `docs/transport-policies.md`.

## License

MIT OR Apache-2.0
