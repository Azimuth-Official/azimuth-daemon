#![allow(dead_code)]

use crate::config::{DtvScanConfig, FmScanConfig, LteScanConfig, ScanningConfig};
use crate::observation::Observation;
use crate::sdr::{SdrManager, calculate_signal_power};
use chrono::Utc;
use std::collections::HashMap;
use tracing::{debug, info, warn};

/// Result from a single scan operation.
pub struct ScanResult {
    pub signal_type: String,
    pub frequency_hz: u64,
    pub signal_strength_dbm: f32,
    pub snr_db: Option<f32>,
    pub source_id: Option<String>,
    pub raw_data: Option<serde_json::Value>,
}

/// Discovery results from auto-sweep.
pub struct DiscoveryResult {
    pub fm_stations: Vec<u64>,
    pub lte_cells: Vec<u64>,
    pub dtv_channels: Vec<u64>,
}

/// Scan mode parsed from config string.
#[derive(Debug, Clone, PartialEq)]
pub enum ScanMode {
    Config,
    Sweep,
    Both,
}

impl ScanMode {
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "config" => ScanMode::Config,
            "sweep" => ScanMode::Sweep,
            _ => ScanMode::Both,
        }
    }
}

/// Multi-station scanning orchestrator with weighted scheduling.
pub struct Scanner {
    mode: ScanMode,
    interval_seconds: u32,
    schedule: Vec<ScanType>,
    schedule_index: usize,
    fm_frequencies: Vec<u64>,
    lte_frequencies: Vec<u64>,
    dtv_frequencies: Vec<u64>,
    fm_config: FmScanConfig,
    lte_config: LteScanConfig,
    dtv_config: DtvScanConfig,
    dbm_offset: f32,
    discovered: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum ScanType {
    Fm,
    Lte,
    Dtv,
}

/// Default IQ capture size for signal processing (128K samples = 256KB).
const CAPTURE_BYTES: usize = 262_144;

impl Scanner {
    pub fn new(config: &ScanningConfig, dbm_offset: f32) -> Self {
        let mode = ScanMode::from_str(&config.mode);

        // Build weighted round-robin schedule
        let mut schedule = Vec::new();
        for _ in 0..config.weight_fm {
            schedule.push(ScanType::Fm);
        }
        for _ in 0..config.weight_lte {
            schedule.push(ScanType::Lte);
        }
        for _ in 0..config.weight_dtv {
            schedule.push(ScanType::Dtv);
        }
        if schedule.is_empty() {
            schedule.push(ScanType::Fm); // Fallback
        }

        Scanner {
            mode,
            interval_seconds: config.interval_seconds,
            schedule,
            schedule_index: 0,
            fm_frequencies: config.fm.frequencies.clone(),
            lte_frequencies: config.lte.frequencies.clone(),
            dtv_frequencies: config.dtv.frequencies.clone(),
            fm_config: config.fm.clone(),
            lte_config: config.lte.clone(),
            dtv_config: config.dtv.clone(),
            dbm_offset,
            discovered: false,
        }
    }

    /// Run first-time auto-discovery sweep. Updates frequency lists in memory.
    pub fn discover_stations(&mut self, sdr_manager: &mut SdrManager) -> DiscoveryResult {
        info!("Starting auto-discovery sweep");
        let mut result = DiscoveryResult {
            fm_stations: Vec::new(),
            lte_cells: Vec::new(),
            dtv_channels: Vec::new(),
        };

        let device = match sdr_manager.device_mut(0) {
            Some(d) => d,
            None => {
                warn!("No SDR device available for discovery sweep");
                return result;
            }
        };

        // FM sweep: 87.5-108 MHz
        info!("Sweeping FM band {}-{} MHz",
            self.fm_config.sweep_start / 1_000_000,
            self.fm_config.sweep_end / 1_000_000);
        let mut freq = self.fm_config.sweep_start;
        while freq <= self.fm_config.sweep_end {
            if let Ok(()) = device.set_center_freq(freq) {
                if let Ok(iq) = device.capture_raw(CAPTURE_BYTES) {
                    let (dbfs, _dbm) = calculate_signal_power(&iq, self.dbm_offset);
                    if dbfs > self.fm_config.sweep_threshold as f64 {
                        info!(freq_mhz = freq as f64 / 1e6, dbfs = format!("{:.1}", dbfs), "FM station found");
                        result.fm_stations.push(freq);
                    }
                }
            }
            freq += self.fm_config.sweep_step;
        }

        // LTE sweep: configured band ranges
        for band in &self.lte_config.sweep_bands {
            info!("Sweeping LTE band {}-{} MHz", band.start / 1_000_000, band.end / 1_000_000);
            let mut freq = band.start;
            while freq <= band.end {
                if let Ok(()) = device.set_center_freq(freq) {
                    if let Ok(iq) = device.capture_raw(CAPTURE_BYTES) {
                        let detections = super::lte::detect_lte_cells(&iq, device.sample_rate());
                        if !detections.is_empty() {
                            info!(freq_mhz = freq as f64 / 1e6, cells = detections.len(), "LTE cells detected");
                            result.lte_cells.push(freq);
                        }
                    }
                }
                freq += band.step;
            }
        }

        // DTV sweep: UHF channels
        info!("Sweeping DTV band {}-{} MHz",
            self.dtv_config.sweep_start / 1_000_000,
            self.dtv_config.sweep_end / 1_000_000);
        let mut freq = self.dtv_config.sweep_start;
        while freq <= self.dtv_config.sweep_end {
            // Tune to place pilot in capture window
            let pilot_tune = freq + 309_441; // Pilot offset from channel lower edge
            if let Ok(()) = device.set_center_freq(pilot_tune) {
                if let Ok(iq) = device.capture_raw(CAPTURE_BYTES) {
                    if super::dtv::extract_atsc_pilot(&iq, device.sample_rate(), pilot_tune).is_some() {
                        info!(freq_mhz = freq as f64 / 1e6, "DTV channel found");
                        result.dtv_channels.push(freq);
                    }
                }
            }
            freq += self.dtv_config.sweep_step;
        }

        // Merge discovered frequencies with config frequencies
        for f in &result.fm_stations {
            if !self.fm_frequencies.contains(f) {
                self.fm_frequencies.push(*f);
            }
        }
        for f in &result.lte_cells {
            if !self.lte_frequencies.contains(f) {
                self.lte_frequencies.push(*f);
            }
        }
        for f in &result.dtv_channels {
            if !self.dtv_frequencies.contains(f) {
                self.dtv_frequencies.push(*f);
            }
        }

        self.discovered = true;
        info!(
            fm = self.fm_frequencies.len(),
            lte = self.lte_frequencies.len(),
            dtv = self.dtv_frequencies.len(),
            "Discovery complete"
        );
        result
    }

    /// Run one full scan cycle. Returns observations to submit.
    pub fn run_cycle(&mut self, sdr_manager: &mut SdrManager) -> Vec<Observation> {
        let mut observations = Vec::new();

        if sdr_manager.device_count() == 0 {
            return observations;
        }

        // Multi-device: assign different scan types to different devices
        let device_count = sdr_manager.device_count();
        let scans_this_cycle: Vec<ScanType> = if device_count >= self.schedule.len() {
            // More devices than schedule entries — run full schedule in one pass
            self.schedule.clone()
        } else {
            // Fewer devices — pick next N from weighted schedule
            let mut scans = Vec::new();
            for _ in 0..device_count {
                scans.push(self.schedule[self.schedule_index % self.schedule.len()]);
                self.schedule_index += 1;
            }
            scans
        };

        for (dev_idx, scan_type) in scans_this_cycle.iter().enumerate() {
            let actual_dev = dev_idx % device_count;
            let freqs: &[u64] = match scan_type {
                ScanType::Fm => &self.fm_frequencies,
                ScanType::Lte => &self.lte_frequencies,
                ScanType::Dtv => &self.dtv_frequencies,
            };

            if freqs.is_empty() {
                continue;
            }

            for &freq in freqs {
                let results = self.scan_frequency(sdr_manager, actual_dev, *scan_type, freq);
                for result in results {
                    observations.push(scan_result_to_observation(&result, freq));
                }
            }
        }

        observations
    }

    fn scan_frequency(
        &self,
        sdr_manager: &mut SdrManager,
        device_idx: usize,
        scan_type: ScanType,
        freq: u64,
    ) -> Vec<ScanResult> {
        let device = match sdr_manager.device_mut(device_idx) {
            Some(d) => d,
            None => return Vec::new(),
        };

        // Tune to frequency
        let tune_freq = match scan_type {
            ScanType::Dtv => freq + 309_441, // Place ATSC pilot near center
            _ => freq,
        };

        if let Err(e) = device.set_center_freq(tune_freq) {
            warn!(freq, error = %e, "Failed to tune device");
            return Vec::new();
        }

        // Capture IQ data
        let iq_bytes = match device.capture_raw(CAPTURE_BYTES) {
            Ok(b) if b.len() >= 4 => b,
            Ok(_) => return Vec::new(),
            Err(e) => {
                warn!(freq, error = %e, "Capture failed");
                return Vec::new();
            }
        };

        let (_dbfs, dbm) = calculate_signal_power(&iq_bytes, self.dbm_offset);
        let mut results = Vec::new();

        match scan_type {
            ScanType::Fm => {
                // FM demodulation + RDS
                let mut raw_data = serde_json::Map::new();
                let mut snr: Option<f32> = None;
                let mut source_id: Option<String> = None;

                if let Some(demod) = super::fm_demod::demodulate_fm(&iq_bytes, device.sample_rate()) {
                    snr = Some(demod.snr_db);
                    raw_data.insert("snr_db".to_string(), serde_json::json!(demod.snr_db));
                    raw_data.insert("signal_power_dbfs".to_string(), serde_json::json!(demod.signal_power_dbfs));

                    // Try RDS extraction
                    if let Some(rds) = super::rds::extract_rds(&demod.audio_samples, device.sample_rate() / 10) {
                        source_id = Some(format!("pi_{:04X}", rds.pi_code));
                        raw_data.insert("pi_code".to_string(), serde_json::json!(rds.pi_code));
                        if let Some(ref cs) = rds.callsign {
                            raw_data.insert("callsign".to_string(), serde_json::json!(cs));
                        }
                        if let Some(pty) = rds.program_type {
                            raw_data.insert("program_type".to_string(), serde_json::json!(pty));
                        }
                        if let Some(ref ps) = rds.program_service {
                            raw_data.insert("program_service".to_string(), serde_json::json!(ps));
                        }
                    }
                }

                results.push(ScanResult {
                    signal_type: "fm_rds".to_string(),
                    frequency_hz: freq,
                    signal_strength_dbm: dbm as f32,
                    snr_db: snr,
                    source_id,
                    raw_data: if raw_data.is_empty() {
                        None
                    } else {
                        Some(serde_json::Value::Object(raw_data))
                    },
                });
            }

            ScanType::Lte => {
                let detections = super::lte::detect_lte_cells(&iq_bytes, device.sample_rate());
                for det in detections {
                    let mut raw_data = serde_json::Map::new();
                    raw_data.insert("nid1".to_string(), serde_json::json!(det.nid1));
                    raw_data.insert("nid2".to_string(), serde_json::json!(det.nid2));
                    raw_data.insert("timing_offset".to_string(), serde_json::json!(det.timing_offset_samples));
                    raw_data.insert("correlation".to_string(), serde_json::json!(det.correlation_strength));
                    raw_data.insert("freq_offset_hz".to_string(), serde_json::json!(det.frequency_offset_hz));

                    results.push(ScanResult {
                        signal_type: "lte_pss".to_string(),
                        frequency_hz: freq,
                        signal_strength_dbm: dbm as f32,
                        snr_db: Some(det.correlation_strength * 30.0), // Rough SNR estimate from correlation
                        source_id: Some(format!("pci_{}", det.physical_cell_id)),
                        raw_data: Some(serde_json::Value::Object(raw_data)),
                    });
                }

                // If no LTE detections, still report power measurement
                if results.is_empty() {
                    results.push(ScanResult {
                        signal_type: "lte_pss".to_string(),
                        frequency_hz: freq,
                        signal_strength_dbm: dbm as f32,
                        snr_db: None,
                        source_id: None,
                        raw_data: None,
                    });
                }
            }

            ScanType::Dtv => {
                if let Some(pilot) = super::dtv::extract_atsc_pilot(&iq_bytes, device.sample_rate(), tune_freq) {
                    let mut raw_data = serde_json::Map::new();
                    raw_data.insert("pilot_freq_hz".to_string(), serde_json::json!(pilot.pilot_frequency_hz));
                    raw_data.insert("pilot_snr_db".to_string(), serde_json::json!(pilot.pilot_snr_db));
                    raw_data.insert("freq_offset_hz".to_string(), serde_json::json!(pilot.frequency_offset_hz));
                    raw_data.insert("channel_power_dbfs".to_string(), serde_json::json!(pilot.channel_power_dbfs));

                    // Determine channel number from frequency
                    let channel = ((freq - 470_000_000) / 6_000_000) as u32 + 14;
                    results.push(ScanResult {
                        signal_type: "dtv_atsc".to_string(),
                        frequency_hz: freq,
                        signal_strength_dbm: dbm as f32,
                        snr_db: Some(pilot.pilot_snr_db),
                        source_id: Some(format!("atsc_ch{}", channel)),
                        raw_data: Some(serde_json::Value::Object(raw_data)),
                    });
                } else {
                    results.push(ScanResult {
                        signal_type: "dtv_atsc".to_string(),
                        frequency_hz: freq,
                        signal_strength_dbm: dbm as f32,
                        snr_db: None,
                        source_id: None,
                        raw_data: None,
                    });
                }
            }
        }

        debug!(
            scan_type = ?scan_type,
            freq_mhz = freq as f64 / 1e6,
            results = results.len(),
            "Scan complete"
        );

        results
    }

    pub fn needs_discovery(&self) -> bool {
        (self.mode == ScanMode::Sweep || self.mode == ScanMode::Both) && !self.discovered
    }

    pub fn interval_seconds(&self) -> u32 {
        self.interval_seconds
    }
}

/// Convert ScanResult to Observation, aligning types with observation.rs.
fn scan_result_to_observation(result: &ScanResult, freq: u64) -> Observation {
    let raw_data = result.raw_data.as_ref().and_then(|val| {
        if let serde_json::Value::Object(map) = val {
            let hm: HashMap<String, serde_json::Value> =
                map.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            if hm.is_empty() {
                None
            } else {
                Some(hm)
            }
        } else {
            None
        }
    });

    Observation {
        signal_type: result.signal_type.clone(),
        observed_at: Utc::now(),
        frequency_hz: Some(freq),
        timestamp_ns: None,
        tdoa_offset_ns: None,
        signal_strength_dbm: Some(result.signal_strength_dbm as f64),
        snr_db: result.snr_db.map(|s| s as f64),
        source_id: result.source_id.clone(),
        raw_data,
    }
}
