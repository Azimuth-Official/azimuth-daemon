# syntax=docker/dockerfile:1
#
# Azimuth Tier 1 SDR node — container image.
# Multi-stage: build the release binary, then ship a slim runtime.
# Native build per platform (buildx selects the platform); no cross-compile plumbing.

# ---- Stage 1: builder ----
FROM rust:1-bookworm AS builder

# libudev-dev: serialport's default `udev` feature links libudev at build time.
# gcc / make / perl / pkg-config (needed by vendored openssl-sys and rusb/libusb1-sys
# static builds) already ship in the rust:bookworm (buildpack-deps) base image.
RUN apt-get update \
 && apt-get install -y --no-install-recommends pkg-config libudev-dev \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy manifests + sources, then build against the lockfile. rtl-sdr-rs is pure-Rust;
# rusb (libusb) and reqwest's TLS (native-tls-vendored / OpenSSL) are vendored and
# statically linked, so the build pulls and compiles them here.
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

# ---- Stage 2: runtime ----
FROM debian:bookworm-slim AS runtime

# Runtime libraries:
#   ca-certificates — TLS trust store (reqwest needs the CA bundle even with vendored OpenSSL)
#   libudev1        — serialport udev enumeration at runtime
#   curl            — HEALTHCHECK probe
# No librtlsdr0 / libusb-1.0-0 / libssl3: SDR access is pure-Rust and libusb + OpenSSL
# are statically linked into the binary (verified with ldd during the build pipeline).
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates libudev1 curl \
 && rm -rf /var/lib/apt/lists/*

# Unprivileged runtime user. Real USB passthrough needs this user in the host device
# group (see DOCKER.md) or root; the no-hardware/stub path runs fine unprivileged.
RUN useradd --system --uid 10001 --user-group --home-dir /var/lib/azimuth \
      --shell /usr/sbin/nologin azimuth \
 && mkdir -p /var/lib/azimuth \
 && chown -R azimuth:azimuth /var/lib/azimuth

COPY --from=builder /app/target/release/azimuth-daemon /usr/local/bin/azimuth-daemon
COPY entrypoint.sh /usr/local/bin/entrypoint.sh
# Normalize potential CRLF (Windows checkout) so the shebang works in-container.
RUN sed -i 's/\r$//' /usr/local/bin/entrypoint.sh \
 && chmod +x /usr/local/bin/entrypoint.sh /usr/local/bin/azimuth-daemon

# OCI metadata — public-facing contact address only.
LABEL org.opencontainers.image.title="Azimuth Node" \
      org.opencontainers.image.description="Azimuth Tier 1 SDR node" \
      org.opencontainers.image.vendor="Azimuth" \
      org.opencontainers.image.authors="Azimuth <contact@azimuth.day>" \
      org.opencontainers.image.source="https://github.com/Azimuth-Official/azimuth-daemon" \
      org.opencontainers.image.version="0.2.3"

USER azimuth
VOLUME ["/var/lib/azimuth"]
EXPOSE 8420

# GET /status on the daemon's status server (entrypoint forces bind 0.0.0.0:8420).
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
  CMD curl -fsS http://127.0.0.1:8420/status || exit 1

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
