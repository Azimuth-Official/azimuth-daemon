# Building the Azimuth Daemon Installer

## Prerequisites

- NSIS 3.x (`winget install NSIS.NSIS`)
- Rust toolchain (`rustup`)
- NSSM (`winget install NSSM.NSSM` or copy nssm.exe to this directory)

## Build Steps

1. Build the daemon in release mode:
   ```
   cargo build --release
   ```

2. Copy `nssm.exe` into this directory (if not already present):
   ```
   copy "C:\path\to\nssm.exe" installer\nssm.exe
   ```

3. Build the installer:
   ```
   makensis installer\azimuth.nsi
   ```

4. Output: `installer\azimuth-daemon-setup.exe`

## What the Installer Does

- Copies `azimuth-daemon.exe`, `nssm.exe`, and `config.example.toml` to `C:\Program Files\Azimuth\`
- Creates `config.toml` from the example template (if not already present)
- Registers `AzimuthDaemon` as a Windows service via NSSM (auto-start at boot)
- Creates Start Menu shortcuts for configuration and uninstall
- Adds entry to Windows Add/Remove Programs

## Uninstaller

- Stops and removes the Windows service
- Removes all files except `config.toml` (preserves user configuration)
- Removes Start Menu shortcuts and registry entries
