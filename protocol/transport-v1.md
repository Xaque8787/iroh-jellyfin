# Transport Protocol v1 (client to host)

Status: draft, implemented in `iroh/crates/iroh-transport`. Message shapes are locked by the wire-format tests in `protocol.rs`. Host session logic is in `host.rs`, and the reference client in `client.rs`.

## Connection

- Transport: an Iroh QUIC connection to the host's endpoint id.
- ALPN: `jellyfin-iroh/1`. A connection that negotiates any other ALPN is not served.
- Peer identity: the client's Iroh endpoint id, authenticated by Iroh's TLS handshake, is the device credential. No bearer token is sent after pairing.

## Framing

Each frame is a 4-byte big-endian unsigned length followed by that many bytes of UTF-8 JSON.

| Limit | Value |
|---|---|
| Control frames (HELLO, AUTH_*, PING/PONG) | 16 KiB |
| HTTP header frames | 64 KiB |
| Headers per message | 128 |
| `request_id` | 1 to 128 bytes |
| Request path | 8 KiB |

A zero-length frame, an oversized length, or JSON that is not a known message is a protocol violation. The receiver checks the length before allocating.

Every message carries a `type` field:
`HELLO`, `AUTH_REQUEST`, `AUTH_RESULT`, `HTTP_REQUEST`, `HTTP_RESPONSE`, `STREAM_ERROR`, `PING`, `PONG`.

## Control stream

The client opens the first bidirectional stream and sends:

```json
{"type":"HELLO","protocol":1,"name":"Living room TV","capabilities":[]}
{"type":"AUTH_REQUEST","protocol":1,"pairing_token":"<43-char token, only when pairing>"}
```

The host replies with:

```json
{"type":"AUTH_RESULT","outcome":"authorized|paired|denied","reason":null,"transport_policy":"auto|relay_only|p2p_only"}
```

- `authorized`: the endpoint id was already a paired, non-revoked device. Any token sent is ignored.
- `paired`: the token redeemed a valid invitation, and the endpoint id is now authorized.
- `denied`: `reason` holds one of the error codes below. The host then closes the connection with close code 2.

The client keeps the control stream open for the session's lifetime. It may send `{"type":"PING","nonce":7}` at any time, and the host answers `{"type":"PONG","nonce":7}`. That gives an application-level round-trip time. Iroh's own per-path RTT is reported separately. If the client finishes the control stream, the host closes the connection with code 0.

The handshake must complete within 10 seconds. Every 2 seconds the host re-checks that the device is still authorized. A revoked device's connection is closed with code 3, including any transfers in progress.

## Invitation links

```
jellyfin-iroh://invite/<base64url without padding of {"v":1,"addr":<iroh EndpointAddr>,"token":"<pairing token>"}>
```

`addr` is the host's full Iroh address: endpoint id, relay URL, and direct IP addresses as last published by the running host. A client can therefore dial without any lookup service. Links are capped at 8 KiB. A `v` other than 1 is rejected. The token works once and expires (default 1 hour, 60 seconds to 24 hours).

## Requests

One HTTP exchange per additional bidirectional stream. Streams are independent, so requests run concurrently.

```json
{"type":"HTTP_REQUEST","request_id":"a1b2","method":"GET","path":"/Videos/1/stream?static=true",
 "headers":[["range","bytes=0-1048575"]],"body":"none","content_length":null}
```

- `path` must be origin-form: it starts with a single `/`, has no spaces or control characters, and has no scheme or authority. The host always sends it to its configured origin, never to another host.
- `method` is an uppercase HTTP token.
- `body`: `none`, or `stream`. With `stream`, the raw request body follows the frame on the same stream, and the client's `finish` marks its end. Request bodies are capped at 32 MiB, and a larger body fails with `payload_too_large`. `content_length`, when present, must match the body and must be absent or 0 with `none`.
- The HTTP_REQUEST frame must arrive within 10 seconds of the stream opening, or the host resets the stream with code 1.
- Hop-by-hop headers (`connection`, `keep-alive`, `te`, `trailer`, `transfer-encoding`, `upgrade`, `proxy-*`, `host`) are dropped.

Success:

```json
{"type":"HTTP_RESPONSE","request_id":"a1b2","status":206,"headers":[["content-range","bytes 0-1048575/9000000"]]}
```

The raw response body bytes follow the frame on the same stream. A clean `finish` marks the end. A stream reset means the body is incomplete. Range requests are passed through unchanged, so the origin's `206` and `Content-Range` reach the client.

Failure before headers:

```json
{"type":"STREAM_ERROR","request_id":"a1b2","code":"upstream_unavailable","message":"..."}
```

Failure after headers: the host resets the stream with code 7 when the origin fails mid-body, or code 5 when it is shutting down. The client must treat the body as truncated.

Cancellation: the client may stop or reset the stream at any time, for example when the user seeks. The host notices immediately, drops the upstream request, and closes that origin connection. A seek is just a new stream with a new `Range`.

Transport policy: before forwarding, the host checks the connection's selected path against the policy. Under `p2p_only`, it waits up to 5 seconds for Iroh to select a direct path before refusing with `transport_policy_denied`. See `docs/transport-policies.md` for exactly what this does and does not guarantee.

## Error codes

`protocol_version_unsupported`, `protocol_violation`, `not_authorized`, `pairing_token_invalid`, `pairing_rate_limited`, `transport_policy_denied`, `bad_request`, `unsupported`, `payload_too_large`, `upstream_unavailable`, `upstream_timeout`, `revoked`, `shutting_down`.

## Close and reset codes

| Code | Meaning |
|---|---|
| 0 | Normal |
| 1 | Protocol violation |
| 2 | Not authorized |
| 3 | Device revoked |
| 4 | Transport policy |
| 5 | Host shutting down |
| 6 | Idle timeout |
| 7 | Origin failed mid-body (stream reset only) |

## Versioning

`protocol` in HELLO and AUTH_REQUEST, and the `/1` ALPN suffix, identify the version. An incompatible change gets a new ALPN (`jellyfin-iroh/2`). A host may serve several ALPNs during a transition. Additive optional fields are allowed within v1, and receivers ignore unknown fields.
