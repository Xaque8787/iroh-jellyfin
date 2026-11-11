#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
: "${ANDROID_NDK_HOME:?Set ANDROID_NDK_HOME to Android NDK 27.2.12479018}"
# NDK r27 provides 16 KiB-compatible shared libraries with these linker flags.
export RUSTFLAGS="${RUSTFLAGS:-} -C link-arg=-Wl,-z,max-page-size=16384"
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-4}
out="$root/android/iroh-client/src/main/jniLibs"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
cd "$root/iroh"
cargo ndk -t arm64-v8a -t armeabi-v7a -t x86_64 -P 24 -o "$stage" build --release --locked -p iroh-client-android
python3 "$root/scripts/android/verify-native.py" "$stage"
# Iroh dependencies also declare cdylibs. The SDK links their Rust code statically;
# ship only our JNI entrypoint, not unused dependency cdylibs.
for abi in arm64-v8a armeabi-v7a x86_64; do
  mkdir -p "$out/$abi"
  rm -f "$out/$abi"/libiroh-*.so "$out/$abi"/libiroh_relay-*.so
  install -m 0644 "$stage/$abi/libiroh_client.so" "$out/$abi/libiroh_client.so"
done
