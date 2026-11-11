#!/usr/bin/env bash
# Run with an already-booted Android emulator. No external relay or media server required.
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
work=$(mktemp -d)
pids=()
cleanup() {
  for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; done
  # Host logs never contain invite tokens, but generated links are deliberately not printed.
  tail -n 100 "$work/host.log" || true
  rm -rf "$work"
}
trap cleanup EXIT
mkdir -p "$work/www"
head -c 1048576 /dev/urandom > "$work/www/movie.bin"
python3 "$root/scripts/ci/range-origin.py" 18096 "$work/www" > "$work/origin.log" 2>&1 &
pids+=($!)
export IROH_RELAY_URL=none
bin="${CARGO_TARGET_DIR:-$root/iroh/target}/debug/iroh-host"
"$bin" serve --data-dir "$work/host" --origin http://127.0.0.1:18096 --control-listen off > "$work/host.log" 2>&1 &
pids+=($!)
for _ in $(seq 1 100); do
  [ -s "$work/host/endpoint-addr.json" ] && break
  sleep 0.1
done
test -s "$work/host/endpoint-addr.json"
# Translate address hints in the invitation; do not race the host's address-file writer.
invite=$("$bin" invite --data-dir "$work/host" --label 'Android SDK integration' | python3 "$root/scripts/android/rewrite-emulator-invite.py")
cd "$root/android"
./gradlew :iroh-client:connectedDebugAndroidTest "-Pandroid.testInstrumentationRunnerArguments.irohInvite=$invite"
