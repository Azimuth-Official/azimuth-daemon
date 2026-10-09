#!/usr/bin/env bash
# Cross-build the Windows daemon (azimuth-daemon.exe) from Linux with cargo-xwin.
#
# Same leak protection as build-windows.ps1:
#   * Rust code  -> --remap-path-prefix for $HOME, the repo and CARGO_HOME, together with
#                   +crt-static in the SAME CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS
#                   (that per-target variable REPLACES any other rustflags source).
#   * C code     -> CFLAGS/CXXFLAGS=/DNDEBUG compiles libusb's and OpenSSL's assert()s out
#                   (their __FILE__ strings would otherwise embed the builder's paths).
#
# Extra for a Linux host: wdi-rs's build.rs emits its libwdi link-search only when the
# HOST is Windows (and with a backslash path), so on Linux the static libwdi.lib is never
# found. The search path and the Windows system libraries it needs are therefore added to
# the same rustflags value. The wdi-rs crate directory is located through
# `cargo metadata`, never hardcoded.
#
# Requirements: cargo-xwin, the x86_64-pc-windows-msvc rust target, python3, and the
# environment variable XWIN_ACCEPT_LICENSE set by YOU after reading the Microsoft SDK/CRT
# license that cargo-xwin downloads. This script never sets it.
#
# Usage: scripts/build-windows-xwin.sh [extra cargo args]
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
REPO_DIR="$(pwd)"
TARGET=x86_64-pc-windows-msvc

if [ -z "${HOME:-}" ]; then
  echo "build-windows-xwin.sh: HOME is not set; refusing to guess a remap path." >&2
  exit 1
fi
if [ -z "${XWIN_ACCEPT_LICENSE:-}" ]; then
  echo "build-windows-xwin.sh: XWIN_ACCEPT_LICENSE is not set. Read the Microsoft license that" >&2
  echo "cargo-xwin downloads and export XWIN_ACCEPT_LICENSE yourself; this script will not." >&2
  exit 1
fi
cargo xwin --version >/dev/null 2>&1 || { echo "build-windows-xwin.sh: cargo-xwin not found (cargo install cargo-xwin)" >&2; exit 1; }

# Directory of the wdi-rs crate (it holds libwdi/lib/libwdi.lib).
WDI_DIR="$(cargo metadata --format-version 1 --locked --filter-platform "$TARGET" \
  | python3 -I -c '
import json, os, sys
meta = json.load(sys.stdin)
paths = [p["manifest_path"] for p in meta["packages"] if p["name"] == "wdi-rs"]
if not paths:
    sys.exit("wdi-rs is not in the dependency graph for this target")
print(os.path.dirname(paths[0]))
')"
if [ ! -f "$WDI_DIR/libwdi/lib/libwdi.lib" ]; then
  echo "build-windows-xwin.sh: libwdi.lib not found in the wdi-rs crate (run: cargo fetch --target $TARGET)" >&2
  exit 1
fi

maps=("--remap-path-prefix=${HOME}=/_" "--remap-path-prefix=${REPO_DIR}=/src")
cargo_home="${CARGO_HOME:-$HOME/.cargo}"
case "$cargo_home" in
  "$HOME"/*) ;;                                   # already covered by the $HOME remap
  *) maps+=("--remap-path-prefix=${cargo_home}=/cargo") ;;
esac

libs="-L native=${WDI_DIR}/libwdi/lib -l advapi32 -l user32 -l ole32 -l setupapi -l newdev -l shell32"
export CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS="${maps[*]} -C target-feature=+crt-static ${libs}"

# NDEBUG for every C dependency the cc crate builds.
export CFLAGS=/DNDEBUG CXXFLAGS=/DNDEBUG
export CFLAGS_x86_64_pc_windows_msvc=/DNDEBUG CXXFLAGS_x86_64_pc_windows_msvc=/DNDEBUG

echo "azimuth-daemon Windows cross build — remapping ${#maps[@]} path prefixes (values not printed)"
cargo xwin build --release --target "$TARGET" "$@"

EXE="target/${TARGET}/release/azimuth-daemon.exe"
echo "built: ${EXE}"
sha256sum "$EXE"
