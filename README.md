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
| FM broadcast | Channel-relative survey (spectrum vs. the capture's own noise floor, FM-demodulation check), RDS PI code | 65–108 MHz (incl. OIRT and Japan bands); 200 kHz raster in the Americas and Korea, 100 kHz elsewhere |
| LTE | PSS/SSS cell ID detection | regional bands up to the tuner limit |
| DTV | ATSC pilot tone (`dtv_atsc`); DVB-T (`dtv_dvbt`) cyclic-prefix detection and DVB-T2 (`dtv_dvbt2`) P1-preamble detection | UHF 470 MHz up to the region's upper edge (most of Europe: 694 MHz) |
| GPS (optional) | NMEA over serial | reference timing and position truth |
| RTK (optional) | NTRIP client (GEODNET, Onocoy, RTK2GO, custom casters) | centimeter-level corrections |

## Features

- **Regional frequency scanning** — embedded region database (30+ countries, worldwide fallback) selects FM/LTE/DTV sweep plans from the node's location (`region = "AUTO"`, resolved offline from a bundled public-domain Natural Earth boundary dataset) or from `region = "XX"` / `--region XX`
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
| `region` | ISO country code for the frequency plan, or `AUTO` (any case) to use the country the node's location falls in | `AUTO` |
| `scanning.mode` | `config`, `sweep`, or `both` | `both` |
| `sdr.max_tuner_mhz` | Tuner frequency ceiling in MHz | `1766` |

### Region and frequency plan

`region = "AUTO"` (or `auto`/`Auto`) looks up the country containing the node's
latitude/longitude with an offline boundary dataset and applies that country's full
FM, LTE and DTV plan, exactly as if you had set its code. A location the dataset does
not place in a country (open water, or still `0,0`) gets the worldwide default plan and
a warning in the log. An explicit code always wins over the location. The effective
region and why it was chosen are reported by `GET /status` as `region_effective` and
`region_source` (`explicit`, `auto:location` or `default`) and logged at startup.

The boundary dataset is [Natural Earth](https://www.naturalearthdata.com/) admin-0
countries (1:10m, public domain), simplified and compacted into a 156 KB file inside the
binary (`data/country-boundaries.bin`, with its generator and verification tools in
`data/tools/`). Border accuracy: a position at least 5 km from a land border resolves to
the right country in all but rare narrow-feature cases (enclaves, river borders); within
a few km of a coastline, or on a small islet, it can resolve to no country and falls back
to the worldwide default plan. See `data/README.md` for the measured agreement figures.

### DTV: ATSC, DVB-T and DVB-T2

The region plan chooses the standard, channel width and channel numbering (ATSC 6 MHz in
North America; DVB-T/T2 on the 7 or 8 MHz raster in Europe and elsewhere, ending at that
country's upper DTT edge; channel centres are tuned, not channel edges). DVB detection
correlates the cyclic prefix for every FFT size and guard interval the standards allow.
DVB-T2 is detected, not decoded, and only from its P1 preamble (EN 302 755 clause 9.8):
`dtv_dvbt2` is emitted only when a P1 symbol is found, whatever the FFT size and guard
interval. A capture is 64 ms but a T2 frame lasts up to 250 ms, so a cyclic-prefix hit
without a P1 in the first capture is re-read over a 384 ms window and searched for a P1.
With no P1 in a complete window, a 2K/8K signal with a DVB-T guard interval is reported
as `dtv_dvbt` with `t2_possible: true`; a mode that exists only in T2 (1K, 4K, 16K, 32K
or a T2-only guard interval) is not reported at all and is counted in
`sweep.dtv.t2_unconfirmed`; an incomplete window (short or failed read, or a device
recovery during it) gives no label and is counted in `sweep.dtv.dvb_dwell_incomplete`.
Detection is validated on synthetic captures only, not on air. An observation is emitted
only when a signal is detected, with `source_id` `dvb_ch<N>` and `raw_data` holding
`fft_mode`, `guard_interval`, `rho`, `contrast`, `cfo_hz`, `channel`, `bandwidth_mhz` and
`t2_possible` (true for every `dtv_dvbt2` result).

### FM survey

FM discovery no longer judges each channel by the whole capture's power (which the
tuner's automatic gain makes meaningless). One capture covers about eight channels; each
is compared with that capture's own noise floor, then confirmed by FM-demodulating it
(and, when present, the stereo pilot and an RDS PI). There is no cap on the number of
stations, and every sweep evaluates every channel of the plan: `GET /status` shows
`evaluated`, `total`, `found` and `tune_failed` per band under `sweep`. The raster follows
the region (200 kHz in the US, Canada, Mexico, Brazil, Argentina, Chile and Korea; 100 kHz
elsewhere, including the worldwide default). A weak station that is an exact repeat of a
much stronger one on the neighbouring channel is indistinguishable from spill and is not
reported. Stations found earlier are kept with slightly relaxed gates.

### Windows: RTL-SDR driver

Windows needs the WinUSB driver bound to Interface 0 of the dongle. The desktop
installer does this for a dongle that is already plugged in; otherwise the app offers
**Install SDR driver** (one administrator prompt). The same step is available from a
terminal: `azimuth-daemon --install-driver` (run as administrator; prints one JSON line,
exit code 0 ok, 2 no device, 3 needs elevation, 4 install failed, 5 other). The daemon
itself never installs a driver. `GET /status` reports `driver.state` (`ok`,
`driver_missing`, `wrong_driver`, `not_found`, ...) and `POST /sdr/test` really opens the
device and answers `ok` (with the tuner name), `driver_missing`, `wrong_driver`, `in_use`
(with likely holders), `not_found` or `usb_error`.

### Logs and diagnostics

Logs are written next to the config file: on Windows
`%APPDATA%\day.azimuth.node\logs\azimuth-daemon.<date>.log` (seven daily files are kept;
set `AZIMUTH_LOG_DIR` to move them). `GET /log/path` returns the folder. `GET /status`
includes `sweep` (per band: when the last sweep started and finished, channels
evaluated of total, stations found, tune failures, next rediscovery), `gps` (state, port,
baud, why detection failed and the port candidates) and `sdr_diagnostics`. A
`[R82xx] PLL not locked!` message from the tuner driver is counted in
`sdr_diagnostics.pll_lock_failures` instead of being printed (per band, tunes whose PLL did not lock after the settle wait are in `sweep.<band>.pll_lock_failures`, with `settle_waits` and `tune_retries` beside them): the "PLL not locked" tuner chatter users used to see on the console no longer appears. Everything the tuner driver (`rtl_sdr_rs::tuners::r82xx`) logs is withheld from the console and the log file at every level, debug included.

### Local control API safety

Every state-changing request (`POST`) must carry a `Host` of `127.0.0.1`, `localhost` or `[::1]` on the bound port, and either no `Origin` or the desktop webview / the daemon's own dashboard. Other web pages, including pages on a DNS name rebound to 127.0.0.1, get `403`; so does any request, reads included, whose `Host` is not one of those names.

**The API has no authentication.** The checks stop web browsers only; a script or `curl` that can reach the port can spoof the headers. Never publish port 8420 beyond loopback (the shipped `docker-compose.yml` binds `127.0.0.1`) without an authenticating reverse proxy. `AZIMUTH_ALLOWED_HOSTS` only adds names to the accepted `Host`; it does not widen the `Origin` rule. In Docker (bound to `0.0.0.0`) the same host names are accepted on any published port; add other names with `AZIMUTH_ALLOWED_HOSTS` (comma-separated `host:port`).

## Dashboard

Open [http://127.0.0.1:8420](http://127.0.0.1:8420) while the daemon runs for real-time signal reception, GPS fix quality, upload statistics, and SDR device health.

## Build from source

```sh
# Requires Rust 1.75+ and libusb
git clone https://github.com/Azimuth-Official/azimuth-daemon.git
cd azimuth-daemon
cargo build --release
```

### Windows executable from Linux

`scripts/build-windows-xwin.sh` cross-builds `azimuth-daemon.exe` with
[cargo-xwin](https://github.com/rust-cross/cargo-xwin) (needs `cargo install cargo-xwin`,
`rustup target add x86_64-pc-windows-msvc` and `python3`):

```sh
export XWIN_ACCEPT_LICENSE=1   # only after reading the Microsoft SDK/CRT license cargo-xwin downloads
scripts/build-windows-xwin.sh
```

It applies the same path remapping and static CRT as `scripts/build-windows.ps1`
(`$HOME` becomes `/_`, the checkout becomes `/src`) and `/DNDEBUG` for the bundled C
libraries, and adds the libwdi link settings that `wdi-rs` only emits on a Windows host.
The script prints the executable path (`target/x86_64-pc-windows-msvc/release/`) and its
SHA-256; the script never sets `XWIN_ACCEPT_LICENSE` for you.

## Links

- [Website](https://azimuth.day)
- [Download](https://azimuth.day/download)
- [Explorer](https://azimuth.day/explorer)
- [Discord](https://discord.gg/3Da6xEWWTq)
- [Twitter/X](https://x.com/AzimuthDePIN)

## Current version

**v0.4.0** — setup, detection and reliability release.

- **DVB-T and DVB-T2 detection.** New cyclic-prefix detector and signal types `dtv_dvbt` and `dtv_dvbt2`; DVB regions now tune channel centres on the correct 7/8 MHz raster up to each country's DTT edge. This follows community reports from France and the UK that DVB-T muxes were being missed.
- **FM survey.** Channel-relative detection with FM-demodulation confirmation, no 3-station cap, every channel evaluated per sweep, region-aware 100/200 kHz raster, and per-band sweep status in `/status`.
- **USB stall recovery.** Some RTL2832U dongles intermittently stall a USB control transfer. The daemon now re-issues the tune, resets and re-initialises the dongle (open, sample rate, tuner, gain) and retries the same channel, instead of abandoning it; counters are in `sdr_diagnostics`, and a dongle that cannot be brought up is reported as `device_error` while the daemon stays alive.
- **`region = "AUTO"`** resolves the country from the node's location with a bundled Natural Earth boundary dataset (public domain) and applies that country's full FM/LTE/DTV plan; `region_effective` and `region_source` are reported.
- **Setup applies live.** A config saved by the setup wizard takes effect in the running daemon (no restart): it validates, applies location/region/scan plan and registers the node, and `/status` reports `config_state` (`awaiting_setup`, `ok`, `invalid`) instead of a frozen startup error. Observations are submitted under the live node id.
- **SDR and Windows driver.** Real `/sdr/test`, precise driver states, an elevated `--install-driver` mode, and no more "PLL not locked" console chatter (counted instead).
- **GPS.** Natural COM-port order, no auto-probing of phone/modem ports, baud promotion, precise errors, live NMEA preview.
- **API.** Per-item rejection reporting for observation batches; `POST /shutdown` and clear exit codes when another daemon holds port 8420 (43); CSRF and DNS-rebinding protection of the local control API.

The Windows driver installer uses [libwdi](https://github.com/pbatard/libwdi) 1.5.1 through the `wdi-rs` crate, under the LGPL option; see `THIRD_PARTY_NOTICES.md`. It generates a self-signed catalog certificate for the WinUSB package and installs it on the machine when the driver is installed.
