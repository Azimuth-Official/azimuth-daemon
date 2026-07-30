#!/bin/sh
# Azimuth node container entrypoint.
# Generates config.toml from AZIMUTH_* environment variables on first boot, then
# execs the daemon. If a config already exists (e.g. persisted on the volume, or a
# user-mounted file) it is left untouched so the daemon's saved node.id / api.key
# survive restarts — delete the file or volume to re-generate from the environment.
set -eu

CONFIG_PATH="${AZIMUTH_CONFIG_PATH:-/var/lib/azimuth/config.toml}"

# ── Docker auto-directory guard ────────────────────────────────────────
# When docker-compose bind-mounts a host FILE path that does not exist,
# Docker silently creates it as an empty DIRECTORY — then the config
# write below fails with "Is a directory" and the container crash-loops.
if [ -d "$CONFIG_PATH" ]; then
  if [ -n "$(ls -A "$CONFIG_PATH" 2>/dev/null)" ]; then
    echo "[entrypoint] FATAL: $CONFIG_PATH is a NON-EMPTY directory — refusing to touch it."
    echo "[entrypoint] Move its contents aside on the host, then restart."
    exit 1
  fi
  if rmdir "$CONFIG_PATH" 2>/dev/null; then
    echo "[entrypoint] Removed empty config.toml directory (Docker auto-created it from a missing bind-mount file)"
  else
    # rmdir fails when the directory IS the bind mountpoint — only fixable on the host.
    echo "[entrypoint] FATAL: $CONFIG_PATH is an empty DIRECTORY bind-mounted by docker-compose."
    echo "[entrypoint] Docker created it because the host file did not exist before 'docker compose up'."
    echo "[entrypoint] Fix on the HOST, then restart:"
    echo "[entrypoint]   docker compose down"
    echo "[entrypoint]   rm -rf ./config.toml"
    echo "[entrypoint]   # either create the file first:   touch ./config.toml"
    echo "[entrypoint]   # or remove the ./config.toml:... volume line and use AZIMUTH_* env vars"
    echo "[entrypoint]   docker compose up -d"
    echo "[entrypoint] Docs: https://azimuth.day/docs/docker"
    exit 1
  fi
fi

API_URL="${AZIMUTH_API_URL:-https://azimuth.day}"
API_KEY="${AZIMUTH_API_KEY:-}"
NODE_ID="${AZIMUTH_NODE_ID:-}"
HARDWARE_TYPE="${AZIMUTH_HARDWARE_TYPE:-tier1_rtlsdr}"
NODE_LABEL="${AZIMUTH_NODE_LABEL:-azimuth-docker-node}"
NODE_LAT="${AZIMUTH_NODE_LAT:-0.0}"
NODE_LON="${AZIMUTH_NODE_LON:-0.0}"
NODE_ALT="${AZIMUTH_NODE_ALT:-0.0}"
STATUS_BIND="${AZIMUTH_STATUS_BIND:-0.0.0.0:8420}"
HEARTBEAT_SECS="${AZIMUTH_HEARTBEAT_SECS:-15}"
SUBMIT_SECS="${AZIMUTH_SUBMIT_SECS:-30}"
SDR_SAMPLE_RATE="${AZIMUTH_SDR_SAMPLE_RATE:-2048000}"
SDR_GAIN="${AZIMUTH_SDR_GAIN:--1}"
REGION="${AZIMUTH_REGION:-AUTO}"

mkdir -p "$(dirname "$CONFIG_PATH")"

# Generate when the config is missing OR an empty file (an empty file is what
# a user creates with `touch` before file-bind-mounting — write through it).
if [ ! -s "$CONFIG_PATH" ]; then
  echo "[entrypoint] Generating $CONFIG_PATH from AZIMUTH_* environment"
  cat > "$CONFIG_PATH" <<EOF
# Region code (ISO country, e.g. US, DE, JP) or AUTO for the worldwide default.
# The daemon resolves this into FM/LTE/DTV sweep plans at startup.
region = "$REGION"

[api]
url = "$API_URL"
key = "$API_KEY"

[node]
id = "$NODE_ID"
hardware_type = "$HARDWARE_TYPE"
label = "$NODE_LABEL"

[node.location]
latitude = $NODE_LAT
longitude = $NODE_LON
altitude_m = $NODE_ALT

[daemon]
heartbeat_interval_secs = $HEARTBEAT_SECS
submit_interval_secs = $SUBMIT_SECS
status_bind = "$STATUS_BIND"

[sdr]
sample_rate = $SDR_SAMPLE_RATE
gain = $SDR_GAIN

# Sweep plans (FM range, LTE bands, DTV channels) come from the region code
# above — the daemon resolves them at startup, so nothing is hardcoded here.
[scanning]
mode = "both"
interval_seconds = 30

[scanning.dtv]
# Container nodes historically ran a more sensitive DTV sweep than the code
# default (-25.0) — keep that behavior explicit.
sweep_threshold = -45.0
EOF
else
  echo "[entrypoint] Using existing $CONFIG_PATH (AZIMUTH_* env ignored on this boot)"
fi

exec /usr/local/bin/azimuth-daemon --config "$CONFIG_PATH"
