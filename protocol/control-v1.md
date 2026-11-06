# Control API v1 (media server plugin to host)

Status: implemented in `iroh/crates/iroh-host/src/control.rs`. Used by the Jellyfin plugin. Designed so any media server (for example KMS) can manage the same `iroh-host` executable.

The control API is separate from the device transport (`transport-v1.md`) and is versioned independently.

## Transport

- HTTP/1.1 with JSON bodies on a TCP listener. Default `127.0.0.1:47950`, set with `--control-listen` / `IROH_CONTROL_LISTEN`, or `off` to disable it.
- By default the host only binds loopback addresses. A non-loopback address needs `--control-allow-remote`, because the API can create invitations.
- Requests are limited to 16 KiB bodies. Responses are `application/json; charset=utf-8`.
- Every path is prefixed with the major version, `/v1/`. Breaking changes get `/v2/`, and a host may serve both during a transition. Fields may be added to v1 responses, and clients must ignore unknown fields.

## Authentication

Every endpoint except `GET /v1/health` requires:

```
Authorization: Bearer <control token>
```

The token is the trimmed content of `control.token` in the host's data directory. `serve` creates it on first start with 256 random bits and owner-only permissions; an existing file shorter than 32 characters stops `serve` with an error rather than being replaced. Setting `IROH_CONTROL_TOKEN` (at least 32 characters) uses that value instead, and nothing is written. The host compares SHA-256 digests of the presented and expected tokens, so timing does not reveal how much of a guess matched. A missing or wrong token gets `401`.

The plugin, or any embedding server, reads the same file (or is given the same environment variable). Anyone who can read the data directory can manage the host. That is intended: the data directory already holds the host's private key.

## Errors

Non-2xx responses carry:

```json
{"error":{"code":"not_found","message":"no active device with that endpoint id"}}
```

| Status | `code` |
|---|---|
| 400 | `bad_request` |
| 401 | `unauthorized` |
| 404 | `not_found` |
| 405 | `method_not_allowed` |
| 409 | `conflict` |
| 413 | `payload_too_large` |
| 500 | `internal` |

## Endpoints

### `GET /v1/health` (no authentication)

```json
{"ok":true,"control_api":1}
```

### `GET /v1/status`

```json
{
  "control_api": 1,
  "transport_protocol": 1,
  "host_version": "0.1.0",
  "endpoint_id": "2118dc05...",
  "origin": "http://127.0.0.1:8096/",
  "relay": {"mode": "default", "url": null},
  "relay_connected": true,
  "addr": { "...": "Iroh EndpointAddr as JSON" },
  "policy": {
    "configured": "p2p_only",
    "enforcement": "best_effort_path_gate",
    "relay_transport_only": false,
    "restart_required": false
  },
  "websocket": {"enabled": true, "origin_header": "rewrite"},
  "devices": 3,
  "pending_invitations": 1,
  "sessions": 1,
  "uptime_secs": 5321
}
```

- `relay.mode` is `default`, `custom`, or `disabled`. `url` is set for `custom`.
- `relay_connected` is true when the endpoint's current address includes a relay.
- `policy.relay_transport_only` reports whether this running endpoint was started without IP transports, which is what `relay_only` does at startup.
- `restart_required` is true when the configured policy needs a restart to take full effect, meaning when switching to or from `relay_only`.

### `GET /v1/sessions`

Live sessions, with the path Iroh has selected right now:

```json
{"sessions":[{
  "endpoint_id":"9436...","name":"Living room TV","connected_at":1793740000,
  "path":{"kind":"direct","rtt_ms":1.3,"remote_addr":"Ip(192.168.1.20:40944)","open_paths":1},
  "requests":42,"active_requests":1,"active_websockets":1,"bytes_sent":73400320
}]}
```

`path.kind` is `direct`, `relay`, or `unknown`. `rtt_ms` and `remote_addr` come from Iroh's path diagnostics.

### `GET /v1/devices`

```json
{"devices":[{"endpoint_id":"9436...","name":"Living room TV","paired_at":1793700000,
  "invitation_id":"inv-3feafff70d54","revoked_at":null,"connected":true}]}
```

### `DELETE /v1/devices/{endpoint_id}`

Revokes an active device: `204`, or `404` if there is no active device with that id. Its open sessions close within about 2 seconds. A revoked device can pair again only with a new invitation.

### `GET /v1/invitations`

Unredeemed, unexpired invitations. Tokens are never stored, so they cannot be listed.

```json
{"invitations":[{"id":"inv-3feafff70d54","label":"Phone","created_at":1793740000,"expires_at":1793743600}]}
```

### `POST /v1/invitations`

Request: `{"label":"Phone","ttl_secs":3600}`. `ttl_secs` is optional, between 60 and 86400, default 900.

Response `201`:

```json
{"id":"inv-3feafff70d54","label":"Phone","created_at":1793740000,"expires_at":1793743600,
 "link":"jellyfin-iroh://invite/eyJ2Ijox..."}
```

The link holds the plaintext token. It is returned only once.

### `DELETE /v1/invitations/{id}`

Cancels an unredeemed invitation: `204`, or `404`.

### `PUT /v1/policy`

Request: `{"policy":"auto|relay_only|p2p_only"}`. Response `200` has the same shape as `status.policy`. It returns `409` when `relay_only` is requested but relays are disabled.

The policy is saved in `state.json`. Request admission follows it immediately. Starting or stopping relay-only transports needs a host restart, which `restart_required` reports. A `--policy` / `IROH_TRANSPORT_POLICY` setting overrides the saved value at every start.

### `PUT /v1/websocket`

Request: `{"enabled":true|false,"origin_header":"rewrite|pass_through"}`. Both fields are optional; a missing field keeps its current value, and unknown fields are rejected with `400`. Response `200` has the same shape as `status.websocket`:

```json
{"enabled":true,"origin_header":"rewrite"}
```

The settings are saved in `state.json` and apply without a restart. A new WebSocket checks them when it opens, and open WebSockets re-check them every 2 seconds, so turning WebSockets off closes the open ones. Plain HTTP requests are never affected. `--websockets` / `IROH_WEBSOCKETS` and `--websocket-origin` / `IROH_WEBSOCKET_ORIGIN` override the saved values at every start.

`status` includes the current `websocket` object, and each entry in `sessions` includes `active_websockets`.
