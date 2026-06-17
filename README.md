# Azimuth Daemon

Passive SDR signal scanner for the [Azimuth](https://azimuth.day) DePIN positioning network. Listens to FM, LTE, and DTV broadcasts using RTL-SDR hardware and submits observations to earn rewards. No transmission — receive-only.

## Quick start

1. Download the installer from [azimuth.day/download](https://azimuth.day/download)
2. Create a free account at [azimuth.day](https://azimuth.day)
3. Run the setup wizard: `azimuth-daemon --setup`

Full getting-started guide: [azimuth.day/docs/getting-started](https://azimuth.day/docs/getting-started)

## What it detects

| Signal | Method | Frequency range |
|--------|--------|----------------|
| FM broadcast | RDS PI code extraction + SNR | 87.5–108.0 MHz |
| LTE | PSS/SSS cell ID detection | 600–960 MHz (B12/B71/B5) |
| DTV (ATSC) | Pilot tone detection | 470–608 MHz |

## Hardware requirements

- RTL-SDR V3 or V4 dongle (~$30)
- Windows 10+ (Linux support coming)
- USB 2.0 port

## Build from source

```sh
# Requires Rust 1.75+ and libusb
git clone https://github.com/Azimuth-Official/azimuth-daemon.git
cd azimuth-daemon
cargo build --release
```

The binary is at `target/release/azimuth-daemon.exe`.

## Configuration

Copy `config.example.toml` to `config.toml` and edit, or use the interactive setup wizard:

```sh
azimuth-daemon --setup
```

See the [getting-started guide](https://azimuth.day/docs/getting-started) for detailed configuration options.

## Links

- [Website](https://azimuth.day)
- [Download](https://azimuth.day/download)
- [Explorer](https://azimuth.day/explorer)
- [Discord](https://discord.gg/3Da6xEWWTq)
- [Twitter/X](https://x.com/AzimuthDePIN)
