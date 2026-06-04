use crate::observation::Observation;
use tracing::warn;

/// SDR module — stub implementation until RTL-SDR hardware is connected.
/// When hardware is available, this will use rtlsdr_mt or rtl-sdr-rs crate
/// to capture IQ samples and detect signals (LTE PSS/SSS, DTV pilot, FM RDS).

pub struct SdrCapture {
    _device_index: u32,
    _sample_rate: u32,
    _center_frequency: u64,
    _gain: i32,
}

impl SdrCapture {
    pub fn new(device_index: u32, sample_rate: u32, center_frequency: u64, gain: i32) -> Self {
        warn!("SDR capture is a stub — no hardware driver loaded");
        Self {
            _device_index: device_index,
            _sample_rate: sample_rate,
            _center_frequency: center_frequency,
            _gain: gain,
        }
    }

    /// Returns true if an SDR device is connected and ready.
    pub fn is_connected(&self) -> bool {
        false
    }

    /// Capture a batch of observations. Returns empty vec in stub mode.
    pub fn capture_batch(&self) -> Vec<Observation> {
        Vec::new()
    }
}
