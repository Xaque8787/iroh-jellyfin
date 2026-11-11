#!/usr/bin/env bash
# Smoke test of the built image: Jellyfin starts with the bundled plugin installed and loaded,
# iroh-host runs beside it with its control API, the supervisor restarts iroh-host if it dies,
# and the container stops cleanly.
# Usage: scripts/ci/docker-smoke.sh <image>
set -euo pipefail

image=$1
work=$(mktemp -d)
name=jellyfin-iroh-smoke
mkdir -p "$work/config" "$work/cache" "$work/media" "$work/iroh-data"
cleanup() {
  echo "----- container log -----"
  docker logs "$name" 2>&1 | tail -n 200 || true
  docker rm -f "$name" > /dev/null 2>&1 || true
}
trap cleanup EXIT

docker run -d --name "$name" --init -p 127.0.0.1:18096:8096 \
  -e IROH_RELAY_URL=none \
  -v "$work/config:/config" -v "$work/cache:/cache" -v "$work/media:/media:ro" -v "$work/iroh-data:/iroh-data" \
  "$image"

in_container() { docker exec "$name" bash -c "$1"; }

# /health answers 200 "Degraded" while Jellyfin is still migrating; only "Healthy" means ready.
health() { curl -fs --max-time 5 http://127.0.0.1:18096/health || true; }
for _ in $(seq 1 90); do
  [ "$(health)" = Healthy ] && break
  sleep 2
done
test "$(health)" = Healthy
echo "jellyfin healthy"

# The plugin was copied into the persistent plugin directory and Jellyfin loaded it.
ls "$work/config/plugins/"
root=$(cd "$(dirname "$0")/../.." && pwd)
plugin_version=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["version"])' "$root/jellyfin-plugin/meta.json")
test -f "$work/config/plugins/JellyfinIroh_$plugin_version/Jellyfin.Plugin.Iroh.dll"
for _ in $(seq 1 30); do
  docker logs "$name" 2>&1 | grep -E 'Loaded plugin.*Iroh Remote Access' > /dev/null && break
  sleep 1
done
docker logs "$name" 2>&1 | grep -E 'Loaded plugin.*Iroh Remote Access'
# The plugin's admin routes exist and require a signed-in administrator (404 would mean not loaded).
route_status() { curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:18096/JellyfinIroh/Status; }
for _ in $(seq 1 15); do
  [ "$(route_status)" = 401 ] && break
  sleep 2
done
code=$(route_status)
echo "unauthenticated /JellyfinIroh/Status -> $code"
test "$code" = 401

# iroh-host runs with its control API on loopback inside the container.
for _ in $(seq 1 30); do
  in_container 'curl -fs http://127.0.0.1:47950/v1/health' && break
  sleep 1
done
echo
# The data directory is private to the container's user, so check the token from inside.
in_container 'test "$(stat -c %a /iroh-data/control.token)" = 600'
in_container 'curl -fs -H "Authorization: Bearer $(cat /iroh-data/control.token)" http://127.0.0.1:47950/v1/status' > "$work/status.json"
python3 -c "import json,sys; s=json.load(open(sys.argv[1])); assert s['control_api']==1 and s['relay']['mode']=='disabled', s; print('status ok', s['endpoint_id'])" "$work/status.json"
# The control API is not published outside the container.
if curl -fs --max-time 2 http://127.0.0.1:47950/v1/health; then
  echo "control API is reachable from outside the container" && exit 1
fi

# The supervisor restarts iroh-host independently of Jellyfin, keeping the same identity.
in_container 'for p in /proc/[0-9]*; do [ "$(cat "$p/comm" 2>/dev/null)" = iroh-host ] && kill "${p#/proc/}"; done; true'
sleep 1
for _ in $(seq 1 30); do
  in_container 'curl -fs http://127.0.0.1:47950/v1/health' > /dev/null 2>&1 && break
  sleep 1
done
in_container 'curl -fs -H "Authorization: Bearer $(cat /iroh-data/control.token)" http://127.0.0.1:47950/v1/status' > "$work/status2.json"
python3 -c "import json,sys; a,b=(json.load(open(p)) for p in sys.argv[1:]); assert a['endpoint_id']==b['endpoint_id'] and b['uptime_secs']<=a['uptime_secs']+5, (a,b); print('restarted ok')" "$work/status.json" "$work/status2.json"
docker logs "$name" 2>&1 | grep "iroh-host exited" > /dev/null
test "$(health)" = Healthy

started=$(date +%s)
docker stop -t 30 "$name"
elapsed=$(( $(date +%s) - started ))
echo "container stopped in ${elapsed}s"
test "$elapsed" -lt 25
test "$(docker inspect -f '{{.State.ExitCode}}' "$name")" != "137"
echo "docker smoke test passed"
