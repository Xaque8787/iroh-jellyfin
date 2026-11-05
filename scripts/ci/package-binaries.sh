#!/usr/bin/env bash
# Packs iroh-host and iroh-test-client into a tarball with a SHA-256 checksum file.
# Usage: scripts/ci/package-binaries.sh <release dir> <platform label> <output dir>
set -euo pipefail

src=$(cd "$1" && pwd)
label=$2
out=$3
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$(dirname "$0")/../../iroh/Cargo.toml" | head -n 1)
name="jellyfin-iroh-$version-$label"

stage=$(mktemp -d)
mkdir -p "$stage/$name" "$out"
install -m 0755 "$src/iroh-host" "$src/iroh-test-client" "$stage/$name/"
tar -C "$stage" -czf "$out/$name.tar.gz" "$name"
(cd "$out" && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
ls -l "$out"
