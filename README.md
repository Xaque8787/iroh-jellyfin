# jellyfin-iroh

Private remote access to a home media server over [Iroh](https://www.iroh.computer/) peer-to-peer connections, without port forwarding or a hosted backend.

The core is a standalone Rust host, `iroh-host`. It knows nothing about Jellyfin. It forwards HTTP requests from paired devices to one configurable local origin. Jellyfin is the first integration, through an admin plugin. Another media server could bundle the same executable.

## Status

| Milestone | Scope | State |
|---|---|---|
| 1. Foundation | Transport protocol v1 messages and framing, policies, pairing primitives, HTTP forwarding primitives, host identity and state storage, `identity` and `doctor` commands, build validation CI | Compiled and tested by CI |
| 2. Rust host | Iroh endpoint, pairing over invite links, sessions, HTTP-over-Iroh streaming with Range and cancellation, revocation, policy gate, test client, end-to-end CI test | Compiled and tested by CI |
| 3. Control API, plugin, Docker | Control API v1 (`protocol/control-v1.md`), live session diagnostics, Jellyfin admin plugin, Docker image with process supervision and automatic plugin install, Compose, CI for plugin and image | Code written, waiting for its first CI run |
| Later | Cross-platform workflow, diagnostics, docs, Android TV fork spec | Not started |

## Repository layout

```
iroh/                     Rust workspace (pinned toolchain, committed Cargo.lock)
  crates/iroh-transport   Reusable protocol, framing, policies, pairing, HTTP forwarding
  crates/iroh-host        Host executable (identity, device registry, control API, commands) and iroh-test-client
jellyfin-plugin/          Jellyfin admin plugin (C#) and its tests
docker/                   Container entrypoint (plugin install, supervision)
Dockerfile, compose.yaml  Image built from local sources
protocol/                 Versioned protocol specifications (transport-v1, control-v1)
docs/                     Policies, versions, design notes
scripts/ci/               End-to-end tests, Docker smoke test, Range-capable test origin
.github/workflows/        Build validation
```

## Running with Docker (Jellyfin + iroh-host)

Requires only Docker with Compose. Everything is compiled inside the build from this repository's sources.

```bash
git clone <repository>
cd jellyfin-iroh
docker compose up -d --build
docker compose logs -f        # wait for Jellyfin and "control API v1 listening"
```

Open http://localhost:8096, finish Jellyfin's setup wizard, then go to Dashboard > Plugins > Iroh Remote Access. From there you can create invitation links, see connected devices with their path (direct or relay) and round-trip time, revoke devices, and change the transport policy.

How the image works:

- At every start, the entrypoint copies the bundled plugin into `./config/plugins/JellyfinIroh_<version>/` (replacing older versions of this plugin only), then launches Jellyfin.
- `iroh-host` runs as a separate process under the entrypoint's supervisor, which restarts it with backoff if it exits. Jellyfin does not start or stop it.
- The plugin talks to `iroh-host` only through control API v1 on `127.0.0.1:47950` inside the container. That port is not published. The token is read from `./iroh-data/control.token`.
- Folders next to `compose.yaml`: `./config`, `./cache`, `./media` (mounted read-only at `/media`; add it as a library), `./iroh-data`.

To update an existing installation after pulling repository changes:

```sh
docker compose build --pull
docker compose up -d --force-recreate
# Confirm the bundled host version (currently 0.4.0):
docker compose exec jellyfin iroh-host --version
```

The image builds the host and plugin from the checked-out source, not GitHub
release downloads. Build-time version checks require the host, plugin assembly
and plugin metadata versions to agree. Recreating the container installs the
bundled plugin over the persistent copy and removes older Iroh plugin versions;
Jellyfin configuration, media and Iroh identity remain in the existing mounts.
The Android AAR is a client dependency and is not needed in the server image.
The pinned Jellyfin server and its plugin SDK/ABI are updated together; `--pull`
does not move the image to a different Jellyfin version.

To pair a test device from another machine, use `iroh-test-client pair '<link>'` and `iroh-test-client proxy`, as described below.

## Prebuilt binaries

Download them from the repository's **Releases** page. Each release has one tarball per platform containing `iroh-host` and `iroh-test-client`, the Jellyfin plugin as a zip, a `.sha256` file for each, and a combined `SHA256SUMS`:

| File | Runs on |
|---|---|
| `jellyfin-iroh-<version>-x86_64-unknown-linux-musl.tar.gz` | Any 64-bit x86 Linux (static, no glibc requirement) |
| `jellyfin-iroh-<version>-aarch64-unknown-linux-musl.tar.gz` | Any 64-bit ARM Linux, for example a Raspberry Pi 4/5 |
| `jellyfin-iroh-<version>-android-arm64-v8a.tar.gz` | 64-bit Android 7+ (`adb shell` or Termux) |
| `jellyfin-iroh-<version>-android-armeabi-v7a.tar.gz` | 32-bit Android 7+, common on Android TV devices |
| `jellyfin-iroh-<version>-jellyfin-plugin.zip` | Jellyfin 12.4: unzip into the server's `plugins` directory and restart |

```sh
sha256sum -c --ignore-missing SHA256SUMS
```

The Linux builds pass the same end-to-end test as CI before publishing. The Android builds are compiled but not run in CI. The test client is for development and testing only.

For a test build without a release, run the **Binaries** workflow from the Actions tab (Run workflow) with **publish** unticked. The same files are attached to that run as artifacts.

### Making a release

One version number covers the whole project. To release `X.Y.Z`:

1. Set `version = "X.Y.Z"` in `iroh/Cargo.toml` and run `cargo update --workspace` in `iroh/` so `Cargo.lock` follows.
2. Set the plugin version to `X.Y.Z.0` in `jellyfin-plugin/meta.json` and in `AssemblyVersion` and `FileVersion` in `Jellyfin.Plugin.Iroh.csproj`.
3. Run `scripts/ci/check-version.sh` and commit to the default branch.
4. Start the release, either way:
   - From the Actions tab: **Binaries**, Run workflow on the default branch, tick **publish**. The workflow creates the `vX.Y.Z` tag on that commit.
   - From git: `git tag vX.Y.Z && git push origin vX.Y.Z`.

The **Binaries** workflow then checks that every version agrees (and matches the tag), builds and tests all platforms and the plugin, and publishes the release with generated notes. If any step fails, nothing is published and no tag is created by a manual run. A manual publish stops straight away if that version was already released. Versions with a suffix, such as `0.4.0-beta.1`, are marked as pre-releases; their plugin version is still `0.4.0.0`.

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
| `IROH_WEBSOCKETS` | stored value, else `on` | `on` forwards WebSocket connections; `off` refuses them (the pre-WebSocket behavior) |
| `IROH_WEBSOCKET_ORIGIN` | stored value, else `rewrite` | `rewrite` sends the origin's own address as the WebSocket `Origin` header; `pass_through` forwards the client's value |
| `RUST_LOG` | `info,iroh=warn,noq=warn,iroh_host=info,iroh_transport=info` | Log filter for `serve` |
| `IROH_CLIENT_DATA_DIR` | `./iroh-client-data` | Test client identity and paired host |
| `IROH_CONTROL_LISTEN` | `127.0.0.1:47950` | Control API address, or `off` |
| `IROH_CONTROL_ALLOW_REMOTE` | unset | Required to listen on a non-loopback address |
| `IROH_CONTROL_TOKEN` | generated `control.token` | Fixed control token (32+ characters) instead of the file |

The plugin reads `JELLYFIN_IROH_CONTROL_URL`, `JELLYFIN_IROH_CONTROL_TOKEN_FILE` and `JELLYFIN_IROH_CONTROL_TOKEN`; these override the plugin's saved settings. The Docker image sets the first two.

## Security notes

- The data directory holds the host's private key (`identity.key`). It is created with owner-only permissions on Unix. Back it up: if it is lost, every device must pair again.
- Invitation tokens are stored only as SHA-256 digests. They are single use and expire. Failed attempts are rate limited, and the limit is saved across restarts.
- `relay_only` and `p2p_only` are **best-effort**, not guaranteed. See `docs/transport-policies.md`.
- The control API token grants full control of pairing. It lives in `control.token` (owner-only) and never reaches the browser: the dashboard page calls the plugin, which requires a Jellyfin administrator and forwards a fixed set of operations.

## License

MIT OR Apache-2.0

## Android client SDK

The embeddable client engine in `iroh/crates/iroh-client` is shared by the existing
CLI and the Android SDK. See [Android SDK integration](docs/android/README.md) for
AAR installation, typed Kotlin APIs, security, lifecycle and media compatibility,
and [implementation plan](docs/android/implementation-plan.md) for architecture.
The separate **Android SDK** workflow builds the AAR and independent sample.

### Coordinated releases

Starting with v0.4.0, host/CLI binaries, the Jellyfin plugin and Android SDK share
one project release version. **Binaries** also runs the reusable Android SDK
workflow and waits for its emulator tests. Successful publishing attaches the
versioned `.aar`, its POM and checksums directly to the GitHub Release alongside
the executable archives and plugin zip; Maven Central is not required.

Update `iroh/Cargo.toml` and the plugin versions, run `cargo update --workspace`
in `iroh/`, then `bash scripts/ci/check-version.sh`. The Android version and file
names follow the workspace version automatically. Merge to `main`, wait for Build
Validation and Android SDK checks, then push the matching `vX.Y.Z` tag (or run
**Binaries** on `main` with **Publish** selected). Release tags must point to a
commit on the default branch. Manual builds with **Publish** unchecked remain
available on development branches.
