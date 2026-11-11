#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
python3 "$root/scripts/android/prepare-verifier.py"
bash "$root/scripts/android/build-native.sh"
cd "$root/android"
./gradlew :iroh-client:assembleRelease :iroh-client:generatePomFileForReleasePublication
mkdir -p "$root/dist"
cp iroh-client/build/outputs/aar/iroh-client-release.aar "$root/dist/iroh-client-android-0.1.0.aar"
cp iroh-client/build/publications/release/pom-default.xml "$root/dist/iroh-client-android-0.1.0.pom"
python3 "$root/scripts/android/verify-aar.py" "$root/dist/iroh-client-android-0.1.0.aar"
(cd "$root/dist" && sha256sum iroh-client-android-0.1.0.aar > iroh-client-android-0.1.0.aar.sha256)
