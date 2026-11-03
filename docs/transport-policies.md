# Transport Policies: Current Enforcement

| Policy | Intent | Enforcement in this build |
|---|---|---|
| `auto` | Direct when possible, relay fallback allowed | `not_restricted`: nothing to enforce |
| `relay_only` | All traffic through the configured relay | `best_effort_path_gate` |
| `p2p_only` | Media only over a verified direct path | `best_effort_path_gate` |

The host reports this label through `iroh-host doctor`. It will also appear in the control API status (Milestone 3). No restricted policy is reported as strictly enforced.

## What this build does

Every HTTP request is checked against the connection's currently selected path before it is forwarded:

- `p2p_only` waits up to 5 seconds for Iroh to select a direct path. If the selected path is still relay or unknown, it refuses the request with `transport_policy_denied`. It does not fall back to relay playback.
- `relay_only` refuses requests while the selected path is direct. The host also starts its endpoint with IP transports removed (`Builder::clear_ip_transports()`), so it has no direct sockets and every path it can open goes through the relay. `relay_only` is rejected at startup when relays are disabled (`IROH_RELAY_URL=none`).
- Changing the policy away from `relay_only` takes effect after a restart, because the endpoint's transports are fixed when it starts.
- Path changes are logged with their RTT, and the client's `status` command shows the path Iroh reports.

## What best-effort means

Iroh 1.4.1 picks the network path for a connection itself. The host observes the selected path through `Connection::paths()`, where each path reports `is_selected()`, `is_ip()`, and `is_relay()`.

Limitations:

1. The gate runs when a request starts. A response body already in flight is not cut off if Iroh switches paths mid-transfer, so under `p2p_only` a long transfer can continue over a relay after a path change.
2. The QUIC handshake, and any path probing Iroh does, happen before the gate can act.
3. Removing IP transports for `relay_only` is a strong mechanism but has not been validated on real networks. Whether the remote side can still send to the host directly, and how Iroh behaves without IP transports, are unverified. The label therefore stays `best_effort_path_gate`.
4. For `p2p_only`, direct IP addresses are still exchanged with the peer for hole punching, as they must be.

## Path to strict enforcement

Iroh 1.4.1 has `Builder::path_selector` behind the unstable `unstable-custom-transports` feature. It lets the endpoint choose which paths may carry traffic. The proposed strict implementation:

- `relay_only`: validate the `clear_ip_transports()` approach on real networks.
- `p2p_only`: a path selector that never selects relay paths, plus re-checking the path during body transfer and resetting the stream with code 4 if it leaves the direct path.

This is not implemented. The feature is marked unstable, and the behavior has not been validated with real traffic. Before any policy is labeled `strict`, it must pass tests that observe the real packet path on both a LAN and a NAT-separated network.
