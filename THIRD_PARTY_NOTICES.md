# Third-party notices

## libwdi (Windows driver installation)

The Windows build uses [libwdi](https://github.com/pbatard/libwdi) 1.5.1 (Pete Batard
and contributors) through the [`wdi-rs`](https://crates.io/crates/wdi-rs) 0.1.1 crate to
bind the WinUSB driver to Interface 0 of an RTL-SDR (`azimuth-daemon --install-driver`).

- **License.** libwdi is available under the GNU GPL and the GNU Lesser General Public
  License (version 3 or later). Azimuth uses it under the **LGPL v3** option.
  The full text ships in `licenses/LGPL-3.0.txt` (source: <https://www.gnu.org/licenses/lgpl-3.0.txt>); installers must include that file next to the daemon.
- **What is linked.** `wdi-rs` links a prebuilt static `libwdi.lib` that its author
  built from a fork of libwdi (<https://github.com/piersfinlayson/libwdi>) whose only
  changes to the upstream code are to the CI jobs. SHA-256 of that file in `wdi-rs`
  0.1.1: `e01cc45d18e6bdafeed3ab2cf23a584b0deddcd7bbe8213ba71584e26495e43e`.
- **Relinking (LGPL section 4).** The complete Azimuth daemon source is public. To use a
  modified libwdi, build libwdi from source following the instructions in its
  repository, replace `libwdi/lib/libwdi.lib` in the `wdi-rs` crate (vendor it with
  `cargo vendor`), and rebuild the daemon with `cargo build --release`.
- **Certificate behaviour.** libwdi generates the WinUSB driver package on the fly,
  signs its catalog with a self-signed certificate created at install time, and adds
  that certificate to the machine's trusted publishers store so Windows accepts the
  package. The certificate is created locally; no key material is shipped. It is NOT
  removed again when the driver or the app is uninstalled: it stays in the machine's
  trusted publishers store until an administrator deletes it.
- **Microsoft SDK.** libwdi is built with the Microsoft Windows SDK; its redistributable
  components fall under the SDK license.

## libusb (USB access, statically linked)

The daemon reads RTL-SDR dongles through [libusb](https://libusb.info/) 1.0, statically
linked into every `azimuth-daemon` artifact (Linux tarballs, the Docker image, the Windows
executable) via the vendored copy in the `libusb1-sys` 0.7.0 crate.

- **License.** GNU Lesser General Public License, version 2.1 or later. The full text
  ships in `licenses/LGPL-2.1.txt`.
- **Copyright.** Copyright (C) 2001 Johannes Erdfelt, 2007-2009 Daniel Drake, 2010-2012
  Peter Stuge, 2008-2016 Nathan Hjelm, 2009-2013 Pete Batard, 2009-2013 Ludovic Rousseau,
  2010-2012 Michael Plante, 2011-2013 Hans de Goede, 2012-2013 Martin Pieuchot, 2012-2013
  Toby Gray, 2013-2018 Chris Dickens and other libusb contributors (see the `AUTHORS`
  file in the libusb source).
- **Modifications.** None. libusb is used unmodified. The corresponding source is the
  public libusb release vendored by `libusb1-sys` 0.7.0 (<https://crates.io/crates/libusb1-sys>).
- **Relinking (LGPL section 6).** The complete Azimuth daemon source is public in this
  repository. To use a modified libusb, point `libusb1-sys` at your build (for example
  with `cargo vendor` and a patched copy of the crate) and rebuild the daemon with
  `cargo build --release`.

## MPL-2.0 crates

- `rtl-sdr-rs` 0.3.3 and `serialport` 4.9.0 are licensed under the Mozilla Public
  License 2.0 (file-level copyleft). They are used unmodified as published on
  crates.io; their source is available there (<https://crates.io/crates/rtl-sdr-rs>,
  <https://crates.io/crates/serialport>).

## unescaper (dual licence, MIT elected)

- `unescaper` 0.1.8 (pulled in by `serialport` on Linux targets) is dual-licensed
  "GPL-3.0/MIT". Azimuth elects the **MIT** option; no GPL-3.0 obligation applies.

Azimuth itself is licensed under the Apache License, Version 2.0; see `LICENSE` and `NOTICE` at the repository root. This file lists third-party components only.
