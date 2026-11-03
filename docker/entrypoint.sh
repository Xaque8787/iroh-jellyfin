#!/bin/bash
# Container entrypoint: installs the bundled plugin into Jellyfin's persistent plugin directory,
# then supervises iroh-host and Jellyfin as independent processes. iroh-host is restarted with
# backoff if it exits; the container stops when Jellyfin stops.
set -euo pipefail

log() { echo "[jellyfin-iroh] $*" >&2; }

install_plugins() {
  local plugins_dir="${JELLYFIN_DATA_DIR:-/config}/plugins"
  mkdir -p "$plugins_dir"
  local bundled name
  for bundled in /opt/jellyfin-iroh/plugins/*/; do
    name=$(basename "$bundled")
    # Older versions of this plugin would otherwise load alongside the new one.
    find "$plugins_dir" -mindepth 1 -maxdepth 1 -type d -name "${name%%_*}_*" ! -name "$name" -exec rm -rf {} +
    rm -rf "${plugins_dir:?}/$name.tmp"
    cp -R "$bundled" "$plugins_dir/$name.tmp"
    rm -rf "${plugins_dir:?}/$name"
    mv "$plugins_dir/$name.tmp" "$plugins_dir/$name"
    log "installed plugin $name into $plugins_dir"
  done
}

supervise_iroh() {
  local child=0 delay=1 started status
  trap 'if [ "$child" -ne 0 ]; then kill -TERM "$child" 2>/dev/null || true; wait "$child" 2>/dev/null || true; fi; exit 0' TERM INT
  while true; do
    started=$(date +%s)
    /usr/local/bin/iroh-host serve &
    child=$!
    status=0
    wait "$child" || status=$?
    child=0
    if [ $(( $(date +%s) - started )) -ge 60 ]; then delay=1; fi
    log "iroh-host exited with status $status; restarting in ${delay}s"
    # Backgrounded so a stop signal interrupts the wait instead of waiting out the sleep.
    sleep "$delay" &
    wait $! || true
    delay=$(( delay * 2 > 30 ? 30 : delay * 2 ))
  done
}

install_plugins
mkdir -p "${IROH_DATA_DIR:-/iroh-data}"

supervise_iroh &
iroh_supervisor=$!
/jellyfin/jellyfin "$@" &
jellyfin=$!

stop() {
  log "stopping"
  kill -TERM "$jellyfin" "$iroh_supervisor" 2>/dev/null || true
}
trap stop TERM INT

jellyfin_status=0
while kill -0 "$jellyfin" 2>/dev/null; do
  # A trapped signal interrupts wait early; loop until Jellyfin has really exited.
  status=0
  wait "$jellyfin" || status=$?
  jellyfin_status=$status
done
kill -TERM "$iroh_supervisor" 2>/dev/null || true
wait "$iroh_supervisor" 2>/dev/null || true
exit "$jellyfin_status"
