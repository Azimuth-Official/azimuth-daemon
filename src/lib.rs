//! Android-only library crate root.
//!
//! The Observer app loads this crate as `libazimuth_daemon.so` and drives the
//! Tier 1 SDR pipeline through the JNI exports in [`android_bridge`]. On every
//! other target this crate compiles to an empty library, so the existing
//! `azimuth-daemon` binary build (main.rs owns its own module tree) is
//! completely unaffected.
#![cfg(target_os = "android")]

mod api_client;
mod config;
mod identity;
mod observation;
mod pending;
mod regions;
mod sdr;

pub mod android_bridge;
