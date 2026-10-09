# data/

## frequency-regions.json
Country to band/DTT-plan database embedded by `src/regions.rs`. Per-country DTT
notes carry their sources in `_note` fields.

## country-boundaries.bin
Offline country boundaries used by `regions::resolve_for_location` (region
`AUTO` from the node's position). Generated, not hand-edited.

- Source: Natural Earth admin-0 countries, 1:10m (public domain, see
  https://www.naturalearthdata.com/about/terms-of-use/), pinned to
  upstream commit `ca96624a56bd078437bca8184e78163e5039ad19`
  (github.com/nvkelso/natural-earth-vector).
- Only countries present in `frequency-regions.json` are stored (35).
- Generator: `tools/ne_to_compact.py` (python3 stdlib only), run as
  `python3 -I tools/ne_to_compact.py ne_10m_admin_0_countries.geojson frequency-regions.json country-boundaries.bin`.
  Coordinates are quantised to 0.01 degrees and Douglas-Peucker simplified with
  a 0.02 degree tolerance; islets under 0.03 degrees in both axes are dropped.
  Result: 156,209 bytes (73,671 vertices, from 323,012).
- sha256 `034227e5b8ecb5fe7d65bf4a0ea00067b3eea1d586b680371184f91cfd75c401`.

### Border accuracy
`tools/verify_boundaries.py` compares the compact file with the unsimplified 1:10m
data: for sampled border vertices it probes the two points 5 km either side of
the border (along the local normal) and requires the inland point to resolve to
the country and the outland point not to.

| probe class (3 seeds) | agreement |
|---|---|
| land borders (outland point is another country) | 99.2 to 99.6 percent (14, 11, 24 of about 2,800 differ) |
| coastline (outland point is sea) | about 95.5 to 96.1 percent |

Practical reading: displacement from the true line is at most about 3 km from
simplification plus the source's own generalisation, so a position at least 5 km
from a land border resolves correctly in all but rare narrow-feature cases
(enclaves, river borders, slivers). A position within a few km of a coastline,
or on a small islet, can resolve to no country; the caller then falls back to
the `_DEFAULT` plan, which is the documented behaviour for ocean and unknown
positions.
