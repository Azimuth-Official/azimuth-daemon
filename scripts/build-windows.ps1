# Reproducible release build of the Windows sidecar (azimuth-daemon.exe), the binary the
# desktop installer bundles.
#
# WHY THIS SCRIPT EXISTS
# ----------------------
# Two separate leaks have to be closed, and closing only one still ships a path:
#
#   1. Rust code. Without --remap-path-prefix the binary embeds the builder's home
#      directory hundreds of times (panic locations, registry source paths, debug info).
#      Supplied here through CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS, together
#      with +crt-static in the SAME value — a per-target rustflags variable REPLACES
#      any other rustflags source, so splitting them silently drops the static CRT.
#
#   2. C code. libusb (vendored through rusb, compiled by the cc crate with MSVC) uses
#      assert(). The MSVC assert macro expands to _wassert(_CRT_WIDE(#expr),
#      _CRT_WIDE(__FILE__), __LINE__), so the full source path of every asserting header
#      lands in the binary as a UTF-16LE string. --remap-path-prefix is a rustc flag and
#      does not reach the C compiler; MSVC has no path-remap option either. Defining
#      NDEBUG compiles those asserts out, which removes the strings at the source.
#      v0.3.6 shipped the builder's home this way inside libusb's threads_windows.h.
#
# The remap prefix is computed from the environment at build time and is never written
# into this repository.
$ErrorActionPreference = 'Stop'

if (-not $env:USERPROFILE) { throw 'build-windows.ps1: USERPROFILE is not set; refusing to guess a remap path.' }
$repo = Split-Path -Parent $PSScriptRoot
Set-Location $repo

$up    = $env:USERPROFILE
$maps  = @(
  "--remap-path-prefix=$up=/_",
  "--remap-path-prefix=$($up.Replace('\','/'))=/_",
  "--remap-path-prefix=$repo=azimuth-daemon",
  "--remap-path-prefix=$($repo.Replace('\','/'))=azimuth-daemon"
)
$env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS = ($maps -join ' ') + ' -C target-feature=+crt-static'

# NDEBUG for every C dependency the cc crate builds (vendored libusb, vendored OpenSSL).
foreach ($n in 'CFLAGS','CXXFLAGS','CFLAGS_x86_64-pc-windows-msvc','CXXFLAGS_x86_64-pc-windows-msvc',
               'CFLAGS_x86_64_pc_windows_msvc','CXXFLAGS_x86_64_pc_windows_msvc') {
  [Environment]::SetEnvironmentVariable($n, '/DNDEBUG')
}

Write-Output ("azimuth-daemon Windows release build — remapping {0} path prefixes (values not printed)" -f $maps.Count)
& cargo build --release --target x86_64-pc-windows-msvc @args
if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE" }
Write-Output ("built: target\x86_64-pc-windows-msvc\release\azimuth-daemon.exe")
