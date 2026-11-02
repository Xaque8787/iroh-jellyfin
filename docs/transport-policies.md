# Transport Policies: Current Enforcement

| Policy | Intent | Enforcement in this build |
|---|---|---|
| `auto` | Direct when possible, relay fallback allowed | `not_restricted`: nothing to enforce |
| `relay_only` | All traffic through the configured relay | `best_effort_path_gate` |
| `p2p_only` | Media only over a verified direct path | `best_effort_path_gate` |

The host reports this label through `iroh-host doctor`. It will also appear in the control API status (Milestone 3). No restricted policy is reported as strictly enforced.

## What best-effort means

Iroh 1.4.1 picks the network path for a connection itself. The host can observe the selected path through `Connection::paths()`, where each path reports `is_selected()`, `is_ip()`, and `is_relay()`. It cannot stop Iroh from using a path through stable APIs.

The planned gate (Milestone 2) refuses a request, or resets a body stream, while the selected path is not allowed:

- `p2p_only` refuses media while the selected path is relay or unknown. It does not fall back to relay playback.
- `relay_only` refuses requests while the selected path is direct.

Limitations of the gate:

1. Packets can still cross a disallowed path between checks. The QUIC handshake, and any path probing Iroh does, happen before the gate can act.
2. `relay_only` cannot stop Iroh from discovering and switching to a direct path. It can only stop serving while that path is selected, which may interrupt playback.
3. Direct IP addresses are still exchanged with the peer for hole punching.

## Path to strict enforcement

Iroh 1.4.1 has `Builder::path_selector` behind the unstable `unstable-custom-transports` feature. It lets the endpoint choose which paths may carry traffic. The proposed strict implementation:

- `relay_only`: a path selector that never selects IP paths, plus not advertising direct addresses.
- `p2p_only`: a path selector that never selects relay paths, plus a gate that refuses requests until a direct path is selected.

This is not implemented. The feature is marked unstable, and the behavior has not been validated with real traffic. Before any policy is labeled `strict`, it must pass tests that observe the real packet path on both a LAN and a NAT-separated network.
