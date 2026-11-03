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
export RUST_LOG=info,iroh=warn,noq=warn
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
