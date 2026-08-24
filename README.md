# Azimuth Daemon

The Azimuth Daemon is the Tier 1 node software for the [Azimuth Network](https://azimuth.day) — a Signals-of-Opportunity (SoOP) decentralized positioning and timing network.

It connects to a USB Software-Defined Radio (SDR) and passively listens to existing radio broadcasts — LTE cell towers, digital TV, FM radio — extracting timing and identity data used to build GPS-independent positioning coverage. No transmitting, no license required. Purely receive-only.

## Quick start

1. Download for your platform from [azimuth.day/download](https://azimuth.day/download)
2. Create a free account at [azimuth.day](https://azimuth.day)
3. Run the setup wizard: `azimuth-daemon --setup`

Full getting-started guide: [azimuth.day/docs/getting-started](https://azimuth.day/docs/getting-started)

## Signal pipeline

| Signal | Method | Frequency range |
|--------|--------|----------------|
| FM broadcast | RDS PI code extraction + SNR | 65–108 MHz (incl. OIRT and Japan bands) |
| LTE | PSS/SSS cell ID detection | regional bands up to the tuner limit |
| DTV | Pilot tone detection | 470–862 MHz (ATSC / DVB-T) |
| GPS (optional) | NMEA over serial | reference timing and position truth |
| RTK (optional) | NTRIP client (GEODNET, Onocoy, RTK2GO, custom casters) | centimeter-level corrections |

## Features

- **Regional frequency scanning** — embedded region database (30+ countries, worldwide fallback) selects FM/LTE/DTV sweep plans automatically or via `region = "XX"` / `--region XX`
- **SDR self-healing** — 4-level recovery escalation: reopen device → USB bus reset → OS-level USB restart → clean process restart under the service manager
- **Local web dashboard** — real-time status at `http://127.0.0.1:8420` (signals, GPS fix, SDR health, upload stats)
- **HTTP control API** — status, SDR list/test, config write, scanner start/stop, RTK and GPS status endpoints
- **Resilient startup** — missing or broken config never crash-loops the daemon; it starts, reports the problem, and waits for setup
- **Node identity persistence** — machine-fingerprint identity survives reinstalls and container recreation without registering duplicate nodes
- **Cross-platform** — Windows, Linux (x86_64, aarch64, armv7, armv6, riscv64), macOS (Apple Silicon + Intel), Docker

## Hardware

### Required

| Component | Recommended | Price | Where to buy |
|-----------|-------------|-------|--------------|
| SDR receiver | RTL-SDR Blog V4 | ~$35 | [rtl-sdr.com](https://www.rtl-sdr.com/buy-rtl-sdr-dvb-t-dongles/) |
| SDR receiver (budget) | RTL-SDR Blog V3 | ~$30 | [rtl-sdr.com](https://www.rtl-sdr.com/buy-rtl-sdr-dvb-t-dongles/) |
| Antenna | Dipole antenna kit | — | Included with RTL-SDR Blog units |

### Optional

| Component | Purpose | Price |
|-----------|---------|-------|
| GPS receiver (u-blox VK-162 or similar) | Reference timing / position truth | ~$15 |
| RTK corrections (GEODNET, Onocoy, RTK2GO) | Centimeter-level positioning | varies |

> Any RTL2832U-based dongle works, but the RTL-SDR Blog V4 is recommended for its improved front end and wide tuning range for LTE coverage.

## Installation

### Windows

Download from [azimuth.day/download](https://azimuth.day/download):

- **Azimuth Node (desktop app)** — recommended; full GUI bundling this daemon
- Or build the standalone daemon from source (below) and run `azimuth-daemon --setup`

### Linux

Download the tarball for your architecture from [azimuth.day/download](https://azimuth.day/download) (x86_64, aarch64, armv7, armv6, riscv64):

```sh
tar xzf azimuth-daemon-<version>-linux-<arch>.tar.gz
cd azimuth-daemon-<version>-linux-<arch>
./azimuth-daemon --setup
./azimuth-daemon
```

### Docker

Pull the multi-architecture image (amd64 and arm64 are both published under the same tag):

```sh
docker pull ghcr.io/azimuth-official/azimuth-node:latest
```

Image tarballs are also published alongside the other downloads, with SHA-256 checksums,
for machines without registry access. See [DOCKER.md](DOCKER.md) for the full guide:

```sh
curl -LO https://azimuth.day/downloads/azimuth-node-docker-amd64.tar.gz
docker load < azimuth-node-docker-amd64.tar.gz
```

### macOS

Download the tarball for Apple Silicon (aarch64) or Intel (x86_64) from [azimuth.day/download](https://azimuth.day/download), then run the setup wizard as on Linux.

## Configuration

Interactive setup:

```sh
azimuth-daemon --setup
```

Or copy `config.example.toml` to `config.toml` and edit. The daemon reads `config.toml` from its working directory by default; point it elsewhere with `--config /path/to/config.toml`. The desktop app manages the config automatically in the platform config directory.

### Key settings

| Setting | Description | Default |
|---------|-------------|---------|
| `api.key` | Your Azimuth API key (Account Settings on azimuth.day) | required |
| `node.location` | Latitude / longitude of the node | required |
| `region` | ISO country code for the frequency plan, or `AUTO` | `AUTO` |
| `scanning.mode` | `config`, `sweep`, or `both` | `both` |
| `sdr.max_tuner_mhz` | Tuner frequency ceiling in MHz | `1766` |

## Dashboard

Open [http://127.0.0.1:8420](http://127.0.0.1:8420) while the daemon runs for real-time signal reception, GPS fix quality, upload statistics, and SDR device health.

## Build from source

```sh
# Requires Rust 1.75+ and libusb
git clone https://github.com/Azimuth-Official/azimuth-daemon.git
cd azimuth-daemon
cargo build --release
```

## Links

- [Website](https://azimuth.day)
- [Download](https://azimuth.day/download)
- [Explorer](https://azimuth.day/explorer)
- [Discord](https://discord.gg/3Da6xEWWTq)
- [Twitter/X](https://x.com/AzimuthDePIN)

## Current version

**v0.3.3** — August 2026
