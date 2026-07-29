#!/bin/sh
# Azimuth node container entrypoint.
# Generates config.toml from AZIMUTH_* environment variables on first boot, then
# execs the daemon. If a config already exists (e.g. persisted on the volume, or a
# user-mounted file) it is left untouched so the daemon's saved node.id / api.key
# survive restarts — delete the file or volume to re-generate from the environment.
set -eu

CONFIG_PATH="${AZIMUTH_CONFIG_PATH:-/var/lib/azimuth/config.toml}"

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

mkdir -p "$(dirname "$CONFIG_PATH")"

if [ ! -f "$CONFIG_PATH" ]; then
  echo "[entrypoint] Generating $CONFIG_PATH from AZIMUTH_* environment"
  cat > "$CONFIG_PATH" <<EOF
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

[scanning]
mode = "both"
interval_seconds = 30
weight_lte = 3
weight_dtv = 2
weight_fm = 1
retune_delay_ms = 100
capture_retries = 3
max_observations_per_cycle = 20

[scanning.fm]
frequencies = []
sweep_start = 87_500_000
sweep_end = 108_000_000
sweep_step = 200_000
sweep_threshold = -20.0
sweep_min_snr_db = 8.0

[scanning.lte]
frequencies = [631_000_000, 637_000_000, 737_000_000, 881_000_000]

[[scanning.lte.sweep_bands]]
start = 617_000_000
end = 652_000_000
step = 5_000_000

[[scanning.lte.sweep_bands]]
start = 729_000_000
end = 746_000_000
step = 5_000_000

[[scanning.lte.sweep_bands]]
start = 869_000_000
end = 894_000_000
step = 5_000_000

[scanning.dtv]
frequencies = []
sweep_start = 470_000_000
sweep_end = 608_000_000
sweep_step = 6_000_000
sweep_threshold = -45.0
EOF
else
  echo "[entrypoint] Using existing $CONFIG_PATH (AZIMUTH_* env ignored on this boot)"
fi

exec /usr/local/bin/azimuth-daemon --config "$CONFIG_PATH"
