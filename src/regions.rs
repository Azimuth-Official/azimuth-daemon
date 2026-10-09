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
    /// Channel number of the first channel (the channel whose lower edge is `low_mhz`); 0 = unknown.
    #[serde(default)]
    pub first_channel: u32,
    pub name: String,
    #[serde(default)]
    pub _note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegionConfig {
    pub name: String,
    pub lte_bands: Vec<u32>,
    pub fm_low_mhz: f64,
    pub fm_high_mhz: f64,
    pub dtv_standard: String,
    /// FM channel raster in kHz: 200 in the Americas and South Korea (odd tenths
    /// from 87.5 MHz), 100 elsewhere (every tenth).
    #[serde(default = "default_fm_step_khz")]
    pub fm_step_khz: u32,
    #[serde(default)]
    pub _note: Option<String>,
}

fn default_fm_step_khz() -> u32 {
    100
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
    /// DTV standard name from the region database ("DVB-T2-694", "ATSC", ...).
    #[allow(dead_code)] // read by Config::apply_region (DAEMON-CORE) once wired
    pub dtv_standard: String,
    #[allow(dead_code)]
    pub dtv_channel_mhz: u32,
    #[allow(dead_code)]
    pub dtv_first_channel: u32,
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
        let code_normalized = if region_code.is_empty() || region_code.eq_ignore_ascii_case("AUTO") {
            "_DEFAULT".to_string()
        } else {
            region_code.to_uppercase()
        };

        let (region, effective_code) = match self.db.regions.get(&code_normalized) {
            Some(r) => (r.clone(), code_normalized),
            None => {
                if !region_code.eq_ignore_ascii_case("AUTO") && !region_code.is_empty() {
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
                        first_channel: 14,
                        name: "fallback UHF".to_string(),
                        _note: None,
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
            fm_sweep_step: u64::from(region.fm_step_khz) * 1000,
            lte_sweep_bands: sweep_bands,
            dtv_sweep_start: dtv_standard.low_mhz * 1_000_000,
            dtv_sweep_end: dtv_end,
            dtv_sweep_step: dtv_standard.channel_mhz * 1_000_000,
            dtv_standard: region.dtv_standard.clone(),
            dtv_channel_mhz: dtv_standard.channel_mhz as u32,
            dtv_first_channel: dtv_standard.first_channel,
            skipped_bands,
        })
    }
}

/// Compact offline country boundaries (Natural Earth admin-0, public domain),
/// generated by `data/tools/ne_to_compact.py`. See `data/README.md`.
static BOUNDARIES: &[u8] = include_bytes!("../data/country-boundaries.bin");

struct CountryShape {
    code: [u8; 2],
    /// Rings as centidegree (0.01 deg) lon/lat pairs; even-odd rule applies.
    rings: Vec<Ring>,
}

struct Ring {
    min_x: i32,
    max_x: i32,
    min_y: i32,
    max_y: i32,
    pts: Vec<(i32, i32)>,
}

fn read_varint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    let mut shift = 0;
    loop {
        let b = *buf.get(*pos)?;
        *pos += 1;
        v |= u64::from(b & 0x7F) << shift;
        if b & 0x80 == 0 {
            return Some(v);
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

fn decode_boundaries(buf: &[u8]) -> Option<Vec<CountryShape>> {
    if buf.get(..4)? != b"AZB1" {
        return None;
    }
    let mut pos = 4;
    let n = read_varint(buf, &mut pos)? as usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let code = [*buf.get(pos)?, *buf.get(pos + 1)?];
        pos += 2;
        let n_rings = read_varint(buf, &mut pos)? as usize;
        let mut rings = Vec::with_capacity(n_rings);
        for _ in 0..n_rings {
            let n_pts = read_varint(buf, &mut pos)? as usize;
            let mut pts = Vec::with_capacity(n_pts);
            let (mut x, mut y) = (0i64, 0i64);
            for i in 0..n_pts {
                let dx = unzigzag(read_varint(buf, &mut pos)?);
                let dy = unzigzag(read_varint(buf, &mut pos)?);
                if i == 0 {
                    x = dx;
                    y = dy;
                } else {
                    x += dx;
                    y += dy;
                }
                pts.push((x as i32, y as i32));
            }
            let min_x = pts.iter().map(|p| p.0).min()?;
            let max_x = pts.iter().map(|p| p.0).max()?;
            let min_y = pts.iter().map(|p| p.1).min()?;
            let max_y = pts.iter().map(|p| p.1).max()?;
            rings.push(Ring { min_x, max_x, min_y, max_y, pts });
        }
        out.push(CountryShape { code, rings });
    }
    Some(out)
}

fn shapes() -> &'static [CountryShape] {
    static SHAPES: std::sync::OnceLock<Vec<CountryShape>> = std::sync::OnceLock::new();
    SHAPES.get_or_init(|| decode_boundaries(BOUNDARIES).unwrap_or_default())
}

fn ring_contains(ring: &Ring, x: i32, y: i32) -> bool {
    if x < ring.min_x || x > ring.max_x || y < ring.min_y || y > ring.max_y {
        return false;
    }
    let mut inside = false;
    let n = ring.pts.len();
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = (f64::from(ring.pts[i].0), f64::from(ring.pts[i].1));
        let (xj, yj) = (f64::from(ring.pts[j].0), f64::from(ring.pts[j].1));
        let (px, py) = (f64::from(x), f64::from(y));
        if (yi > py) != (yj > py) && px < (xj - xi) * (py - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Resolve a WGS-84 position to an ISO-3166 alpha-2 code that is a key of
/// `data/frequency-regions.json`, fully offline.
///
/// Returns `None` for non-finite or out-of-range input, the (0, 0) "null
/// island" placeholder, open ocean, and countries not in the region database.
///
/// Accuracy: boundaries are Natural Earth 1:10m, quantised to 0.01 deg and
/// simplified with a 0.02 deg tolerance, so a border can be displaced by up to
/// about 3 km on top of the source's own generalisation. Positions at least
/// 5 km from a national border resolve to the correct country
/// (see `data/README.md` for the measured figures). Coastal points within a few
/// km of the shoreline may resolve to `None`.
#[allow(dead_code)] // consumed by Config::apply_region (DAEMON-CORE)
pub fn resolve_for_location(lat: f64, lon: f64) -> Option<&'static str> {
    if !lat.is_finite() || !lon.is_finite() || lat.abs() > 90.0 || lon.abs() > 180.0 {
        return None;
    }
    if lat == 0.0 && lon == 0.0 {
        return None;
    }
    let x = (lon * 100.0).round() as i32;
    let y = (lat * 100.0).round() as i32;
    for shape in shapes() {
        let mut inside = false;
        for ring in &shape.rings {
            if ring_contains(ring, x, y) {
                inside = !inside;
            }
        }
        if inside {
            return std::str::from_utf8(&shape.code).ok().and_then(|c| {
                // The database keys are static strings; map back to one.
                const CODES: &[&str] = &[
                    "AE", "AR", "AU", "BR", "CA", "CL", "CN", "DE", "ES", "FR", "GB", "ID", "IN",
                    "IT", "JP", "KE", "KR", "MX", "MY", "NG", "NL", "NO", "NZ", "PH", "PL", "RU",
                    "SA", "SE", "SG", "TH", "TR", "TW", "UA", "US", "ZA",
                ];
                CODES.iter().copied().find(|k| *k == c)
            });
        }
    }
    None
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

    #[test]
    fn dtt_plans_use_channel_centre_rasters_and_post_700mhz_edges() {
        let resolver = RegionResolver::new(1766).unwrap();
        for code in ["GB", "DE", "FR", "ES", "IT", "NL", "SE", "NO", "PL"] {
            let r = resolver.resolve(code).unwrap();
            assert_eq!((r.dtv_sweep_start, r.dtv_sweep_end, r.dtv_channel_mhz, r.dtv_first_channel), (470_000_000, 694_000_000, 8, 21), "{code}");
            assert!(r.dtv_standard.starts_with("DVB"));
        }
        let au = resolver.resolve("AU").unwrap();
        assert_eq!((au.dtv_sweep_start, au.dtv_sweep_end, au.dtv_channel_mhz, au.dtv_first_channel), (526_000_000, 694_000_000, 7, 28));
        let us = resolver.resolve("US").unwrap();
        assert_eq!((us.dtv_standard.as_str(), us.dtv_sweep_step, us.dtv_first_channel), ("ATSC", 6_000_000, 14));
    }

    #[test]
    fn fm_raster_follows_the_region() {
        let resolver = RegionResolver::new(1766).unwrap();
        for c in ["US", "CA", "MX", "BR", "AR", "CL", "KR"] {
            assert_eq!(resolver.resolve(c).unwrap().fm_sweep_step, 200_000, "{c}");
        }
        for c in ["FR", "GB", "DE", "JP", "AU", "IN", "ZA", "_DEFAULT"] {
            assert_eq!(resolver.resolve(c).unwrap().fm_sweep_step, 100_000, "{c}");
        }
        assert_eq!(resolver.resolve("auto").unwrap().region_code, "_DEFAULT");
        assert_eq!(resolver.resolve("Auto").unwrap().region_code, "_DEFAULT");
    }

    #[test]
    fn resolves_spec_city_centroids() {
        for (lat, lon, want) in [
            (48.8566, 2.3522, "FR"),
            (51.5074, -0.1278, "GB"),
            (52.5200, 13.4050, "DE"),
            (39.8283, -98.5795, "US"),
            (43.6532, -79.3832, "CA"),
            (35.6762, 139.6503, "JP"),
        ] {
            assert_eq!(resolve_for_location(lat, lon), Some(want), "({lat}, {lon})");
        }
    }

    #[test]
    fn ocean_null_island_and_garbage_resolve_to_none() {
        assert_eq!(resolve_for_location(30.0, -40.0), None);
        assert_eq!(resolve_for_location(0.0, 0.0), None);
        assert_eq!(resolve_for_location(f64::NAN, 10.0), None);
        assert_eq!(resolve_for_location(95.0, 10.0), None);
    }

    #[test]
    fn resolved_codes_exist_in_region_database_and_tokyo_has_japan_fm() {
        let resolver = RegionResolver::new(1766).unwrap();
        let code = resolve_for_location(35.6762, 139.6503).unwrap();
        let r = resolver.resolve(code).unwrap();
        assert_eq!(r.region_code, "JP");
        assert_eq!(r.fm_sweep_start, 76_000_000);
        assert_eq!(r.fm_sweep_end, 95_000_000);
    }

    #[test]
    fn more_city_centroids_resolve_to_their_country() {
        for (lat, lon, want) in [
            (50.6292, 3.0573, "FR"),
            (40.4168, -3.7038, "ES"),
            (41.9028, 12.4964, "IT"),
            (52.3676, 4.9041, "NL"),
            (52.2297, 21.0122, "PL"),
            (-33.8688, 151.2093, "AU"),
            (-36.8485, 174.7633, "NZ"),
            (19.4326, -99.1332, "MX"),
            (55.9533, -3.1883, "GB"),
            (50.4501, 30.5234, "UA"),
            (41.0082, 28.9784, "TR"),
            (37.5665, 126.9780, "KR"),
            (28.6139, 77.2090, "IN"),
            (-23.5505, -46.6333, "BR"),
            (59.3293, 18.0686, "SE"),
        ] {
            assert_eq!(resolve_for_location(lat, lon), Some(want), "({lat}, {lon})");
        }
    }
}
