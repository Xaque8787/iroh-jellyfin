#!/usr/bin/env bash
# End-to-end test of a real host and client on one machine, with relays disabled so it needs no
# internet access: pair from an invite link, fetch whole and ranged bodies, check the invite is
# single use, revoke the device, and stop the host gracefully.
# Usage: scripts/ci/e2e.sh <path to target/debug>
set -euo pipefail

bin=$(cd "$1" && pwd)
work=$(mktemp -d)
host_log="$work/host.log"
export IROH_RELAY_URL=none
export RUST_LOG=info,iroh=warn,noq=warn,iroh_host=info,iroh_transport=info
pids=()
cleanup() {
  for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; done
  echo "----- host log -----"
  cat "$host_log" 2>/dev/null || true
}
trap cleanup EXIT

mkdir -p "$work/www"
head -c 3000000 /dev/urandom > "$work/www/movie.bin"
python3 "$(dirname "$0")/range-origin.py" 18096 "$work/www" &
pids+=($!)

host=(--data-dir "$work/host")
client=("$bin/iroh-test-client" --data-dir "$work/client" --name ci-client)

"$bin/iroh-host" serve "${host[@]}" --origin http://127.0.0.1:18096 > "$host_log" 2>&1 &
host_pid=$!
pids+=("$host_pid")

for _ in $(seq 1 50); do
  [ -s "$work/host/endpoint-addr.json" ] && break
  sleep 0.2
done
test -s "$work/host/endpoint-addr.json"
test "$(stat -c %a "$work/host/endpoint-addr.json")" = "600"

link=$("$bin/iroh-host" invite "${host[@]}" --label "CI client")
case "$link" in jellyfin-iroh://invite/*) ;; *) echo "unexpected link: $link"; exit 1 ;; esac

"${client[@]}" pair "$link"
test "$(stat -c %a "$work/client/host.json")" = "600"

# A used invite must not pair a second device.
if "$bin/iroh-test-client" --data-dir "$work/intruder" pair "$link"; then
  echo "invite link was accepted twice" && exit 1
fi

"${client[@]}" status

"${client[@]}" fetch /movie.bin --output "$work/full.bin"
cmp "$work/www/movie.bin" "$work/full.bin"

"${client[@]}" fetch /movie.bin --range bytes=1000000-1065535 --output "$work/part.bin" 2> "$work/part.headers"
grep -q "^status 206" "$work/part.headers"
grep -qi "^content-range: bytes 1000000-1065535/3000000" "$work/part.headers"
dd if="$work/www/movie.bin" of="$work/expected.bin" bs=1 skip=1000000 count=65536 status=none
cmp "$work/expected.bin" "$work/part.bin"

"${client[@]}" fetch /missing.bin --output "$work/missing.bin" 2> "$work/missing.headers"
grep -q "^status 404" "$work/missing.headers"

# The local proxy must forward Range for ordinary HTTP tools.
"${client[@]}" proxy --listen 127.0.0.1:18097 &
proxy_pid=$!
pids+=("$proxy_pid")
for _ in $(seq 1 50); do
  curl -s -o /dev/null http://127.0.0.1:18097/ && break
  sleep 0.2
done
test "$(curl -s -o "$work/curl.bin" -w '%{http_code}' -r 1000000-1065535 http://127.0.0.1:18097/movie.bin)" = "206"
cmp "$work/expected.bin" "$work/curl.bin"

# Control API v1. The proxy keeps its session open, so it must be listed with a direct path.
api=http://127.0.0.1:47950/v1
test "$(stat -c %a "$work/host/control.token")" = "600"
auth=(-H "Authorization: Bearer $(cat "$work/host/control.token")")
json() { python3 -c "import json,sys; d=json.load(sys.stdin); print(eval(sys.argv[1], {'d': d}))" "$1"; }
test "$(curl -sf "$api/health" | json 'd["control_api"]')" = "1"
test "$(curl -s -o /dev/null -w '%{http_code}' "$api/status")" = "401"
curl -sf "${auth[@]}" "$api/status" > "$work/status.json"
test "$(json 'd["relay"]["mode"]' < "$work/status.json")" = "disabled"
test "$(json 'd["devices"]' < "$work/status.json")" = "1"
curl -sf "${auth[@]}" "$api/sessions" > "$work/sessions.json"
cat "$work/sessions.json"
test "$(json 'd["sessions"][0]["name"]' < "$work/sessions.json")" = "ci-client"
test "$(json 'd["sessions"][0]["path"]["kind"]' < "$work/sessions.json")" = "direct"
test "$(curl -sf "${auth[@]}" "$api/devices" | json 'd["devices"][0]["connected"]')" = "True"

policy() { curl -s -o /dev/null -w '%{http_code}' "${auth[@]}" -X PUT -H 'Content-Type: application/json' -d "{\"policy\":\"$1\"}" "$api/policy"; }
test "$(policy p2p_only)" = "200"
test "$(policy relay_only)" = "409"
test "$(policy auto)" = "200"

# An invitation made through the API pairs a second device, which the API then revokes.
curl -sf "${auth[@]}" -X POST -H 'Content-Type: application/json' -d '{"label":"API phone","ttl_secs":600}' "$api/invitations" > "$work/invite.json"
phone=("$bin/iroh-test-client" --data-dir "$work/phone" --name api-phone)
"${phone[@]}" pair "$(json 'd["link"]' < "$work/invite.json")"
test "$(curl -sf "${auth[@]}" "$api/invitations" | json 'len(d["invitations"])')" = "0"
phone_id=$(curl -sf "${auth[@]}" "$api/devices" | json '[x for x in d["devices"] if x["name"] == "api-phone"][0]["endpoint_id"]')
test "$(curl -s -o /dev/null -w '%{http_code}' "${auth[@]}" -X DELETE "$api/devices/$phone_id")" = "204"
test "$(curl -s -o /dev/null -w '%{http_code}' "${auth[@]}" -X DELETE "$api/devices/$phone_id")" = "404"

kill -INT "$proxy_pid"
wait "$proxy_pid" || true

device=$("$bin/iroh-host" devices "${host[@]}" | awk '$1 == "active" { print $2 }')
test -n "$device"
"$bin/iroh-host" revoke "${host[@]}" "$device"
"$bin/iroh-host" devices "${host[@]}" | grep -q "^revoked  $device"

# The running host reloads state every 2 seconds.
sleep 3
if "${client[@]}" fetch /movie.bin --output "$work/denied.bin"; then
  echo "revoked device was still served" && exit 1
fi
if "${phone[@]}" fetch /movie.bin --output "$work/denied.bin"; then
  echo "device revoked through the control API was still served" && exit 1
fi

kill -TERM "$host_pid"
for _ in $(seq 1 50); do
  kill -0 "$host_pid" 2>/dev/null || break
  sleep 0.2
done
if kill -0 "$host_pid" 2>/dev/null; then
  echo "host did not stop within 10 seconds of SIGTERM" && exit 1
fi
wait "$host_pid"
echo "end-to-end test passed"
