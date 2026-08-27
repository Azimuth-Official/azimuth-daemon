# Running an Azimuth Node in Docker

This repository ships a container image for the Azimuth Tier 1 SDR node daemon. Pull it
from the registry, or — if the machine has no registry access — load a released tarball or
build from source. Maintained by the Azimuth team.

## 1. Get the image

### Option A — pull from the registry (recommended)

```sh
docker pull ghcr.io/azimuth-official/azimuth-node:latest
```

The tag is multi-architecture: Docker selects amd64 or arm64 (Raspberry Pi 4/5) for you.
Pin a release instead of tracking `latest` if you want reproducible deployments:

```sh
docker pull ghcr.io/azimuth-official/azimuth-node:0.3.5
```

`docker compose up -d` (section 2) pulls this image on its own, so you can skip straight
there.

### Option B — load a released tarball (offline / air-gapped)

```sh
# amd64 (x86_64)
curl -LO https://azimuth.day/downloads/azimuth-node-docker-amd64.tar.gz
docker load < azimuth-node-docker-amd64.tar.gz

# arm64 (aarch64)
curl -LO https://azimuth.day/downloads/azimuth-node-docker-arm64.tar.gz
docker load < azimuth-node-docker-arm64.tar.gz
```

Verify the download against the published `azimuth-node-docker-sha256.txt`:

```sh
sha256sum -c azimuth-node-docker-sha256.txt
```

A loaded tarball is tagged `azimuth-node:latest`, so point `image:` in
`docker-compose.yml` at that tag, or retag it:

```sh
docker tag azimuth-node:latest ghcr.io/azimuth-official/azimuth-node:latest
```

### Option C — build from source

```sh
git clone https://github.com/Azimuth-Official/azimuth-daemon.git
cd azimuth-daemon
docker build -t azimuth-node:latest .
```

## 2. Run with Docker Compose

Edit the `AZIMUTH_*` variables in `docker-compose.yml`, then:

```sh
docker compose up -d
docker compose logs -f
```

Check health:

```sh
curl http://127.0.0.1:8420/status
```

## 3. Configuration (environment variables)

On **first boot** the entrypoint generates `/var/lib/azimuth/config.toml` from the
variables below and then persists it (later boots reuse the saved file — the daemon also
writes its assigned `node.id` back to it). To regenerate, delete the file or the volume
and restart. To use the full config (including the advanced `[scanning]`, `[ntrip]`, and
`[gps]` sections), mount your own `config.toml` at `AZIMUTH_CONFIG_PATH` instead —
but **create the host file before `docker compose up`** (e.g. `touch ./config.toml`):
if it does not exist, Docker creates an empty directory in its place and the container
refuses to start with a `FATAL: ... is an empty DIRECTORY` message.

| Variable | Maps to | Default | Notes |
|----------|---------|---------|-------|
| `AZIMUTH_API_URL` | `api.url` | `https://azimuth.day` | Azimuth backend base URL |
| `AZIMUTH_API_KEY` | `api.key` | *(empty)* | **Required** to register and earn; from the azimuth.day dashboard |
| `AZIMUTH_NODE_LABEL` | `node.label` | `azimuth-docker-node` | Human-readable node name |
| `AZIMUTH_HARDWARE_TYPE` | `node.hardware_type` | `tier1_rtlsdr` | `tier0_mobile` / `tier1_rtlsdr` / `tier2_gpsdo` |
| `AZIMUTH_NODE_LAT` | `node.location.latitude` | `0.0` | Set your real latitude |
| `AZIMUTH_NODE_LON` | `node.location.longitude` | `0.0` | Set your real longitude |
| `AZIMUTH_NODE_ALT` | `node.location.altitude_m` | `0.0` | Altitude in meters |
| `AZIMUTH_STATUS_BIND` | `daemon.status_bind` | `0.0.0.0:8420` | Status server bind (keep `0.0.0.0` in a container) |
| `AZIMUTH_HEARTBEAT_SECS` | `daemon.heartbeat_interval_secs` | `15` | |
| `AZIMUTH_SUBMIT_SECS` | `daemon.submit_interval_secs` | `30` | |
| `AZIMUTH_SDR_SAMPLE_RATE` | `sdr.sample_rate` | `2048000` | Hz |
| `AZIMUTH_SDR_GAIN` | `sdr.gain` | `-1` | `-1` = automatic gain control |
| `AZIMUTH_CONFIG_PATH` | *(config file location)* | `/var/lib/azimuth/config.toml` | Where the config is written/read |

## 4. USB / RTL-SDR passthrough (real hardware)

The container reaches the RTL-SDR through `/dev/bus/usb`; the compose file maps it:

```yaml
devices:
  - "/dev/bus/usb:/dev/bus/usb"
```

Because the container runs as the non-root `azimuth` user (uid 10001), that user must be
in the **host's** USB device group. The group *name* `plugdev` is unreliable across the
host/container boundary (the numeric gid rarely matches), so use one of:

1. **Numeric gid (recommended)** — find the host gid and pass it:
   ```sh
   getent group plugdev        # e.g. -> plugdev:x:46:
   ls -n /dev/bus/usb/001/     # the numeric group column is the device's gid
   ```
   ```yaml
   group_add:
     - "46"                    # the numeric gid from above
   ```
2. **Run as root** — add `user: "0:0"` to the service.
3. **Privileged** — add `privileged: true` (broadest access, least isolation).

Without hardware the daemon runs in **stub mode**: `/status` reports
`sdr_connected: false`, `device_count: 0`, and the daemon does not crash — useful for
verifying the container itself.

### GPS receiver (serial)

A USB GPS dongle appears as a serial device (`/dev/ttyUSB0` or `/dev/ttyACM0`) owned by
the host's `dialout` group. Map the device and grant the group:

```yaml
devices:
  - "/dev/ttyUSB0:/dev/ttyUSB0"
group_add:
  - "dialout"
```

The compose file adds `dialout` by name, which works when the host and container gids
match (both `20` on Debian/Ubuntu). If `getent group dialout` on the host shows a
different gid, use that number instead. The entrypoint logs a clear warning at startup if
a GPS device is present but not accessible.

## 5. State & identity persistence

`/var/lib/azimuth` holds `node-identity.json`, `rtk-config.json`, and the generated
`config.toml`. Keep it on a named volume (the compose file does) so the node keeps its
identity — and any RTK/NTRIP caster config enabled via the desktop app — across restarts
and does not re-register or lose its RTK setup.

## 6. Health endpoint

`GET /status` returns JSON with SDR connection state, device count, uptime, signal counts,
and any `config_error`. The image's `HEALTHCHECK` polls it, and fails (container shows
`unhealthy` in `docker ps` — this does NOT stop, kill, or restart it) whenever
`config_error` is non-null: most commonly a missing `AZIMUTH_API_KEY` or an unset
`(0,0)` location. The daemon runs fine in that state and keeps scanning; it just cannot
register, so the node will not appear on the dashboard. Check `docker logs` or
`curl http://127.0.0.1:8420/status` for the exact reason.

> **Security:** `/status` is unauthenticated. `docker-compose.yml` binds it to
> `127.0.0.1` on the host by default. Only expose it publicly behind an authenticating
> reverse proxy.
