use crate::config::{DtvScanConfig, FmScanConfig, LteScanConfig, ScanningConfig};
use crate::observation::Observation;
use crate::sdr::{SdrDevice, SdrManager, calculate_signal_power};
use crate::sdr::recovery::{RecoveryAction, RecoveryManager, reset_usb_bus_device, restart_os_device};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};
use super::dtv::dvb;
use super::fm_demod::survey::{evaluate_adaptive, plan_captures, update_hold_streaks, CapturePlan, SurveyConfig};

/// Result from a single scan operation.
pub struct ScanResult {
    pub signal_type: String,
    #[allow(dead_code)] // set by every scan path; consumers use the freq passed alongside
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

/// Progress and outcome of the most recent sweep of one band.
///
/// `evaluated == total` for every completed sweep: a channel whose tune or
/// capture failed (after retries) is still counted as evaluated and ALSO in
/// `tune_failed`, so `evaluated - tune_failed` is the number actually measured.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct BandSweep {
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub evaluated: u32,
    pub total: u32,
    pub found: u32,
    pub tune_failed: u32,
    /// FM only: candidates dropped as a mirror image or adjacent-channel spill.
    pub spill_rejected: u32,
    pub in_progress: bool,
    pub next_rediscovery_at: Option<DateTime<Utc>>,
}

/// Per-band sweep status exported to /status (`sweep`).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SweepReport {
    pub fm: BandSweep,
    pub lte: BandSweep,
    pub dtv: BandSweep,
}

#[derive(Debug, Clone, Copy)]
enum BandKind {
    Fm,
    Lte,
    Dtv,
}

/// How the DTV band is scanned. ATSC keeps the original pilot-tone path.
#[derive(Debug, Clone, PartialEq)]
pub enum DtvPlan {
    Atsc,
    /// DVB-T/T2 cyclic-prefix detection on a channel-centre raster.
    Dvb { bandwidth_mhz: u32, first_channel: u32 },
}

impl DtvPlan {
    pub fn is_dvb(&self) -> bool {
        matches!(self, DtvPlan::Dvb { .. })
    }

    /// Route on the region's DTV standard: any "DVB-T*" name selects the DVB
    /// detector; "" and "ATSC" (and ISDB-T/DTMB, as before) keep the legacy
    /// pilot path. `channel_mhz` / `first_channel` of 0 are derived from the
    /// sweep step / the 470 MHz and 526 MHz rasters.
    pub fn from_config(cfg: &DtvScanConfig) -> Self {
        if !cfg.standard.starts_with("DVB") {
            return DtvPlan::Atsc;
        }
        let bw = if cfg.channel_mhz > 0 { cfg.channel_mhz } else { (cfg.sweep_step / 1_000_000) as u32 };
        if !matches!(bw, 6..=8) {
            return DtvPlan::Atsc;
        }
        let first_channel = if cfg.first_channel > 0 {
            cfg.first_channel
        } else {
            match bw {
                8 => 21 + (cfg.sweep_start.saturating_sub(470_000_000) / 8_000_000) as u32,
                7 => 28 + (cfg.sweep_start.saturating_sub(526_000_000) / 7_000_000) as u32,
                _ => 14 + (cfg.sweep_start.saturating_sub(470_000_000) / 6_000_000) as u32,
            }
        };
        DtvPlan::Dvb { bandwidth_mhz: bw, first_channel }
    }
}

/// Channel-centre raster for a DVB plan: `sweep_start` is the lower edge of the
/// first channel, `sweep_end` the upper edge of the last. Returns (channel
/// index, centre Hz).
pub fn dvb_channel_centres(sweep_start: u64, sweep_end: u64, bandwidth_mhz: u32) -> Vec<(u32, u64)> {
    let bw = u64::from(bandwidth_mhz) * 1_000_000;
    if bw == 0 || sweep_end < sweep_start {
        return Vec::new();
    }
    (0u64..)
        .take_while(|k| sweep_start + (k + 1) * bw <= sweep_end)
        .map(|k| (k as u32, sweep_start + k * bw + bw / 2))
        .collect()
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
    dtv_plan: DtvPlan,
    sweep: SweepReport,
    /// Live copy of `sweep` for other tasks (/status), updated during sweeps.
    sweep_shared: Arc<Mutex<SweepReport>>,
    /// Running count of tunes that returned failure (see `tune_with_recovery`).
    tune_fail_count: u32,
    /// Consecutive sweeps each FM station has been kept on power alone (`held`).
    fm_hold: HashMap<u64, u32>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum ScanType {
    Fm,
    Lte,
    Dtv,
}

/// A band sweep is abandoned after this many CONSECUTIVE failed tunes/captures.
/// `SdrDevice::set_center_freq` already runs the stall ladder (retry, USB reset +
/// re-init cycles) internally, so a failure here has survived all of that; on a
/// dead dongle more attempts only add minutes per channel. The unvisited channels
/// are counted in `tune_failed`, the sweep is marked complete, and the short-leash
/// rediscovery (`needs_discovery`) retries the band on a later cycle.
const MAX_CONSECUTIVE_TUNE_FAILURES: u32 = 3;

/// Default IQ capture size for signal processing (128K samples = 256KB).
const CAPTURE_BYTES: usize = 262_144;

/// Relaxed gates for the one-shot FM sweep retry when the primary pass (using
/// `fm_config.sweep_threshold` / `sweep_min_snr_db`) finds zero stations.
/// Retained only for the historical regression tests; discovery now uses the
/// channel-relative survey (`fm_survey_pass`) and has no relaxed second pass.
#[cfg(test)]
const FM_FALLBACK_THRESHOLD_DBFS: f32 = -35.0;
#[cfg(test)]
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
#[cfg(test)]
const FM_FALLBACK_MIN_STATIONS: usize = 3;

/// Whether the FM discovery sweep should re-run with relaxed gates.
///
/// Takes the threshold as a parameter rather than reading the constant, so the
/// constant stays independently assertable and a future `fm.fallback_min_stations`
/// config field can feed this without touching the predicate.
#[cfg(test)]
fn should_run_fm_fallback(primary_result: &[u64], min_stations: usize) -> bool {
    primary_result.len() < min_stations
}

/// Union two frequency lists, preserving sort order and dropping duplicates.
///
/// The relaxed pass re-walks the same deterministic grid, so it normally
/// re-finds everything the strict pass found — but it is not *guaranteed* to:
/// a fade, an AGC excursion or a capture error on one step can drop a frequency
/// the strict pass caught. Assigning instead of merging would silently lose it.
#[cfg(test)]
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
            dtv_plan: DtvPlan::from_config(&config.dtv),
            sweep: SweepReport::default(),
            sweep_shared: Arc::new(Mutex::new(SweepReport::default())),
            tune_fail_count: 0,
            fm_hold: HashMap::new(),
        };
        scanner.fm_frequencies.sort_unstable();
        scanner.lte_frequencies.sort_unstable();
        scanner.dtv_frequencies.sort_unstable();
        scanner
    }

    /// Replace the scan plan (frequency lists, sweep ranges, DTV routing,
    /// schedule) from a freshly applied configuration. Clears the discovered
    /// state so the next `needs_discovery()` is true and discovery runs
    /// immediately with the new plan. No hardware access.
    #[allow(dead_code)] // called by DAEMON-CORE on config apply
    pub fn apply_plan(&mut self, plan: &ScanningConfig) {
        let fresh = Scanner::new(plan, self.dbm_offset);
        self.mode = fresh.mode;
        self.interval_seconds = fresh.interval_seconds;
        self.schedule = fresh.schedule;
        self.schedule_index = 0;
        self.fm_frequencies = fresh.fm_frequencies;
        self.lte_frequencies = fresh.lte_frequencies;
        self.dtv_frequencies = fresh.dtv_frequencies;
        self.fm_config = fresh.fm_config;
        self.lte_config = fresh.lte_config;
        self.dtv_config = fresh.dtv_config;
        self.discovered = false;
        self.last_discovery = None;
        self.rediscovery_interval = fresh.rediscovery_interval;
        self.rediscovery_min_fm = fresh.rediscovery_min_fm;
        self.retune_delay_ms = fresh.retune_delay_ms;
        self.capture_retries = fresh.capture_retries;
        self.max_observations_per_cycle = fresh.max_observations_per_cycle;
        self.dtv_plan = fresh.dtv_plan;
        self.sweep = SweepReport::default();
        self.fm_hold.clear();
        self.publish_sweep();
        info!(
            fm_range_mhz = format!("{}-{}", self.fm_config.sweep_start / 1_000_000, self.fm_config.sweep_end / 1_000_000),
            dtv_plan = ?self.dtv_plan,
            "Scan plan applied; discovery scheduled"
        );
    }

    /// Shared handle to the live sweep report; updated at the start and end of
    /// every band sweep (and on `apply_plan`), so `in_progress` is visible while a
    /// sweep runs.
    #[allow(dead_code)] // consumed by DAEMON-CORE for /status
    pub fn sweep_handle(&self) -> Arc<Mutex<SweepReport>> {
        Arc::clone(&self.sweep_shared)
    }

    fn publish_sweep(&self) {
        let snapshot = self.sweep_report();
        if let Ok(mut g) = self.sweep_shared.lock() {
            *g = snapshot;
        }
    }

    /// Snapshot of per-band sweep progress for /status.
    pub fn sweep_report(&self) -> SweepReport {
        let mut r = self.sweep.clone();
        if let Some(wait) = self.time_until_rediscovery() {
            let at = Utc::now() + chrono::Duration::from_std(wait).unwrap_or_else(|_| chrono::Duration::zero());
            r.fm.next_rediscovery_at = Some(at);
            r.lte.next_rediscovery_at = Some(at);
            r.dtv.next_rediscovery_at = Some(at);
        }
        r
    }

    /// Time until the next repeat discovery sweep, or `None` if none is scheduled
    /// (config-only mode, rediscovery disabled, or not yet discovered).
    fn time_until_rediscovery(&self) -> Option<Duration> {
        if !(self.mode == ScanMode::Sweep || self.mode == ScanMode::Both) {
            return None;
        }
        let elapsed = self.last_discovery?.elapsed();
        let mut due: Option<Duration> = None;
        if !self.rediscovery_interval.is_zero() {
            due = Some(self.rediscovery_interval.saturating_sub(elapsed));
        }
        let low_yield = (self.rediscovery_min_fm > 0 && self.fm_frequencies.len() < self.rediscovery_min_fm)
            || self.sweep.fm.tune_failed > 0;
        if low_yield {
            let d = LOW_YIELD_RETRY_INTERVAL.saturating_sub(elapsed);
            due = Some(due.map_or(d, |x| x.min(d)));
        }
        due
    }

    fn begin_band(&mut self, kind: BandKind, total: u32) {
        let b = BandSweep { started_at: Some(Utc::now()), total, in_progress: true, ..Default::default() };
        match kind {
            BandKind::Fm => self.sweep.fm = b,
            BandKind::Lte => self.sweep.lte = b,
            BandKind::Dtv => self.sweep.dtv = b,
        }
        self.publish_sweep();
    }

    /// Record progress while a sweep is running: `n` more channels evaluated,
    /// `found` the running total. Published immediately so /status shows live progress.
    fn advance_band(&mut self, kind: BandKind, n: u32, found: u32) {
        let band = match kind {
            BandKind::Fm => &mut self.sweep.fm,
            BandKind::Lte => &mut self.sweep.lte,
            BandKind::Dtv => &mut self.sweep.dtv,
        };
        band.evaluated = (band.evaluated + n).min(band.total);
        band.found = found;
        self.publish_sweep();
    }

    /// Complete a band sweep. The sweep loop has run to the end here, so every
    /// channel has been evaluated (`evaluated == total`); channels that could
    /// not be measured are in `tune_failed`.
    fn finish_band(&mut self, kind: BandKind, found: u32, tune_failed: u32) {
        let band = match kind {
            BandKind::Fm => &mut self.sweep.fm,
            BandKind::Lte => &mut self.sweep.lte,
            BandKind::Dtv => &mut self.sweep.dtv,
        };
        band.evaluated = band.total;
        band.found = found;
        band.tune_failed = tune_failed.min(band.total);
        band.in_progress = false;
        band.completed_at = Some(Utc::now());
        self.publish_sweep();
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

        // FM sweep: channel-relative survey of the configured/regional range.
        info!("Sweeping FM band {}-{} MHz",
            self.fm_config.sweep_start / 1_000_000,
            self.fm_config.sweep_end / 1_000_000);
        result.fm_stations = self.fm_survey_pass(sdr_manager);

        // LTE sweep: configured band ranges
        let lte_bands = self.lte_config.sweep_bands.clone();
        let lte_total: u32 = lte_bands
            .iter()
            .map(|b| if b.step == 0 || b.end < b.start { 0 } else { ((b.end - b.start) / b.step + 1) as u32 })
            .sum();
        self.begin_band(BandKind::Lte, lte_total);
        let lte_fail_before = self.tune_fail_count;
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

        let lte_found = result.lte_cells.len() as u32;
        let lte_failed = self.tune_fail_count - lte_fail_before;
        self.finish_band(BandKind::Lte, lte_found, lte_failed);

        // DTV sweep: UHF channels
        let dtv_dvb = self.dtv_plan.is_dvb();
        if dtv_dvb {
            self.dvb_sweep(sdr_manager, &mut result);
        } else {
            let steps = if self.dtv_config.sweep_step == 0 || self.dtv_config.sweep_end < self.dtv_config.sweep_start {
                0
            } else {
                ((self.dtv_config.sweep_end - self.dtv_config.sweep_start) / self.dtv_config.sweep_step + 1) as u32
            };
            self.begin_band(BandKind::Dtv, steps);
        }
        let dtv_fail_before = self.tune_fail_count;
        info!("Sweeping DTV band {}-{} MHz",
            self.dtv_config.sweep_start / 1_000_000,
            self.dtv_config.sweep_end / 1_000_000);
        let mut freq = self.dtv_config.sweep_start;
        while !dtv_dvb && freq <= self.dtv_config.sweep_end {
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

        if !dtv_dvb {
            let dtv_failed = self.tune_fail_count - dtv_fail_before;
            self.finish_band(BandKind::Dtv, result.dtv_channels.len() as u32, dtv_failed);
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

    /// Tune + capture + survey one FM capture plan. `None` when the tune or the
    /// capture failed (the caller retries and counts).
    fn survey_capture(&mut self, sdr_manager: &mut SdrManager, plan: &CapturePlan, sample_rate: u32, cfg: &SurveyConfig) -> Option<Vec<crate::sdr::fm_demod::survey::FmChannel>> {
        self.tune_with_recovery(sdr_manager, 0, plan.centre_hz)?;
        let iq = self.capture_with_auto_reset(sdr_manager, 0, plan.centre_hz)?;
        // Borderline channels get extra captures of the same tune; the decision is
        // made on the median evidence so ordinary noise/fading does not flip it.
        let centre = plan.centre_hz;
        let r = {
            let mut more = || self.capture_with_auto_reset(sdr_manager, 0, centre);
            evaluate_adaptive(&iq, sample_rate, centre, &plan.channels, cfg, &mut more)
        };
        if r.is_empty() { None } else { Some(r) }
    }

    /// Channel-relative FM survey (see `fm_demod::survey`): every channel of the
    /// raster is evaluated every sweep, one tune per ~8 channels, no station
    /// cap. A capture whose tune/capture fails is retried once after the first
    /// pass; channels still unresolved are counted in `tune_failed`.
    fn fm_survey_pass(&mut self, sdr_manager: &mut SdrManager) -> Vec<u64> {
        let sample_rate = sdr_manager.device(0).map(|d| d.sample_rate()).unwrap_or(2_048_000);
        let plans = plan_captures(self.fm_config.sweep_start, self.fm_config.sweep_end, self.fm_config.sweep_step, sample_rate);
        // Hysteresis: stations known from earlier sweeps (or configured) keep lowered gates.
        let cfg = SurveyConfig { channel_step_hz: self.fm_config.sweep_step as f64, known: self.fm_frequencies.clone(), held_streak: self.fm_hold.iter().map(|(k, v)| (*k, *v)).collect(), ..SurveyConfig::default() };
        self.fm_survey_run(plans, |sc, plan| sc.survey_capture(sdr_manager, plan, sample_rate, &cfg))
    }

    /// The sweep loop behind `fm_survey_pass`, with the capture step injected so
    /// it can be exercised without hardware. `evaluated` advances (and is
    /// published) after every capture plan, so progress is live.
    fn fm_survey_run<F>(&mut self, plans: Vec<CapturePlan>, mut capture: F) -> Vec<u64>
    where
        F: FnMut(&mut Scanner, &CapturePlan) -> Option<Vec<crate::sdr::fm_demod::survey::FmChannel>>,
    {
        let total: u32 = plans.iter().map(|p| p.channels.len() as u32).sum();
        let hold_min = SurveyConfig::default().hold_min_excess_db;
        self.begin_band(BandKind::Fm, total);
        let mut found: Vec<u64> = Vec::new();
        let mut retry: Vec<CapturePlan> = Vec::new();
        let (mut stations_images, mut weak_rejected) = (0usize, 0usize);
        let mut consecutive = 0u32;
        let mut unresolved = 0u32;
        let mut aborted = false;
        for (idx, plan) in plans.iter().enumerate() {
            match capture(self, plan) {
                Some(r) => {
                    consecutive = 0;
                    update_hold_streaks(&mut self.fm_hold, &r, hold_min);
                    Self::record_fm(&r, &mut found, &mut stations_images, &mut weak_rejected);
                }
                None => {
                    consecutive += 1;
                    retry.push(plan.clone());
                }
            }
            self.advance_band(BandKind::Fm, plan.channels.len() as u32, found.len() as u32);
            if consecutive >= MAX_CONSECUTIVE_TUNE_FAILURES {
                aborted = true;
                let skipped: u32 = plans[idx + 1..].iter().map(|p| p.channels.len() as u32).sum();
                unresolved += skipped;
                warn!(consecutive, skipped_channels = skipped, "FM band abandoned after consecutive tune failures; the next cycle retries");
                break;
            }
        }
        let retried = if aborted { 0 } else { retry.len() };
        if aborted {
            unresolved += retry.iter().map(|p| p.channels.len() as u32).sum::<u32>();
        } else {
            consecutive = 0;
            for (i, plan) in retry.iter().enumerate() {
                warn!(centre_mhz = plan.centre_hz as f64 / 1e6, "FM capture failed; retrying once");
                match capture(self, plan) {
                    Some(r) => {
                        consecutive = 0;
                        update_hold_streaks(&mut self.fm_hold, &r, hold_min);
                        Self::record_fm(&r, &mut found, &mut stations_images, &mut weak_rejected);
                    }
                    None => {
                        consecutive += 1;
                        unresolved += plan.channels.len() as u32;
                        warn!(centre_mhz = plan.centre_hz as f64 / 1e6, channels = plan.channels.len(), "FM capture failed after retry; channels counted as tune_failed");
                    }
                }
                self.advance_band(BandKind::Fm, 0, found.len() as u32);
                if consecutive >= MAX_CONSECUTIVE_TUNE_FAILURES {
                    unresolved += retry[i + 1..].iter().map(|p| p.channels.len() as u32).sum::<u32>();
                    warn!(consecutive, "FM retry pass abandoned after consecutive tune failures");
                    break;
                }
            }
        }
        found.sort_unstable();
        found.dedup();
        info!(
            channels = total,
            captures = plans.len(),
            retried,
            tune_failed = unresolved,
            image_or_spill_rejected = stations_images,
            below_gates = weak_rejected,
            found = found.len(),
            "FM survey complete"
        );
        self.sweep.fm.spill_rejected = stations_images as u32;
        self.finish_band(BandKind::Fm, found.len() as u32, unresolved);
        found
    }

    fn record_fm(r: &[crate::sdr::fm_demod::survey::FmChannel], found: &mut Vec<u64>, images: &mut usize, rejected: &mut usize) {
        for c in r {
            // One line per evaluated channel so a sweep can be audited against a
            // reference survey (grep "FM channel evaluated").
            debug!(
                freq_mhz = c.freq_hz as f64 / 1e6,
                excess_db = format!("{:.1}", c.excess_db),
                quality_db = format!("{:.1}", c.quality_db),
                pilot_db = ?c.pilot_db.map(|p| (p * 10.0).round() / 10.0),
                rds_pi = ?c.rds_pi,
                marginal = c.marginal,
                corroborated = c.corroborated,
                held = c.held,
                image = c.image_of_mirror,
                spill = c.spill_of_neighbour,
                decision = if c.confirmed { "station" } else { "no" },
                "FM channel evaluated"
            );
            if c.confirmed {
                info!(freq_mhz = c.freq_hz as f64 / 1e6, excess_db = format!("{:.1}", c.excess_db), quality_db = format!("{:.1}", c.quality_db), pilot_db = ?c.pilot_db.map(|p| (p * 10.0).round() / 10.0), marginal = c.marginal, held = c.held, "FM station found");
                found.push(c.freq_hz);
            } else if c.image_of_mirror || c.spill_of_neighbour {
                *images += 1;
            } else {
                *rejected += 1;
            }
        }
    }

    /// DVB-T/T2 discovery: every channel centre of the region raster, detection only.
    fn dvb_sweep(&mut self, sdr_manager: &mut SdrManager, result: &mut DiscoveryResult) {
        let DtvPlan::Dvb { bandwidth_mhz, .. } = self.dtv_plan.clone() else { return };
        let centres = dvb_channel_centres(self.dtv_config.sweep_start, self.dtv_config.sweep_end, bandwidth_mhz);
        self.dvb_sweep_run(&centres, bandwidth_mhz, result, |sc, centre| {
            // One attempt per channel: core's set_center_freq already retries and resets.
            sc.tune_with_recovery(sdr_manager, 0, centre)?;
            let sample_rate = sdr_manager.device(0).map(|d| d.sample_rate()).unwrap_or(2_048_000);
            sc.capture_with_auto_reset(sdr_manager, 0, centre).map(|iq| (iq, sample_rate))
        });
    }

    /// The DVB discovery loop with the capture step injected (testable without a
    /// device). Abandons the band after `MAX_CONSECUTIVE_TUNE_FAILURES` consecutive
    /// failures, counting every unvisited channel as `tune_failed`.
    fn dvb_sweep_run<F>(&mut self, centres: &[(u32, u64)], bandwidth_mhz: u32, result: &mut DiscoveryResult, mut capture: F)
    where
        F: FnMut(&mut Scanner, u64) -> Option<(Vec<u8>, u32)>,
    {
        self.begin_band(BandKind::Dtv, centres.len() as u32);
        let mut failed = 0u32;
        let mut consecutive = 0u32;
        for (idx, &(_, centre)) in centres.iter().enumerate() {
            match capture(self, centre) {
                Some((iq, sample_rate)) => {
                    consecutive = 0;
                    if let Some(det) = dvb::detect_dvb(&iq, sample_rate, bandwidth_mhz) {
                        info!(freq_mhz = centre as f64 / 1e6, mode = det.fft_mode, gi = det.guard_interval, rho = format!("{:.2}", det.rho), contrast = format!("{:.0}", det.contrast), "DVB channel found");
                        result.dtv_channels.push(centre);
                    }
                }
                None => {
                    failed += 1;
                    consecutive += 1;
                    debug!(freq_mhz = centre as f64 / 1e6, "DVB channel skipped: tune/capture failed");
                }
            }
            self.advance_band(BandKind::Dtv, 1, result.dtv_channels.len() as u32);
            if consecutive >= MAX_CONSECUTIVE_TUNE_FAILURES {
                let skipped = (centres.len() - idx - 1) as u32;
                failed += skipped;
                warn!(consecutive, skipped_channels = skipped, "DVB band abandoned after consecutive tune failures; the next cycle retries");
                break;
            }
        }
        let found = result.dtv_channels.len() as u32;
        self.finish_band(BandKind::Dtv, found, failed);
    }

    /// Build the detection-only observation for a DVB capture, or nothing.
    fn scan_dvb_capture(&self, iq_bytes: &[u8], sample_rate: u32, freq: u64, dbm: f64) -> Vec<ScanResult> {
        let DtvPlan::Dvb { bandwidth_mhz, first_channel } = self.dtv_plan.clone() else { return Vec::new() };
        let Some(det) = dvb::detect_dvb(iq_bytes, sample_rate, bandwidth_mhz) else { return Vec::new() };
        let bw_hz = u64::from(bandwidth_mhz) * 1_000_000;
        let k = (freq.saturating_sub(self.dtv_config.sweep_start) / bw_hz) as u32;
        let channel = first_channel + k;
        let mut raw = serde_json::Map::new();
        raw.insert("fft_mode".to_string(), serde_json::json!(det.fft_mode));
        raw.insert("guard_interval".to_string(), serde_json::json!(det.guard_interval));
        raw.insert("rho".to_string(), serde_json::json!(det.rho));
        raw.insert("contrast".to_string(), serde_json::json!(det.contrast));
        raw.insert("cfo_hz".to_string(), serde_json::json!(det.cfo_hz));
        raw.insert("channel".to_string(), serde_json::json!(channel));
        raw.insert("bandwidth_mhz".to_string(), serde_json::json!(bandwidth_mhz));
        // `t2_possible` is true for every DVB-T2 result (definitely T2) and for a
        // `dtv_dvbt` reading that could also be T2 (2K/8K with GI 1/4..1/32).
        raw.insert("t2_possible".to_string(), serde_json::json!(det.t2_possible));
        // In-band SNR estimate from the CP correlation: rho ~= SNR / (1 + SNR).
        let rho = det.rho.clamp(0.01, 0.999);
        let snr_db = (10.0 * (rho / (1.0 - rho)).log10()) as f32;
        vec![ScanResult {
            signal_type: det.signal_type.to_string(),
            frequency_hz: freq,
            signal_strength_dbm: dbm as f32,
            snr_db: Some(snr_db),
            source_id: Some(format!("dvb_ch{}", channel)),
            raw_data: Some(serde_json::Value::Object(raw)),
        }]
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
        let dvb_plan = self.dtv_plan.is_dvb();
        let tune_freq = match scan_type {
            ScanType::Dtv if !dvb_plan => freq + 309_441, // Place ATSC pilot near center
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
        if scan_type == ScanType::Dtv && dvb_plan {
            return self.scan_dvb_capture(&iq_bytes, sample_rate, freq, dbm);
        }
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
        // A sweep that could not evaluate every FM channel is retried on the short
        // leash rather than waiting for the full interval.
        if self.sweep.fm.tune_failed > 0
            && self.last_discovery.is_some_and(|t| t.elapsed() >= LOW_YIELD_RETRY_INTERVAL)
        {
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
        let r = self.tune_with_recovery_inner(sdr_manager, device_idx, freq);
        if r.is_none() {
            self.tune_fail_count += 1;
        }
        r
    }

    fn tune_with_recovery_inner(
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
                            // Another program holds the dongle: escalating to a
                            // bus reset or PnP restart would fight it for the device.
                            if crate::sdr::dongle::is_in_use_anyhow(&err) {
                                return None;
                            }
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
                crate::sdr::dongle::guarded_usb_reset(|| reset_usb_bus_device(device_idx))?;
                std::thread::sleep(Duration::from_millis(500));
                if let Some((pos, config, _serial)) = closed {
                    sdr_manager.reopen_device_at(device_idx, pos, &config)?;
                }
                Ok(())
            }
            RecoveryAction::OsDeviceRestart => {
                info!(device = device_idx, "Executing Level 3 recovery: OS device restart");
                // A PnP restart would drop another running program's session too.
                if crate::sdr::holders::other_holder_running() {
                    return Err(anyhow::Error::new(crate::sdr::dongle::in_use_error()));
                }
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

#[cfg(test)]
mod dsp_plan_tests {
    use super::*;
    use crate::sdr::dtv::dvb::{FftMode, GuardInterval};
    use crate::sdr::dtv::testgen::{dvb_capture, negative_capture, Negative};

    fn dvb_cfg(start: u64, end: u64, step: u64) -> ScanningConfig {
        let mut c = ScanningConfig::default();
        if step != 6_000_000 {
            c.dtv.standard = "DVB-T2-694".to_string();
            c.dtv.channel_mhz = (step / 1_000_000) as u32;
        }
        c.dtv.sweep_start = start;
        c.dtv.sweep_end = end;
        c.dtv.sweep_step = step;
        c
    }

    #[test]
    fn dvb_raster_is_channel_centres_not_edges() {
        let ch = dvb_channel_centres(470_000_000, 694_000_000, 8);
        assert_eq!(ch.len(), 28);
        assert_eq!(ch[0], (0, 474_000_000));
        assert_eq!(ch[27], (27, 690_000_000));
        // Channel 37 (602 MHz) and channel 47 (682 MHz) as reported from the field: first_channel 21.
        assert!(ch.contains(&(16, 602_000_000)));
        assert!(ch.contains(&(26, 682_000_000)));
        let au = dvb_channel_centres(526_000_000, 694_000_000, 7);
        assert_eq!(au.len(), 24);
        assert_eq!(au[0].1, 529_500_000);
        assert_eq!(au[23].1, 690_500_000);
        // Partial last channel is not tuned.
        assert_eq!(dvb_channel_centres(470_000_000, 700_000_000, 8).len(), 28);
        assert!(dvb_channel_centres(470_000_000, 470_000_000, 8).is_empty());
    }

    #[test]
    fn dtv_routing_keeps_atsc_defaults_on_the_pilot_path() {
        assert_eq!(DtvPlan::from_config(&ScanningConfig::default().dtv), DtvPlan::Atsc);
        assert_eq!(DtvPlan::from_config(&dvb_cfg(470_000_000, 608_000_000, 6_000_000).dtv), DtvPlan::Atsc);
        let mut atsc = ScanningConfig::default();
        atsc.dtv.standard = "ATSC".to_string();
        atsc.dtv.sweep_step = 8_000_000; // a stray step never routes an ATSC plan to DVB
        assert_eq!(DtvPlan::from_config(&atsc.dtv), DtvPlan::Atsc);
        let mut tw = ScanningConfig::default();
        tw.dtv.standard = "DVB-T-6".to_string();
        tw.dtv.channel_mhz = 6;
        tw.dtv.first_channel = 14;
        assert_eq!(DtvPlan::from_config(&tw.dtv), DtvPlan::Dvb { bandwidth_mhz: 6, first_channel: 14 });
        assert_eq!(DtvPlan::from_config(&dvb_cfg(470_000_000, 694_000_000, 8_000_000).dtv), DtvPlan::Dvb { bandwidth_mhz: 8, first_channel: 21 });
        assert_eq!(DtvPlan::from_config(&dvb_cfg(526_000_000, 694_000_000, 7_000_000).dtv), DtvPlan::Dvb { bandwidth_mhz: 7, first_channel: 28 });
    }

    #[test]
    fn apply_plan_replaces_lists_routing_and_schedules_discovery() {
        let mut scanner = Scanner::new(&ScanningConfig::default(), -30.0);
        scanner.discovered = true;
        scanner.last_discovery = Some(Instant::now());
        assert!(!scanner.needs_discovery());
        assert!(!scanner.dtv_plan.is_dvb());

        let mut plan = dvb_cfg(470_000_000, 694_000_000, 8_000_000);
        plan.fm.sweep_start = 76_000_000;
        plan.fm.sweep_end = 95_000_000;
        plan.fm.frequencies = vec![80_000_000];
        scanner.apply_plan(&plan);
        assert!(scanner.needs_discovery(), "a new plan must trigger an immediate discovery");
        assert!(scanner.dtv_plan.is_dvb());
        assert_eq!(scanner.fm_frequencies, vec![80_000_000]);
        assert_eq!(scanner.fm_config.sweep_start, 76_000_000);
        assert_eq!(scanner.dtv_config.sweep_end, 694_000_000);
        assert_eq!(scanner.sweep_report(), SweepReport::default());

        // Back to ATSC.
        scanner.apply_plan(&ScanningConfig::default());
        assert!(!scanner.dtv_plan.is_dvb());
    }

    #[test]
    fn dead_dongle_bounds_attempts_per_band() {
        use crate::sdr::fm_demod::survey::plan_captures;
        // FM: a device whose every tune fails. Without the bound this is 13 first-pass
        // attempts + 13 retries; with it, 3 consecutive failures end the band.
        let mut scanner = Scanner::new(&ScanningConfig::default(), -30.0);
        let h = scanner.sweep_handle();
        let plans = plan_captures(87_500_000, 108_000_000, 200_000, 2_048_000);
        let mut calls = 0;
        let found = scanner.fm_survey_run(plans, |_sc, _plan| {
            calls += 1;
            None
        });
        assert!(found.is_empty());
        assert_eq!(calls, MAX_CONSECUTIVE_TUNE_FAILURES, "attempts on a dead dongle");
        let r = h.lock().unwrap().fm.clone();
        assert!(!r.in_progress && r.completed_at.is_some());
        assert_eq!((r.evaluated, r.total, r.tune_failed), (103, 103, 103));

        // DVB: one attempt per channel, then abandon after 3 consecutive failures.
        let mut scanner = Scanner::new(&dvb_cfg(470_000_000, 694_000_000, 8_000_000), -30.0);
        let h = scanner.sweep_handle();
        let centres = dvb_channel_centres(470_000_000, 694_000_000, 8);
        let mut result = DiscoveryResult { fm_stations: vec![], lte_cells: vec![], dtv_channels: vec![] };
        let mut calls = 0;
        scanner.dvb_sweep_run(&centres, 8, &mut result, |_sc, _centre| {
            calls += 1;
            None
        });
        assert_eq!(calls, MAX_CONSECUTIVE_TUNE_FAILURES);
        let r = h.lock().unwrap().dtv.clone();
        assert!(!r.in_progress);
        assert_eq!((r.evaluated, r.total, r.tune_failed, r.found), (28, 28, 28, 0));
    }

    #[test]
    fn intermittent_failures_do_not_abandon_the_band() {
        use crate::sdr::fm_demod::survey::plan_captures;
        let mut scanner = Scanner::new(&ScanningConfig::default(), -30.0);
        let plans = plan_captures(87_500_000, 108_000_000, 200_000, 2_048_000);
        let mut calls = 0;
        // Every 3rd call fails (never 3 in a row), including the retries.
        scanner.fm_survey_run(plans, |_sc, plan| {
            calls += 1;
            if calls % 3 == 0 {
                None
            } else {
                Some(plan.channels.iter().map(|&f| crate::sdr::fm_demod::survey::FmChannel {
                    freq_hz: f, power_dbfs: -40.0, excess_db: 0.0, quality_db: 0.0, pilot_db: None, rds_pi: None,
                    image_of_mirror: false, spill_of_neighbour: false, image_prone: false, marginal: false, corroborated: false, held: false, confirmed: false,
                }).collect())
            }
        });
        let r = scanner.sweep_report().fm;
        assert_eq!(r.evaluated, 103);
        assert!(calls > 13, "all plans visited, plus retries");
    }

    #[test]
    fn fm_survey_progress_is_live_and_exact() {
        use crate::sdr::fm_demod::survey::{plan_captures, FmChannel};
        let mut scanner = Scanner::new(&ScanningConfig::default(), -30.0);
        let h = scanner.sweep_handle();
        let plans = plan_captures(87_500_000, 108_000_000, 200_000, 2_048_000);
        let total: u32 = plans.iter().map(|p| p.channels.len() as u32).sum();
        assert_eq!(total, 103);
        let mut seen: Vec<(u32, bool)> = Vec::new();
        let mut calls = 0;
        let found = scanner.fm_survey_run(plans, |_sc, plan| {
            {
                let g = h.lock().unwrap();
                seen.push((g.fm.evaluated, g.fm.in_progress));
            }
            calls += 1;
            // The 4th capture plan fails on the first pass (call 4) and on its retry
            // (call 14, after the 13 first-pass calls); the rest yield one station each.
            if calls == 4 || calls == 14 {
                return None;
            }
            Some(plan.channels.iter().map(|&f| FmChannel {
                freq_hz: f, power_dbfs: -40.0, excess_db: 20.0, quality_db: 30.0, pilot_db: None, rds_pi: None,
                image_of_mirror: false, spill_of_neighbour: false, image_prone: false, marginal: false, corroborated: false, held: false, confirmed: f == plan.channels[0],
            }).collect())
        });
        // Mid-sweep: strictly between 0 and total at least once, monotonic, always in progress.
        assert!(seen.iter().any(|(e, _)| *e > 0 && *e < total), "{seen:?}");
        assert!(seen.windows(2).all(|w| w[0].0 <= w[1].0));
        assert!(seen.iter().all(|(_, ip)| *ip));
        let r = h.lock().unwrap().clone();
        assert!(!r.fm.in_progress);
        assert_eq!(r.fm.evaluated, r.fm.total);
        assert_eq!(r.fm.total, 103);
        assert_eq!(calls, 14);
        assert_eq!(r.fm.found as usize, found.len());
        assert_eq!(r.fm.tune_failed, 8, "one 8-channel capture plan failed after its retry");
    }

    #[test]
    fn sweep_handle_shows_in_progress_live() {
        let mut scanner = Scanner::new(&ScanningConfig::default(), -30.0);
        let h = scanner.sweep_handle();
        scanner.begin_band(BandKind::Fm, 103);
        assert!(h.lock().unwrap().fm.in_progress);
        scanner.finish_band(BandKind::Fm, 5, 0);
        let r = h.lock().unwrap().clone();
        assert!(!r.fm.in_progress && r.fm.evaluated == 103);
    }

    #[test]
    fn sweep_report_serialises_with_the_contract_fields() {
        let mut scanner = Scanner::new(&ScanningConfig::default(), -30.0);
        scanner.begin_band(BandKind::Fm, 103);
        let mid = serde_json::to_value(scanner.sweep_report()).unwrap();
        assert_eq!(mid["fm"]["in_progress"], true);
        assert_eq!(mid["fm"]["total"], 103);
        scanner.finish_band(BandKind::Fm, 17, 0);
        let v = serde_json::to_value(scanner.sweep_report()).unwrap();
        for band in ["fm", "lte", "dtv"] {
            for key in ["started_at", "completed_at", "evaluated", "total", "found", "tune_failed", "spill_rejected", "in_progress", "next_rediscovery_at"] {
                assert!(v[band].get(key).is_some(), "{band}.{key}");
            }
        }
        assert_eq!(v["fm"]["evaluated"], v["fm"]["total"]);
        assert_eq!(v["fm"]["found"], 17);
        assert_eq!(v["fm"]["in_progress"], false);
    }

    #[test]
    fn failed_fm_sweep_is_retried_on_the_short_leash_not_the_full_interval() {
        let mut scanner = Scanner::new(&ScanningConfig::default(), -30.0);
        scanner.discovered = true;
        scanner.last_discovery = Some(Instant::now() - Duration::from_secs(1000));
        scanner.fm_frequencies = vec![1, 2, 3, 4, 5];
        assert!(!scanner.needs_discovery());
        scanner.begin_band(BandKind::Fm, 103);
        scanner.finish_band(BandKind::Fm, 5, 8);
        assert!(scanner.needs_discovery());
    }

    #[test]
    fn dvb_capture_becomes_detection_only_observation() {
        let scanner = {
            let mut s = Scanner::new(&dvb_cfg(470_000_000, 694_000_000, 8_000_000), -30.0);
            s.dtv_plan = DtvPlan::from_config(&s.dtv_config);
            s
        };
        let iq = dvb_capture(11, FftMode::K8, GuardInterval::G1_8, 8, 12.0, 60_000.0, false);
        let r = scanner.scan_dvb_capture(&iq, 2_048_000, 602_000_000, -40.0);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].signal_type, "dtv_dvbt");
        assert_eq!(r[0].source_id.as_deref(), Some("dvb_ch37"));
        assert_eq!(r[0].frequency_hz, 602_000_000);
        let raw = r[0].raw_data.as_ref().unwrap();
        for key in ["fft_mode", "guard_interval", "rho", "contrast", "cfo_hz", "channel", "bandwidth_mhz", "t2_possible"] {
            assert!(raw.get(key).is_some(), "raw_data.{key}");
        }
        assert_eq!(raw["fft_mode"], "8K");
        assert_eq!(raw["guard_interval"], "1/8");
        assert_eq!(raw["channel"], 37);
        assert_eq!(raw["bandwidth_mhz"], 8);
        assert_eq!(raw["t2_possible"], true);

        let t2 = dvb_capture(12, FftMode::K32, GuardInterval::G19_128, 8, 12.0, -120_000.0, false);
        let r2 = scanner.scan_dvb_capture(&t2, 2_048_000, 682_000_000, -40.0);
        assert_eq!(r2[0].signal_type, "dtv_dvbt2");
        assert_eq!(r2[0].source_id.as_deref(), Some("dvb_ch47"));
        assert_eq!(r2[0].raw_data.as_ref().unwrap()["t2_possible"], true);
    }

    #[test]
    fn nothing_detected_means_nothing_emitted_and_atsc_plan_never_uses_the_dvb_path() {
        let dvb = {
            let mut s = Scanner::new(&dvb_cfg(470_000_000, 694_000_000, 8_000_000), -30.0);
            s.dtv_plan = DtvPlan::from_config(&s.dtv_config);
            s
        };
        for kind in [Negative::Awgn, Negative::Cw, Negative::Fm, Negative::Atsc] {
            let iq = negative_capture(5, kind);
            assert!(dvb.scan_dvb_capture(&iq, 2_048_000, 602_000_000, -40.0).is_empty(), "{kind:?}");
        }
        let atsc = Scanner::new(&ScanningConfig::default(), -30.0);
        let iq = dvb_capture(13, FftMode::K8, GuardInterval::G1_8, 8, 12.0, 0.0, false);
        assert!(atsc.scan_dvb_capture(&iq, 2_048_000, 602_000_000, -40.0).is_empty());
    }
}

