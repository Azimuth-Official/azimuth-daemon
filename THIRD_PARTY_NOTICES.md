# Third-party notices

## libwdi (Windows driver installation)

The Windows build uses [libwdi](https://github.com/pbatard/libwdi) 1.5.1 (Pete Batard
and contributors) through the [`wdi-rs`](https://crates.io/crates/wdi-rs) 0.1.1 crate to
bind the WinUSB driver to Interface 0 of an RTL-SDR (`azimuth-daemon --install-driver`).

- **License.** libwdi is available under the GNU GPL and the GNU Lesser General Public
  License (version 3 or later). Azimuth uses it under the **LGPL v3** option.
  Text: <https://www.gnu.org/licenses/lgpl-3.0.txt>.
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
  package. The certificate is created locally; no key material is shipped.
- **Microsoft SDK.** libwdi is built with the Microsoft Windows SDK; its redistributable
  components fall under the SDK license.
