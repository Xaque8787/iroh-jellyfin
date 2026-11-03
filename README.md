# jellyfin-iroh

Private remote access to a home media server over [Iroh](https://www.iroh.computer/) peer-to-peer connections, without port forwarding or a hosted backend.

The core is a standalone Rust host, `iroh-host`. It knows nothing about Jellyfin. It forwards HTTP requests from paired devices to one configurable local origin. Jellyfin is the first integration, through an admin plugin. Another media server could bundle the same executable.

## Status

| Milestone | Scope | State |
|---|---|---|
| 1. Foundation | Transport protocol v1 messages and framing, policies, pairing primitives, HTTP forwarding primitives, host identity and state storage, `identity` and `doctor` commands, build validation CI | Compiled and tested by CI |
| 2. Rust host | Iroh endpoint, pairing over invite links, sessions, HTTP-over-Iroh streaming with Range and cancellation, revocation, policy gate, test client, end-to-end CI test | Code written, waiting for its first CI run |
| 3. Control API, plugin, Docker | Versioned control socket, Jellyfin plugin, Docker image, Compose | Not started |
| Later | Cross-platform workflow, diagnostics, docs, Android TV fork spec | Not started |

## Repository layout

```
iroh/                     Rust workspace (pinned toolchain, committed Cargo.lock)
  crates/iroh-transport   Reusable protocol, framing, policies, pairing, HTTP forwarding
  crates/iroh-host        Host executable (identity, device registry, commands) and iroh-test-client
protocol/                 Versioned protocol specifications
docs/                     Policies, versions, design notes
scripts/ci/               End-to-end test and a Range-capable test origin
.github/workflows/        Build validation
```

## Building the host without Docker

Requires Rust (the pinned toolchain installs automatically through `rustup`).

```bash
cd iroh
cargo test --workspace --locked
cargo build --release --locked -p iroh-host --bins
./target/release/iroh-host doctor --data-dir ./iroh-data --origin http://127.0.0.1:8096
./target/release/iroh-host serve --data-dir ./iroh-data --origin http://127.0.0.1:8096
```

With the host running, in a second terminal:

```bash
./target/release/iroh-host invite --data-dir ./iroh-data --label "Laptop"   # prints a jellyfin-iroh://invite/... link
./target/release/iroh-host devices --data-dir ./iroh-data
./target/release/iroh-host revoke --data-dir ./iroh-data <endpoint id>
```

On the device (any machine, using its own data directory):

```bash
./target/release/iroh-test-client pair 'jellyfin-iroh://invite/...'
./target/release/iroh-test-client status                       # policy, path (direct/relay), RTT
./target/release/iroh-test-client fetch /System/Info/Public
./target/release/iroh-test-client fetch /Videos/<id>/stream?static=true --range bytes=0-1048575 -o part.bin
./target/release/iroh-test-client proxy --listen 127.0.0.1:8097  # open http://127.0.0.1:8097 in a browser
```

The proxy makes the remote Jellyfin usable from a local browser, so you can test real playback, seeking, and cancellation.

`invite`, `devices`, and `revoke` work while `serve` is running. They share `state.json` through `state.lock`, and the running host picks up changes within 2 seconds. The running host writes its current address to `endpoint-addr.json`, and `invite` embeds it in the link.

Configuration (flags or environment):

| Variable | Default | Meaning |
|---|---|---|
| `IROH_DATA_DIR` | `./iroh-data` | Identity, device registry, lock file |
| `IROH_ORIGIN` | `http://127.0.0.1:8096` | Local HTTP service to expose |
| `IROH_TRANSPORT_POLICY` | stored value, else `auto` | `auto`, `relay_only`, `p2p_only` |
| `IROH_RELAY_URL` | Iroh default relays | Custom relay URL, or `none` to disable relays (LAN or testing only) |
| `RUST_LOG` | `info,iroh=warn,noq=warn` | Log filter for `serve` |
| `IROH_CLIENT_DATA_DIR` | `./iroh-client-data` | Test client identity and paired host |

## Security notes

- The data directory holds the host's private key (`identity.key`). It is created with owner-only permissions on Unix. Back it up: if it is lost, every device must pair again.
- Invitation tokens are stored only as SHA-256 digests. They are single use and expire. Failed attempts are rate limited, and the limit is saved across restarts.
- `relay_only` and `p2p_only` are **best-effort**, not guaranteed. See `docs/transport-policies.md`.

## License

MIT OR Apache-2.0
