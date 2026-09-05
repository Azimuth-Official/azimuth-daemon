#!/usr/bin/env bash
# Reproducible release build for the Linux cross targets, plus tarball packaging.
#
# WHY THIS SCRIPT EXISTS
# ----------------------
# A plain `cross build --release` embeds the builder's home directory in the shipped
# binary. Two independent mechanisms are needed to prevent that, and omitting either
# one silently reintroduces the leak:
#
#   * Rust code -> --remap-path-prefix in RUSTFLAGS. Covers panic locations, the
#     dependency source paths under the cargo registry, and debug info.
#   * C code    -> -ffile-prefix-map in CFLAGS/CXXFLAGS. Rust's --remap-path-prefix
#     does NOT apply to anything the cc crate compiles: vendored libusb (rusb),
#     libudev-sys (serialport), and vendored OpenSSL (reqwest) all embed __FILE__.
#
# cross mounts the host CARGO_HOME into the container, so inside the container the
# registry paths still begin with the host home directory in its container form
# (/mnt/<drive>/... when the host is Windows). Every prefix below is computed from the
# environment at build time; none is ever committed to this repository.
#
# PRECEDENCE TRAP: CARGO_TARGET_<TRIPLE>_RUSTFLAGS REPLACES RUSTFLAGS entirely, it does
# not merge. Any target needing an extra flag must repeat the whole remap set. v0.3.6
# shipped an armv6 binary with 781 home-directory strings because its per-target
# override carried only the linker argument.
#
# Usage:  scripts/build-release.sh [target ...]      (default: all five Linux targets)
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
REPO_DIR="$(pwd)"
VER="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
OUT="${OUT:-$REPO_DIR/dist}"
mkdir -p "$OUT"

# ---- prefixes, all computed from the environment -----------------------------------
PREFIXES=()
add_prefix() { [ -n "${1:-}" ] && PREFIXES+=("$1=$2") || true; }

# POSIX home of whoever is building.
add_prefix "${HOME:-}" "/_"
# Windows host: $USERPROFILE in its container-visible /mnt/<drive>/... form, and in the
# plain forward-slash form that some toolchains record.
if [ -n "${USERPROFILE:-}" ]; then
  WIN_HOME="$(printf '%s' "$USERPROFILE" | sed 's|\\|/|g')"                   # C:/Users/<u>
  MNT_HOME="$(printf '%s' "$WIN_HOME" | sed 's|^\([A-Za-z]\):|/mnt/\l\1|')"   # /mnt/c/Users/<u>
  add_prefix "$MNT_HOME" "/_"
  add_prefix "$WIN_HOME" "/_"
fi
# Container-side cargo home and the default cross project mount.
add_prefix "/root/.cargo" "/_cargo"
add_prefix "/project" "azimuth-daemon"
add_prefix "$REPO_DIR" "azimuth-daemon"

RUST_MAP=""; C_MAP=""
for p in "${PREFIXES[@]}"; do
  RUST_MAP="$RUST_MAP --remap-path-prefix=$p"
  C_MAP="$C_MAP -ffile-prefix-map=$p"
done

export RUSTFLAGS="${RUST_MAP# }"

# The C prefix map is OPT-IN (AZ_CFLAGS_MAP=1) for two reasons:
#
#   1. -ffile-prefix-map needs GCC 8+ / Clang 10+. The cross-rs 0.2.5 images ship older
#      GCC and hard-fail on it:
#        cc: error: unrecognized command line option '-ffile-prefix-map=...'
#      That aborts libusb1-sys and openssl-sys on all five Linux targets.
#   2. It is not needed here. cc-rs invokes the cross compilers from inside the
#      container with RELATIVE source paths (e.g. -c libusb/libusb/os/linux_netlink.c),
#      so the C objects never record a host path. Verified by scanning the shipped
#      v0.3.6 Linux binaries: amd64, aarch64, armv7 and riscv64 contained zero
#      home-directory strings, and the 781 found in armv6 were all Rust registry paths
#      from the per-target RUSTFLAGS override, not C.
#
# Windows/MSVC is the one target where C really does leak, through assert()'s __FILEW__;
# scripts/build-windows.ps1 handles that with NDEBUG instead of a prefix map.
if [ -n "${AZ_CFLAGS_MAP:-}" ]; then
  export CFLAGS="${C_MAP# }"
  export CXXFLAGS="${C_MAP# }"
fi

export PKG_CONFIG_ALLOW_CROSS=1
export PKG_CONFIG_SYSROOT_DIR=/
export MSYS2_ENV_CONV_EXCL="PKG_CONFIG_PATH;PKG_CONFIG_SYSROOT_DIR;RUSTFLAGS;CFLAGS;CXXFLAGS;CARGO_TARGET_ARM_UNKNOWN_LINUX_GNUEABIHF_RUSTFLAGS"
export MSYS_NO_PATHCONV=1

BUILD_TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_DIR/target}"

# Archive metadata is an identity channel of its own: tar stores the archiving account's
# NAME in the uname/gname header fields (byte 265 of the header) unless told otherwise,
# so a tarball can publish the builder's username even when its payload binary is clean.
# GNU tar and bsdtar spell the suppression differently, and passing the wrong dialect is
# a hard error, so pick by flavour. gzip -n additionally drops the timestamp and the
# original file name from the gzip header.
if tar --version 2>/dev/null | head -1 | grep -qi 'gnu tar'; then
  TAR_META=(--owner=0 --group=0 --numeric-owner "--mtime=@${SOURCE_DATE_EPOCH:-0}")
else
  TAR_META=(--numeric-owner --uid 0 --gid 0 --uname '' --gname '')
fi

echo "azimuth-daemon $VER — remapping ${#PREFIXES[@]} path prefixes (values not printed)"
echo "tar: $(tar --version 2>/dev/null | head -1)  target-dir: ${BUILD_TARGET_DIR#"$REPO_DIR/"}"

build() {
  local triple="$1" plat="$2" pcdir="$3"; shift 3
  echo "=== building $triple ($plat) ==="
  if [ -n "$pcdir" ]; then export PKG_CONFIG_PATH="$pcdir"; else unset PKG_CONFIG_PATH; fi
  cross build --release --target "$triple" "$@" || return 1
  # cross does not always honour CARGO_TARGET_DIR on the host side (it always mounts the
  # target tree at /target inside the container), so resolve where the binary actually
  # landed rather than assuming.
  local outdir="$BUILD_TARGET_DIR/$triple/release"
  [ -f "$outdir/azimuth-daemon" ] || outdir="$REPO_DIR/target/$triple/release"
  [ -f "$outdir/azimuth-daemon" ] || { echo "  no binary for $triple"; return 1; }
  ( cd "$outdir" \
      && tar "${TAR_META[@]}" -cf - azimuth-daemon | gzip -9n > "$OUT/azimuth-daemon-$VER-$plat.tar.gz" )
  ( cd "$OUT" && sha256sum "azimuth-daemon-$VER-$plat.tar.gz" > "azimuth-daemon-$VER-$plat.tar.gz.sha256" )
  echo "  OK  $plat -> $(stat -c%s "$OUT/azimuth-daemon-$VER-$plat.tar.gz") bytes"
}

R=""
run_one() {
  local key="$1"; shift
  if [ ${#SELECT[@]} -ne 0 ]; then
    local found=1
    for s in "${SELECT[@]}"; do [ "$s" = "$key" ] && found=0; done
    [ $found -eq 0 ] || return 0
  fi
  if "$@"; then R="$R $key:OK"; else R="$R $key:FAIL"; fi
}

SELECT=("$@")
run_one amd64   build x86_64-unknown-linux-gnu      linux-amd64   /usr/lib/x86_64-linux-gnu/pkgconfig
run_one aarch64 build aarch64-unknown-linux-gnu     linux-aarch64 /usr/lib/aarch64-linux-gnu/pkgconfig
run_one armv7   build armv7-unknown-linux-gnueabihf linux-armv7   /usr/lib/arm-linux-gnueabihf/pkgconfig

# armv6 needs one extra linker argument: the Ubuntu-armhf libudev.so baked into the
# image references getrandom@GLIBC_2.25, which the toolchain's glibc 2.17 predates, so
# the link fails unless that shared-library symbol is deferred to runtime (it resolves
# on any modern-glibc Pi).
#
# It is applied by extending RUSTFLAGS, NOT via CARGO_TARGET_<TRIPLE>_RUSTFLAGS. That
# per-target variable is just the env spelling of the `target.<triple>.rustflags` CONFIG
# key, and cargo ignores every config rustflags source whenever the RUSTFLAGS env var is
# set. v0.3.6 got away with the per-target variable only because it never set RUSTFLAGS
# at all; now that RUSTFLAGS carries the remap set, a per-target value would be dropped
# silently and the armv6 link would fail with an undefined getrandom reference.
SAVED_RUSTFLAGS="$RUSTFLAGS"
export RUSTFLAGS="-C link-arg=-Wl,--allow-shlib-undefined $SAVED_RUSTFLAGS"
run_one armv6   build arm-unknown-linux-gnueabihf   linux-armv6   /usr/lib/arm-linux-gnueabihf/pkgconfig
export RUSTFLAGS="$SAVED_RUSTFLAGS"

run_one riscv64 build riscv64gc-unknown-linux-gnu   linux-riscv64 "" --no-default-features

echo "SUMMARY:$R"
ls -la "$OUT"
