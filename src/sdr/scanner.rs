#![allow(dead_code)]

use crate::config::{DtvScanConfig, FmScanConfig, LteScanConfig, ScanningConfig};
use crate::observation::Observation;
use crate::sdr::{SdrDevice, SdrManager, calculate_signal_power};
use crate::sdr::recovery::{RecoveryAction, RecoveryManager, reset_usb_bus_device, restart_os_device};
use chrono::Utc;
use std::collections::HashMap;
use std::time::Duration;
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
    retune_delay_ms: u32,
    capture_retries: u32,
    max_observations_per_cycle: u32,
    recovery_manager: RecoveryManager,
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

        let mut scanner = Scanner {
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
            retune_delay_ms: config.retune_delay_ms,
            capture_retries: config.capture_retries,
            max_observations_per_cycle: config.max_observations_per_cycle,
            recovery_manager: RecoveryManager::new(),
        };
        scanner.fm_frequencies.sort_unstable();
        scanner.lte_frequencies.sort_unstable();
        scanner.dtv_frequencies.sort_unstable();
        scanner
    }

    /// Run first-time auto-discovery sweep. Updates frequency lists in memory.
    pub fn discover_stations(&mut self, sdr_manager: &mut SdrManager) -> DiscoveryResult {
        info!("Starting auto-discovery sweep");
        let mut result = DiscoveryResult {
            fm_stations: Vec::new(),
            lte_cells: Vec::new(),
            dtv_channels: Vec::new(),
        };

        if sdr_manager.device_count() == 0 {
            warn!("No SDR device available for discovery sweep");
            return result;
        }

        // FM sweep: 87.5-108 MHz
        info!("Sweeping FM band {}-{} MHz",
            self.fm_config.sweep_start / 1_000_000,
            self.fm_config.sweep_end / 1_000_000);
        let mut freq = self.fm_config.sweep_start;
        while freq <= self.fm_config.sweep_end {
            if self.tune_with_recovery(sdr_manager, 0, freq).is_some() {
                let device = sdr_manager.device_mut(0).unwrap();
                if let Ok(iq) = device.capture_raw(CAPTURE_BYTES) {
                    let (dbfs, _dbm) = calculate_signal_power(&iq, self.dbm_offset);
                    if dbfs > self.fm_config.sweep_threshold as f64 {
                        // SNR gate — verify real station, not noise spike
                        if let Some(demod) = super::fm_demod::demodulate_fm(&iq, device.sample_rate()) {
                            if demod.snr_db >= self.fm_config.sweep_min_snr_db {
                                info!(freq_mhz = freq as f64 / 1e6, dbfs = format!("{:.1}", dbfs), snr = format!("{:.1}", demod.snr_db), "FM station found");
                                result.fm_stations.push(freq);
                            } else {
                                debug!(freq_mhz = freq as f64 / 1e6, snr = format!("{:.1}", demod.snr_db), "Power OK but SNR too low");
                            }
                        }
                    }
                }
            }
            freq += self.fm_config.sweep_step;
        }

        // LTE sweep: configured band ranges
        let lte_bands = self.lte_config.sweep_bands.clone();
        for band in lte_bands {
            info!("Sweeping LTE band {}-{} MHz", band.start / 1_000_000, band.end / 1_000_000);
            let mut freq = band.start;
            while freq <= band.end {
                if self.tune_with_recovery(sdr_manager, 0, freq).is_some() {
                let device = sdr_manager.device_mut(0).unwrap();
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
            if self.tune_with_recovery(sdr_manager, 0, pilot_tune).is_some() {
                let device = sdr_manager.device_mut(0).unwrap();
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

        self.fm_frequencies.sort_unstable();
        self.lte_frequencies.sort_unstable();
        self.dtv_frequencies.sort_unstable();

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

        // Prime USB pipes — device state goes stale during async sleep between cycles.
        // A throwaway capture re-establishes the USB data stream.
        let dc = sdr_manager.device_count();
        for i in 0..dc {
            if let Some(dev) = sdr_manager.device(i) {
                if let Err(e) = dev.capture_raw(512) {
                    warn!(device = i, error = %e, "USB pipe prime failed");
                }
            }
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
            let freqs: Vec<u64> = match scan_type {
                ScanType::Fm => self.fm_frequencies.clone(),
                ScanType::Lte => self.lte_frequencies.clone(),
                ScanType::Dtv => self.dtv_frequencies.clone(),
            };

            if freqs.is_empty() {
                continue;
            }

            for freq in freqs {
                let results = self.scan_frequency(sdr_manager, actual_dev, *scan_type, freq);
                for result in results {
                    observations.push(scan_result_to_observation(&result, freq));
                }
            }
        }

        // Cap observations per cycle — strongest signals first
        let cap = self.max_observations_per_cycle as usize;
        if cap > 0 && observations.len() > cap {
            observations.sort_by(|a, b| {
                let sa = a.signal_strength_dbm.unwrap_or(f64::MIN);
                let sb = b.signal_strength_dbm.unwrap_or(f64::MIN);
                sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
            });
            info!(collected = observations.len(), cap, "Capping observations");
            observations.truncate(cap);
        }

        observations
    }

    fn scan_frequency(
        &mut self,
        sdr_manager: &mut SdrManager,
        device_idx: usize,
        scan_type: ScanType,
        freq: u64,
    ) -> Vec<ScanResult> {
        // Tune to frequency — handles pipe errors and band-aware delay internally
        let tune_freq = match scan_type {
            ScanType::Dtv => freq + 309_441, // Place ATSC pilot near center
            _ => freq,
        };

        if self.tune_with_recovery(sdr_manager, device_idx, tune_freq).is_none() {
            return Vec::new();
        }

        let sample_rate = {
            let device = match sdr_manager.device_mut(device_idx) {
                Some(d) => d,
                None => return Vec::new(),
            };
            device.sample_rate()
        };

        // Capture IQ data with retry and auto-reset on persistent pipe errors
        let iq_bytes = match self.capture_with_auto_reset(sdr_manager, device_idx, freq) {
            Some(b) if b.len() >= 4 => b,
            _ => return Vec::new(),
        };

        let (_dbfs, dbm) = calculate_signal_power(&iq_bytes, self.dbm_offset);
        let mut results = Vec::new();

        match scan_type {
            ScanType::Fm => {
                // FM demodulation + RDS
                let mut raw_data = serde_json::Map::new();
                let mut snr: Option<f32> = None;
                let mut source_id: Option<String> = None;

                if let Some(demod) = super::fm_demod::demodulate_fm(&iq_bytes, sample_rate) {
                    snr = Some(demod.snr_db);
                    raw_data.insert("snr_db".to_string(), serde_json::json!(demod.snr_db));
                    raw_data.insert("signal_power_dbfs".to_string(), serde_json::json!(demod.signal_power_dbfs));

                    // Try RDS extraction
                    if let Some(rds) = super::rds::extract_rds(&demod.audio_samples, sample_rate / 10) {
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
                let detections = super::lte::detect_lte_cells(&iq_bytes, sample_rate);
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
                if let Some(pilot) = super::dtv::extract_atsc_pilot(&iq_bytes, sample_rate, tune_freq) {
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

    /// PLL settle delay. Large cross-band jumps (> 100 MHz) use 250 ms;
    /// otherwise use the configured retune_delay_ms.
    fn retune_delay(&self, device: &SdrDevice, freq: u64) -> Duration {
        const LARGE_JUMP_HZ: u64 = 100_000_000;
        const LARGE_JUMP_DELAY_MS: u64 = 250;

        match device.last_tuned_freq_hz() {
            Some(prev) if freq.abs_diff(prev) > LARGE_JUMP_HZ => {
                Duration::from_millis(LARGE_JUMP_DELAY_MS)
            }
            _ => Duration::from_millis(self.retune_delay_ms as u64),
        }
    }

    /// Tune a device, applying band-aware delay and recovering from pipe errors
    /// using the 4-level escalation ladder.
    fn tune_with_recovery(
        &mut self,
        sdr_manager: &mut SdrManager,
        device_idx: usize,
        freq: u64,
    ) -> Option<()> {
        // Compute and apply delay based on *previous* tuned frequency, before we update it.
        let delay = {
            let device = sdr_manager.device_mut(device_idx)?;
            self.retune_delay(device, freq)
        };
        if delay > Duration::ZERO {
            std::thread::sleep(delay);
        }

        let result = {
            let device = sdr_manager.device_mut(device_idx)?;
            device.set_center_freq(freq)
        };

        match result {
            Ok(()) => {
                if let Some(device) = sdr_manager.device_mut(device_idx) {
                    device.set_last_tuned_freq_hz(freq);
                }
                Some(())
            }
            Err(e) => {
                if !is_pipe_error(&e) {
                    warn!(device = device_idx, freq, error = %e, "Failed to tune device");
                    return None;
                }

                let mut action = self.recovery_manager.record_pipe_error()?;
                warn!(
                    device = device_idx,
                    freq,
                    ?action,
                    "Pipe error during tune; executing recovery action"
                );

                loop {
                    match self.execute_recovery_action(sdr_manager, device_idx, action) {
                        Ok(()) => {
                            let device = sdr_manager.device_mut(device_idx)?;
                            match device.set_center_freq(freq) {
                                Ok(()) => {
                                    device.set_last_tuned_freq_hz(freq);
                                    self.recovery_manager.record_recovery_success();
                                    info!(device = device_idx, freq, "Tune succeeded after recovery");
                                    return Some(());
                                }
                                Err(e2) => {
                                    if is_pipe_error(&e2) {
                                        warn!(
                                            device = device_idx,
                                            freq,
                                            error = %e2,
                                            "Tune still failing with pipe error after recovery; escalating"
                                        );
                                        match self.recovery_manager.record_recovery_failure(action) {
                                            Some(next_action) => action = next_action,
                                            None => return None,
                                        }
                                    } else {
                                        warn!(
                                            device = device_idx,
                                            freq,
                                            error = %e2,
                                            "Tune failed after recovery"
                                        );
                                        return None;
                                    }
                                }
                            }
                        }
                        Err(err) => {
                            warn!(
                                device = device_idx,
                                freq,
                                error = %err,
                                ?action,
                                "Recovery action failed"
                            );
                            match self.recovery_manager.record_recovery_failure(action) {
                                Some(next_action) => action = next_action,
                                None => return None,
                            }
                        }
                    }
                }
            }
        }
    }

    fn execute_recovery_action(
        &mut self,
        sdr_manager: &mut SdrManager,
        device_idx: usize,
        action: RecoveryAction,
    ) -> anyhow::Result<()> {
        match action {
            RecoveryAction::ReopenHandle => {
                info!(device = device_idx, "Executing Level 1 recovery: reopen handle");
                sdr_manager.reopen_device(device_idx)?;
                std::thread::sleep(Duration::from_millis(500));
                Ok(())
            }
            RecoveryAction::UsbBusReset => {
                info!(device = device_idx, "Executing Level 2 recovery: USB bus reset");
                reset_usb_bus_device(device_idx)?;
                std::thread::sleep(Duration::from_millis(500));
                sdr_manager.reopen_device(device_idx)?;
                Ok(())
            }
            RecoveryAction::OsDeviceRestart => {
                info!(device = device_idx, "Executing Level 3 recovery: OS device restart");
                restart_os_device(self.recovery_manager.os_device_id(), device_idx)?;
                std::thread::sleep(Duration::from_millis(2000));
                sdr_manager.reopen_device(device_idx)?;
                Ok(())
            }
            RecoveryAction::ProcessExit(code) => {
                warn!(exit_code = code, "Executing Level 4 recovery: process exit");
                std::process::exit(code)
            }
        }
    }
}

/// Capture IQ data with exponential-backoff retry on USB errors.
fn capture_with_retry(
    device: &SdrDevice,
    num_bytes: usize,
    max_retries: u32,
) -> anyhow::Result<Vec<u8>> {
    let mut last_err = None;
    for attempt in 0..=max_retries {
        match device.capture_raw(num_bytes) {
            Ok(data) => return Ok(data),
            Err(e) => {
                if attempt < max_retries {
                    let delay_ms = 200 * 2u64.pow(attempt);
                    warn!(
                        attempt = attempt + 1,
                        max = max_retries + 1,
                        delay_ms,
                        error = %e,
                        "Capture failed, retrying"
                    );
                    std::thread::sleep(Duration::from_millis(delay_ms));
                }
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap())
}

/// Detect whether an error is a USB transport error (pipe stall or I/O failure).
/// Both `Pipe` and `Io` are USB-level failures that the recovery ladder should handle.
fn is_pipe_error(err: &anyhow::Error) -> bool {
    if let Some(rtl_err) = err.downcast_ref::<rtl_sdr_rs::error::RtlsdrError>() {
        matches!(
            rtl_err,
            rtl_sdr_rs::error::RtlsdrError::Usb(rusb::Error::Pipe)
                | rtl_sdr_rs::error::RtlsdrError::Usb(rusb::Error::Io)
        )
    } else if let Some(usb_err) = err.downcast_ref::<rusb::Error>() {
        matches!(usb_err, rusb::Error::Pipe | rusb::Error::Io)
    } else {
        false
    }
}

impl Scanner {
    /// Capture IQ data, retrying on transient errors and escalating through the
    /// recovery ladder on persistent USB pipe errors.
    fn capture_with_auto_reset(
        &mut self,
        sdr_manager: &mut SdrManager,
        device_idx: usize,
        freq: u64,
    ) -> Option<Vec<u8>> {
        let result = {
            let device = sdr_manager.device_mut(device_idx)?;
            capture_with_retry(device, CAPTURE_BYTES, self.capture_retries)
        };

        match result {
            Ok(buf) if buf.len() >= 4 => Some(buf),
            Ok(_) => None,
            Err(e) => {
                if !is_pipe_error(&e) {
                    warn!(device = device_idx, freq, error = %e, "Capture failed after retries");
                    return None;
                }

                let mut action = self.recovery_manager.record_pipe_error()?;
                warn!(
                    device = device_idx,
                    freq,
                    ?action,
                    "Pipe error during capture; executing recovery action"
                );

                loop {
                    match self.execute_recovery_action(sdr_manager, device_idx, action) {
                        Ok(()) => {
                            let device = sdr_manager.device_mut(device_idx)?;
                            match capture_with_retry(device, CAPTURE_BYTES, self.capture_retries) {
                                Ok(buf) if buf.len() >= 4 => {
                                    self.recovery_manager.record_recovery_success();
                                    info!(device = device_idx, freq, "Capture succeeded after recovery");
                                    return Some(buf);
                                }
                                Ok(_) => {
                                    self.recovery_manager.record_recovery_success();
                                    return None;
                                }
                                Err(e2) => {
                                    if is_pipe_error(&e2) {
                                        warn!(
                                            device = device_idx,
                                            freq,
                                            error = %e2,
                                            "Capture still failing with pipe error after recovery; escalating"
                                        );
                                        match self.recovery_manager.record_recovery_failure(action) {
                                            Some(next_action) => action = next_action,
                                            None => return None,
                                        }
                                    } else {
                                        warn!(
                                            device = device_idx,
                                            freq,
                                            error = %e2,
                                            "Capture failed after recovery"
                                        );
                                        return None;
                                    }
                                }
                            }
                        }
                        Err(err) => {
                            warn!(
                                device = device_idx,
                                freq,
                                error = %err,
                                ?action,
                                "Recovery action failed"
                            );
                            match self.recovery_manager.record_recovery_failure(action) {
                                Some(next_action) => action = next_action,
                                None => return None,
                            }
                        }
                    }
                }
            }
        }
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
