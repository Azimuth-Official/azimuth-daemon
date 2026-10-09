#!/usr/bin/env bash
# Build the local cross image used for the ARMv6 target (Cross.toml, target
# arm-unknown-linux-gnueabihf -> azimuth-cross-armv6:local). Needs docker; run it once
# (or after changing packaging/cross/armv6.Dockerfile) before
#   cross build --release --target arm-unknown-linux-gnueabihf
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
IMAGE="azimuth-cross-armv6:local"

command -v docker >/dev/null 2>&1 || { echo "build-cross-armv6-image.sh: docker not found" >&2; exit 1; }

docker build --pull=false -f packaging/cross/armv6.Dockerfile -t "$IMAGE" packaging/cross

# Prove the headers are visible where the vendored libusb build looks for them.
docker run --rm "$IMAGE" test -f /usr/include/bits/libc-header-start.h
echo "built $IMAGE ($(docker image inspect --format '{{.Id}}' "$IMAGE"))"
