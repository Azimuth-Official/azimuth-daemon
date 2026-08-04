#!/usr/bin/env bash
set -euo pipefail

# Azimuth Daemon — Linux setup.
# Configures GPS serial-port permissions so the daemon can read the receiver
# without manual group work. Run once: sudo ./install.sh   (safe to re-run.)

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
UDEV_DIR="$SCRIPT_DIR/udev"
UDEV_DST="/etc/udev/rules.d"
DAEMON_BIN="$SCRIPT_DIR/azimuth-daemon"

echo "=== Azimuth Daemon Setup ==="

if [ "$(id -u)" -ne 0 ]; then
    echo "Error: run with sudo:  sudo ./install.sh" >&2
    exit 1
fi

# Resolve the real (non-root) user for the group add.
REAL_USER="${SUDO_USER:-$(logname 2>/dev/null || true)}"
if [ -z "$REAL_USER" ] || [ "$REAL_USER" = "root" ]; then
    echo "Warning: could not determine a non-root user; skipping dialout group add."
    echo "  Add yourself manually:  sudo usermod -aG dialout <your-username>"
else
    if id -nG "$REAL_USER" | tr ' ' '\n' | grep -qx dialout; then
        echo "User '$REAL_USER' already in the dialout group."
    else
        usermod -aG dialout "$REAL_USER"
        echo "Added '$REAL_USER' to the dialout group."
        echo "  Log out and back in (or reboot) for this to take effect."
    fi
fi

# Install every bundled udev rule (GPS + RTL-SDR) and reload.
if [ -d "$UDEV_DIR" ]; then
    installed=0
    for rule in "$UDEV_DIR"/*.rules; do
        [ -e "$rule" ] || continue
        cp "$rule" "$UDEV_DST/"
        echo "Installed $(basename "$rule") -> $UDEV_DST/"
        installed=1
    done
    if [ "$installed" -eq 1 ]; then
        udevadm control --reload-rules 2>/dev/null || true
        udevadm trigger --subsystem-match=tty 2>/dev/null || true
        udevadm trigger --subsystem-match=usb 2>/dev/null || true
        echo "Reloaded udev rules."
    fi
else
    echo "Warning: no udev/ directory beside this script; skipping device rules."
fi

if [ -f "$DAEMON_BIN" ]; then
    chmod +x "$DAEMON_BIN"
    echo "Made azimuth-daemon executable."
fi

echo ""
echo "=== Setup complete ==="
echo "If the dialout group was just added, log out and back in, then run:"
echo "  ./azimuth-daemon"
echo "Check your GPS device with:  ls -la /dev/ttyUSB* /dev/ttyACM*"
