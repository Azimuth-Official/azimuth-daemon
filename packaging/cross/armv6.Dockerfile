# Local cross image for the ARMv6 (Raspberry Pi 1 / Zero) daemon build:
#   azimuth-cross-armv6:local   (referenced by Cross.toml, target arm-unknown-linux-gnueabihf)
# Build it with scripts/build-cross-armv6-image.sh.
#
# The stock cross-rs image omits the armhf libc development headers. The vendored libusb
# build for the armhf target needs <bits/libc-header-start.h> and friends, which Ubuntu's
# multiarch layout keeps under /usr/include/arm-linux-gnueabihf/, while the cross C
# compiler looks in /usr/include. So: install the armhf headers, then symlink-merge that
# multiarch directory into /usr/include (existing files are never overwritten).
#
# Base image pinned by digest (the multi-arch index of ghcr.io/cross-rs/
# arm-unknown-linux-gnueabihf:0.2.5, resolved with `docker buildx imagetools inspect`).
FROM ghcr.io/cross-rs/arm-unknown-linux-gnueabihf:0.2.5@sha256:2e3ba7a2284844975124febea982bda2586ae4910ef51294729284ff31b0aec5

RUN dpkg --add-architecture armhf \
 && apt-get update \
 && apt-get install -y --no-install-recommends libc6-dev:armhf linux-libc-dev:armhf \
 && cp -rsn /usr/include/arm-linux-gnueabihf/. /usr/include/ \
 && rm -rf /var/lib/apt/lists/*
