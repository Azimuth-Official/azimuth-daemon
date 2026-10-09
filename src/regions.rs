use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegionDatabase {
    pub bands: BandDatabase,
    /// Keyed by standard name (ATSC, DVB-T2, ISDB-T, DTMB).
    pub dtv_standards: HashMap<String, DtvStandard>,
    /// Keyed by ISO country code; includes the "_DEFAULT" worldwide fallback.
    pub regions: HashMap<String, RegionConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BandDatabase {
    /// Keyed by 3GPP band number (the JSON object key).
    pub lte: HashMap<u32, LteBandSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LteBandSpec {
    pub dl_low: f64,
    pub dl_high: f64,
    #[serde(rename = "type")]
    pub band_type: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DtvStandard {
    pub low_mhz: u64,
    pub high_mhz: u64,
    pub channel_mhz: u64,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegionConfig {
    pub name: String,
    pub lte_bands: Vec<u32>,
    pub fm_low_mhz: f64,
    pub fm_high_mhz: f64,
    pub dtv_standard: String,
    #[serde(default)]
    pub _note: Option<String>,
}

pub struct ResolvedRegion {
    pub region_code: String,
    pub fm_sweep_start: u64,
    pub fm_sweep_end: u64,
    pub fm_sweep_step: u64,
    pub lte_sweep_bands: Vec<SweepBand>,
    pub dtv_sweep_start: u64,
    pub dtv_sweep_end: u64,
    pub dtv_sweep_step: u64,
    pub skipped_bands: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SweepBand {
    pub band_number: u32,
    pub start: u64,
    pub end: u64,
    pub step: u64,
}

pub struct RegionResolver {
    db: RegionDatabase,
    max_tuner_mhz: u64,
}

impl RegionResolver {
    /// Load region database from embedded data.
    /// max_tuner_mhz caps all frequencies returned (e.g., 1766 for RTL-SDR Blog V4).
    pub fn new(max_tuner_mhz: u64) -> anyhow::Result<Self> {
        let db_str = include_str!("../data/frequency-regions.json");
        let db: RegionDatabase = serde_json::from_str(db_str)?;
        Ok(Self { db, max_tuner_mhz })
    }

    /// Resolve a region code to a ResolvedRegion with tuner capability filter applied.
    /// Unknown codes fall back to _DEFAULT.
    pub fn resolve(&self, region_code: &str) -> anyhow::Result<ResolvedRegion> {
        let code_normalized = if region_code.is_empty() || region_code == "AUTO" {
            "_DEFAULT".to_string()
        } else {
            region_code.to_uppercase()
        };

        let (region, effective_code) = match self.db.regions.get(&code_normalized) {
            Some(r) => (r.clone(), code_normalized),
            None => {
                if region_code != "AUTO" && !region_code.is_empty() {
                    tracing::warn!(
                        region = region_code,
                        "unknown region code, falling back to _DEFAULT"
                    );
                }
                let default = self
                    .db
                    .regions
                    .get("_DEFAULT")
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("region database has no _DEFAULT entry"))?;
                (default, "_DEFAULT".to_string())
            }
        };

        let mut sweep_bands = Vec::new();
        let mut skipped_bands = Vec::new();
        let max_hz = self.max_tuner_mhz * 1_000_000;

        // Resolve LTE bands
        for band_num in &region.lte_bands {
            if let Some(band_spec) = self.db.bands.lte.get(band_num) {
                let dl_low_hz = (band_spec.dl_low * 1_000_000.0) as u64;
                let dl_high_hz = (band_spec.dl_high * 1_000_000.0) as u64;

                if dl_low_hz > max_hz {
                    skipped_bands.push(format!(
                        "Band {} ({:.0}-{:.0} MHz) exceeds tuner range ({} MHz) — skipped",
                        band_num,
                        band_spec.dl_low,
                        band_spec.dl_high,
                        self.max_tuner_mhz
                    ));
                    continue;
                }

                let clamped_end = dl_high_hz.min(max_hz);
                sweep_bands.push(SweepBand {
                    band_number: *band_num,
                    start: dl_low_hz,
                    end: clamped_end,
                    step: 5_000_000, // 5 MHz step per spec
                });
            }
        }

        // Resolve DTV standard
        let dtv_standard = self
            .db
            .dtv_standards
            .get(&region.dtv_standard)
            .cloned()
            .unwrap_or_else(|| {
                tracing::warn!(
                    standard = %region.dtv_standard,
                    "unknown DTV standard, falling back to DVB-T2"
                );
                self.db
                    .dtv_standards
                    .get("DVB-T2")
                    .cloned()
                    .unwrap_or(DtvStandard {
                        low_mhz: 470,
                        high_mhz: 700,
                        channel_mhz: 6,
                        name: "fallback UHF".to_string(),
                    })
            });

        let dtv_end = (dtv_standard.high_mhz * 1_000_000).min(max_hz);

        // Log skipped bands at resolve time
        for skipped_msg in &skipped_bands {
            tracing::warn!("{}", skipped_msg);
        }

        Ok(ResolvedRegion {
            region_code: effective_code,
            fm_sweep_start: (region.fm_low_mhz * 1_000_000.0) as u64,
            fm_sweep_end: (region.fm_high_mhz * 1_000_000.0) as u64,
            fm_sweep_step: 200_000, // 200 kHz step
            lte_sweep_bands: sweep_bands,
            dtv_sweep_start: dtv_standard.low_mhz * 1_000_000,
            dtv_sweep_end: dtv_end,
            dtv_sweep_step: dtv_standard.channel_mhz * 1_000_000,
            skipped_bands,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_us_region() {
        let resolver = RegionResolver::new(1766).expect("should load resolver");
        let resolved = resolver.resolve("US").expect("US region should resolve");

        assert_eq!(resolved.region_code, "US");
        assert!(!resolved.lte_sweep_bands.is_empty(), "US should have LTE bands");
        assert!(resolved.fm_sweep_start > 0, "FM sweep should be configured");
    }

    #[test]
    fn resolves_auto_to_default() {
        let resolver = RegionResolver::new(1766).expect("should load resolver");
        let resolved = resolver.resolve("AUTO").expect("AUTO should resolve");

        assert_eq!(resolved.region_code, "_DEFAULT");
        assert!(!resolved.lte_sweep_bands.is_empty());
    }

    #[test]
    fn unknown_region_falls_back_to_default() {
        let resolver = RegionResolver::new(1766).expect("should load resolver");
        let resolved = resolver.resolve("ZZ").expect("unknown region should resolve");

        assert_eq!(resolved.region_code, "_DEFAULT");
    }

    #[test]
    fn clamps_bands_above_tuner_limit() {
        let resolver = RegionResolver::new(1000).expect("should load resolver");
        let resolved = resolver.resolve("US").expect("should resolve");

        // All bands should be <= 1000 MHz
        for band in &resolved.lte_sweep_bands {
            assert!(
                band.end <= 1000 * 1_000_000,
                "Band end {} should be <= tuner max",
                band.end
            );
        }

        // Some bands should be skipped (those >1000 MHz)
        assert!(
            !resolved.skipped_bands.is_empty(),
            "should have skipped bands above tuner limit"
        );
    }

    #[test]
    fn validates_fm_range() {
        let resolver = RegionResolver::new(1766).expect("should load resolver");
        let resolved = resolver.resolve("US").expect("should resolve");

        // FM range should be reasonable (not 0)
        assert!(resolved.fm_sweep_start > 0);
        assert!(resolved.fm_sweep_end > resolved.fm_sweep_start);
        assert!(resolved.fm_sweep_step > 0);
    }
}

/// STUB(contract C7) - DROP AT MERGE: DAEMON-DSP's real implementation replaces this.
pub fn resolve_for_location(_lat: f64, _lon: f64) -> Option<&'static str> {
    None
}
