#![allow(dead_code)]

use crate::config::{DtvScanConfig, FmScanConfig, LteScanConfig, ScanningConfig};
use crate::observation::Observation;
use crate::sdr::{SdrDevice, SdrManager, calculate_signal_power};
use crate::sdr::recovery::{RecoveryAction, RecoveryManager, reset_usb_bus_device, restart_os_device};
use chrono::Utc;
use std::collections::HashMap;
use std::time::{Duration, Instant};
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
    /// When the last discovery sweep finished. `None` until the first one runs.
    last_discovery: Option<Instant>,
    rediscovery_interval: Duration,
    rediscovery_min_fm: usize,
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

/// Relaxed gates for the one-shot FM sweep retry when the primary pass (using
/// `fm_config.sweep_threshold` / `sweep_min_snr_db`) finds zero stations.
/// See `fm_sweep_pass` / `should_run_fm_fallback`.
const FM_FALLBACK_THRESHOLD_DBFS: f32 = -35.0;
const FM_FALLBACK_MIN_SNR_DB: f32 = 5.0;

/// Minimum station count a primary FM sweep must clear before its gates are
/// trusted. Below this the sweep is read as "these gates are too tight for this
/// RF environment", not as a complete picture of the band.
///
/// 3, because the default grid is 87.5-108 MHz at 200 kHz = ~103 candidate
/// steps, and anywhere worth siting a node carries far more than three
/// receivable carriers (a thin rural market still runs 8-15). A node that
/// genuinely lives in an RF desert is exactly the case where one extra relaxed
/// pass costs a single sweep and can only add stations. 1 would leave the bug
/// half-fixed (2 found => still no retry, still nearly blind); 10 would
/// re-sweep on almost every node for no gain in a healthy environment.
const FM_FALLBACK_MIN_STATIONS: usize = 3;

/// Whether the FM discovery sweep should re-run with relaxed gates.
///
/// Takes the threshold as a parameter rather than reading the constant, so the
/// constant stays independently assertable and a future `fm.fallback_min_stations`
/// config field can feed this without touching the predicate.
fn should_run_fm_fallback(primary_result: &[u64], min_stations: usize) -> bool {
    primary_result.len() < min_stations
}

/// Union two frequency lists, preserving sort order and dropping duplicates.
///
/// The relaxed pass re-walks the same deterministic grid, so it normally
/// re-finds everything the strict pass found — but it is not *guaranteed* to:
/// a fade, an AGC excursion or a capture error on one step can drop a frequency
/// the strict pass caught. Assigning instead of merging would silently lose it.
fn merge_frequencies(mut a: Vec<u64>, b: Vec<u64>) -> Vec<u64> {
    a.extend(b);
    a.sort_unstable();
    a.dedup();
    a
}

/// Short leash for a node that came out of discovery nearly blind: retry sooner
/// than the full interval rather than waiting hours to find out the antenna was
/// unplugged during the one sweep that ever ran.
const LOW_YIELD_RETRY_INTERVAL: Duration = Duration::from_secs(900);

/// Whether a *repeat* discovery sweep is due. Pure, so it is testable without
/// hardware; the caller supplies elapsed time rather than reading a clock.
///
/// `discovered` being a one-shot latch is what pinned a node to whatever a
/// single sweep happened to find at process start — see `needs_discovery`.
fn should_rediscover(
    since_last: Option<Duration>,
    interval: Duration,
    fm_station_count: usize,
    min_fm_stations: usize,
) -> bool {
    let Some(elapsed) = since_last else {
        return true;
    };
    if min_fm_stations > 0
        && fm_station_count < min_fm_stations
        && elapsed >= LOW_YIELD_RETRY_INTERVAL
    {
        return true;
    }
    !interval.is_zero() && elapsed >= interval
}

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
            last_discovery: None,
            rediscovery_interval: Duration::from_secs(config.rediscovery_interval_secs),
            rediscovery_min_fm: config.rediscovery_min_fm_stations,
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

        // FM sweep: 87.5-108 MHz (or the configured/regional range)
        info!("Sweeping FM band {}-{} MHz",
            self.fm_config.sweep_start / 1_000_000,
            self.fm_config.sweep_end / 1_000_000);
        result.fm_stations = self.fm_sweep_pass(
            sdr_manager,
            self.fm_config.sweep_threshold,
            self.fm_config.sweep_min_snr_db,
        );
        if should_run_fm_fallback(&result.fm_stations, FM_FALLBACK_MIN_STATIONS) {
            // Too few stations at the configured gates. `discovered` is latched
            // true below regardless of outcome, and run_cycle's periodic FM
            // scan only iterates `fm_frequencies` — so a sweep that finds
            // almost nothing here pins this node to that handful of
            // frequencies until the next full discovery. Re-sweep once with
            // materially looser gates, to catch cases where the configured
            // threshold/SNR are simply too strict for this node's RF
            // environment (weak antenna feed, distant stations, etc.)
            // without silently leaving the node nearly blind.
            warn!(
                found = result.fm_stations.len(),
                min_stations = FM_FALLBACK_MIN_STATIONS,
                threshold_dbfs = FM_FALLBACK_THRESHOLD_DBFS,
                min_snr_db = FM_FALLBACK_MIN_SNR_DB,
                "FM sweep found too few stations at configured gates — retrying with relaxed gates"
            );
            let relaxed = self.fm_sweep_pass(
                sdr_manager,
                FM_FALLBACK_THRESHOLD_DBFS,
                FM_FALLBACK_MIN_SNR_DB,
            );
            let before = result.fm_stations.len();
            // MERGE, never assign: the relaxed pass is not guaranteed to
            // re-find every strict hit (see merge_frequencies).
            result.fm_stations = merge_frequencies(result.fm_stations, relaxed);
            info!(
                before,
                after = result.fm_stations.len(),
                "relaxed FM sweep merged into strict result"
            );
        }

        // LTE sweep: configured band ranges
        let lte_bands = self.lte_config.sweep_bands.clone();
        for band in lte_bands {
            info!("Sweeping LTE band {}-{} MHz", band.start / 1_000_000, band.end / 1_000_000);
            let mut freq = band.start;
            while freq <= band.end {
                if self.tune_with_recovery(sdr_manager, 0, freq).is_some() {
                    if let Some(device) = sdr_manager.device_mut(0) {
                        if let Ok(iq) = device.capture_raw(CAPTURE_BYTES) {
                            let detections = super::lte::detect_lte_cells(&iq, device.sample_rate());
                            if !detections.is_empty() {
                                info!(freq_mhz = freq as f64 / 1e6, cells = detections.len(), "LTE cells detected");
                                result.lte_cells.push(freq);
                            }
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
                if let Some(device) = sdr_manager.device_mut(0) {
                    if let Ok(iq) = device.capture_raw(CAPTURE_BYTES) {
                        if super::dtv::extract_atsc_pilot(&iq, device.sample_rate(), pilot_tune).is_some() {
                            info!(freq_mhz = freq as f64 / 1e6, "DTV channel found");
                            result.dtv_channels.push(freq);
                        }
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
        self.last_discovery = Some(Instant::now());
        info!(
            fm = self.fm_frequencies.len(),
            lte = self.lte_frequencies.len(),
            dtv = self.dtv_frequencies.len(),
            "Discovery complete"
        );
        result
    }

    /// One full FM sweep pass across the configured band using the given
    /// power/SNR gates. Returns the frequencies that passed both gates.
    /// Emits debug-level diagnostics for every candidate — dBFS, SNR (when
    /// demod succeeded), and which gate (power/demod/snr) rejected it, if
    /// any — so a user can self-diagnose from logs why a given frequency
    /// wasn't detected as a station.
    fn fm_sweep_pass(
        &mut self,
        sdr_manager: &mut SdrManager,
        threshold_dbfs: f32,
        min_snr_db: f32,
    ) -> Vec<u64> {
        let mut found = Vec::new();
        let mut candidates = 0usize;
        let mut tune_failed = 0usize;
        let mut capture_failed = 0usize;
        let mut rejected_power = 0usize;
        let mut rejected_snr = 0usize;
        let mut demod_failed = 0usize;

        let mut freq = self.fm_config.sweep_start;
        while freq <= self.fm_config.sweep_end {
            candidates += 1;
            if self.tune_with_recovery(sdr_manager, 0, freq).is_some() {
                if let Some(device) = sdr_manager.device_mut(0) {
                    match device.capture_raw(CAPTURE_BYTES) {
                        Ok(iq) => {
                            let (dbfs, _dbm) = calculate_signal_power(&iq, self.dbm_offset);
                            if dbfs > threshold_dbfs as f64 {
                                match super::fm_demod::demodulate_fm(&iq, device.sample_rate()) {
                                    Some(demod) if demod.snr_db >= min_snr_db => {
                                        info!(freq_mhz = freq as f64 / 1e6, dbfs = format!("{:.1}", dbfs), snr = format!("{:.1}", demod.snr_db), "FM station found");
                                        found.push(freq);
                                    }
                                    Some(demod) => {
                                        rejected_snr += 1;
                                        debug!(freq_mhz = freq as f64 / 1e6, dbfs = format!("{:.1}", dbfs), snr = format!("{:.1}", demod.snr_db), gate = "snr", "FM candidate rejected: SNR below threshold");
                                    }
                                    None => {
                                        demod_failed += 1;
                                        debug!(freq_mhz = freq as f64 / 1e6, dbfs = format!("{:.1}", dbfs), gate = "demod", "FM candidate rejected: demodulation failed (near-DC input or capture too short)");
                                    }
                                }
                            } else {
                                rejected_power += 1;
                                debug!(freq_mhz = freq as f64 / 1e6, dbfs = format!("{:.1}", dbfs), gate = "power", "FM candidate rejected: below power threshold");
                            }
                        }
                        Err(e) => {
                            // Previously silent: a failed capture skipped the
                            // frequency with no trace at any log level.
                            capture_failed += 1;
                            debug!(freq_mhz = freq as f64 / 1e6, gate = "capture", error = %e, "FM candidate skipped: capture failed");
                        }
                    }
                }
            } else {
                // Previously silent: tune_with_recovery returning None skipped
                // the frequency with no trace at any log level, so a sweep that
                // failed to tune 102 of 103 steps looked identical in the logs
                // to one where 102 steps were genuinely below threshold.
                tune_failed += 1;
                debug!(freq_mhz = freq as f64 / 1e6, gate = "tune", "FM candidate skipped: tune failed (recovery exhausted or device gone)");
            }
            freq += self.fm_config.sweep_step;
        }

        // Single info-level line that makes this sweep diagnosable without
        // debug logging: "103 candidates / 87 below power / 1 found" and
        // "103 candidates / 102 tune failures / 1 found" are entirely
        // different diagnoses and were previously indistinguishable.
        info!(
            threshold_dbfs,
            min_snr_db,
            candidates,
            tune_failed,
            capture_failed,
            rejected_power,
            rejected_snr,
            demod_failed,
            found = found.len(),
            "FM sweep pass complete"
        );

        found
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
        if !(self.mode == ScanMode::Sweep || self.mode == ScanMode::Both) {
            return false;
        }
        if !self.discovered {
            return true;
        }
        should_rediscover(
            self.last_discovery.map(|t| t.elapsed()),
            self.rediscovery_interval,
            self.fm_frequencies.len(),
            self.rediscovery_min_fm,
        )
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
        // Compute the delay from the *previous* tuned frequency BEFORE the tune
        // updates it — the jump size is a property of the transition, not of the
        // target. (Computing it after set_last_tuned_freq_hz would make
        // freq.abs_diff(prev) == 0 and silently delete the large-jump path.)
        let delay = {
            let device = sdr_manager.device_mut(device_idx)?;
            self.retune_delay(device, freq)
        };

        let result = {
            let device = sdr_manager.device_mut(device_idx)?;
            device.set_center_freq(freq)
        };

        match result {
            Ok(()) => {
                if let Some(device) = sdr_manager.device_mut(device_idx) {
                    device.set_last_tuned_freq_hz(freq);
                }
                // Settle AFTER the tune. The PLL needs this time to lock the NEW
                // frequency. Sleeping before set_center_freq waited on the
                // previous, already-locked frequency and left ZERO settle before
                // the caller's capture_raw() — so every post-retune capture read
                // a slewing PLL. That is the "only the strongest station is ever
                // found" symptom.
                if delay > Duration::ZERO {
                    std::thread::sleep(delay);
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
                                    // Same post-tune PLL settle as the happy path — a
                                    // capture straight after a recovery retune is just
                                    // as vulnerable to reading a slewing PLL.
                                    if delay > Duration::ZERO {
                                        std::thread::sleep(delay);
                                    }
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
                // Close rtl-sdr handle first — WinUSB rejects concurrent handles
                let closed = sdr_manager.close_device(device_idx);
                std::thread::sleep(Duration::from_millis(200));
                reset_usb_bus_device(device_idx)?;
                std::thread::sleep(Duration::from_millis(500));
                if let Some((pos, config, _serial)) = closed {
                    sdr_manager.reopen_device_at(device_idx, pos, &config)?;
                }
                Ok(())
            }
            RecoveryAction::OsDeviceRestart => {
                info!(device = device_idx, "Executing Level 3 recovery: OS device restart");
                // Close handle before OS-level device restart
                let closed = sdr_manager.close_device(device_idx);
                restart_os_device(self.recovery_manager.os_device_id(), device_idx)?;
                std::thread::sleep(Duration::from_millis(2000));
                if let Some((pos, config, _serial)) = closed {
                    sdr_manager.reopen_device_at(device_idx, pos, &config)?;
                }
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
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("capture failed with no recorded error")))
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

#[cfg(test)]
mod tests {
    use super::*;

    // `discover_stations`/`fm_sweep_pass` require a real RTL-SDR device
    // (SdrManager opens hardware directly), so the fallback DECISION logic
    // is factored into `should_run_fm_fallback` specifically so it can be
    // tested without hardware. This is the regression coverage for the
    // "FM sweep finds zero stations => permanently zero FM observations"
    // bug: `run_cycle` only iterates `fm_frequencies`, and `discovered` is
    // latched true after one discovery pass regardless of outcome, so a
    // sweep that finds nothing must retry with relaxed gates before that
    // latch closes.
    // CHANGED (BUG-5). The previous revision asserted that a single discovered
    // station suppresses the relaxed re-sweep:
    //
    //     #[test]
    //     fn fm_fallback_does_not_engage_when_primary_sweep_found_a_station() {
    //         assert!(!should_run_fm_fallback(&[100_300_000]));
    //     }
    //
    // That test was correct about the code and wrong about the requirement. The
    // rule it encoded — "a partial result means the gates are working for this
    // RF environment" — does not hold: `discovered` is latched true after one
    // pass and `run_cycle` only re-tunes the frozen `fm_frequencies`, so a sweep
    // that clears the strict gates on exactly one strong local carrier pinned
    // the node to a single FM frequency for the rest of the process lifetime.
    // That is the reported symptom (one station near the top of the band while
    // other users see 20+). One station out of the ~103 candidate steps in
    // 87.5-108 MHz is evidence the gates are too tight, not evidence they are
    // calibrated. The tests below pin the corrected contract.
    #[test]
    fn fm_fallback_engages_when_primary_sweep_found_nothing() {
        assert!(should_run_fm_fallback(&[], FM_FALLBACK_MIN_STATIONS));
    }

    #[test]
    fn fm_fallback_engages_when_primary_sweep_found_a_single_station() {
        assert!(should_run_fm_fallback(
            &[100_300_000],
            FM_FALLBACK_MIN_STATIONS
        ));
    }

    #[test]
    fn fm_fallback_engages_below_the_minimum_station_count() {
        assert!(should_run_fm_fallback(
            &[88_500_000, 101_100_000],
            FM_FALLBACK_MIN_STATIONS
        ));
    }

    #[test]
    fn fm_fallback_does_not_engage_with_multiple_stations_found() {
        assert!(!should_run_fm_fallback(
            &[88_500_000, 101_100_000, 107_900_000],
            FM_FALLBACK_MIN_STATIONS
        ));
        assert!(!should_run_fm_fallback(
            &[88_500_000, 95_500_000, 101_100_000, 107_900_000],
            FM_FALLBACK_MIN_STATIONS
        ));
    }

    #[test]
    fn fm_fallback_min_stations_is_a_plausible_floor() {
        // Guards a future edit that disables the fallback (0) or forces it on
        // every node (a value near the ~103 steps in the default grid).
        assert!((1..=5).contains(&FM_FALLBACK_MIN_STATIONS));
    }

    #[test]
    fn fm_fallback_gates_are_looser_than_defaults() {
        // FmScanConfig defaults (config.rs): sweep_threshold = -20.0 dBFS,
        // sweep_min_snr_db = 8.0. The fallback must be strictly looser on
        // both axes, or it wouldn't find anything the primary pass missed.
        assert!(FM_FALLBACK_THRESHOLD_DBFS < -20.0);
        assert!(FM_FALLBACK_MIN_SNR_DB < 8.0);
    }

    #[test]
    fn merge_frequencies_unions_without_duplicates() {
        let merged = merge_frequencies(
            vec![100_300_000, 88_500_000],
            vec![88_500_000, 95_700_000],
        );
        assert_eq!(merged, vec![88_500_000, 95_700_000, 100_300_000]);
    }

    #[test]
    fn merge_frequencies_never_loses_a_strict_result() {
        // The relaxed pass is not guaranteed to re-find every strict hit; the
        // strict result must survive regardless.
        let merged = merge_frequencies(vec![107_900_000], vec![88_100_000]);
        assert!(merged.contains(&107_900_000));
        assert!(merged.contains(&88_100_000));
    }

    #[test]
    fn merge_frequencies_handles_an_empty_relaxed_pass() {
        let merged = merge_frequencies(vec![98_100_000], vec![]);
        assert_eq!(merged, vec![98_100_000]);
    }

    // --- repeat discovery -------------------------------------------------

    const SIX_HOURS: Duration = Duration::from_secs(21_600);

    #[test]
    fn rediscovery_required_before_the_first_discovery() {
        assert!(should_rediscover(None, SIX_HOURS, 0, 3));
    }

    #[test]
    fn rediscovery_not_triggered_before_the_interval_with_a_healthy_list() {
        assert!(!should_rediscover(
            Some(Duration::from_secs(3_600)),
            SIX_HOURS,
            12,
            3
        ));
    }

    #[test]
    fn rediscovery_triggered_after_the_interval() {
        assert!(should_rediscover(
            Some(Duration::from_secs(21_601)),
            SIX_HOURS,
            12,
            3
        ));
    }

    #[test]
    fn low_station_count_triggers_early_rediscovery() {
        // The BUG-5 shape: one station found, so retry on the short leash
        // rather than waiting the full six hours.
        assert!(should_rediscover(
            Some(Duration::from_secs(900)),
            SIX_HOURS,
            1,
            3
        ));
    }

    #[test]
    fn low_station_count_still_respects_the_short_leash() {
        assert!(!should_rediscover(
            Some(Duration::from_secs(60)),
            SIX_HOURS,
            1,
            3
        ));
    }

    #[test]
    fn zero_interval_disables_periodic_rediscovery() {
        assert!(!should_rediscover(
            Some(Duration::from_secs(86_400)),
            Duration::ZERO,
            12,
            3
        ));
    }

    #[test]
    fn zero_min_stations_disables_the_low_yield_trigger() {
        assert!(!should_rediscover(
            Some(Duration::from_secs(1_000)),
            SIX_HOURS,
            0,
            0
        ));
    }

    #[test]
    fn fresh_scanner_needs_discovery_in_both_mode() {
        // Scanner::new touches no hardware.
        let scanner = Scanner::new(&ScanningConfig::default(), -30.0);
        assert!(scanner.needs_discovery());
    }

    #[test]
    fn config_mode_never_needs_discovery() {
        let cfg = ScanningConfig {
            mode: "config".to_string(),
            ..Default::default()
        };
        let scanner = Scanner::new(&cfg, -30.0);
        assert!(!scanner.needs_discovery());
    }
}
