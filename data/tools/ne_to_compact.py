#!/usr/bin/env python3
"""Convert Natural Earth admin-0 GeoJSON to the compact boundary file used by
regions::resolve_for_location.

Usage: python3 -I ne_to_compact.py <ne_50m_admin_0_countries.geojson> <frequency-regions.json> <out.bin>

Format (all integers LEB128 varints; zigzag where signed):
  magic "AZB1"
  n_countries
  per country: 2 ASCII bytes ISO-3166 alpha-2, n_rings,
    per ring: n_points, first point (lon_cd, lat_cd) zigzag, then
    (dlon, dlat) zigzag deltas.   cd = centidegrees (0.01 deg).
Rings of every polygon (outer and holes) are stored together; the reader
applies the even-odd rule, which handles holes and multipolygons.
Simplification: Douglas-Peucker, tolerance TOL_DEG, after quantisation.
Source: Natural Earth (public domain). Python 3 stdlib only.
"""
import json
import math
import sys

import os
TOL_DEG = float(os.environ.get("TOL_DEG", "0.02"))
MIN_RING_SPAN_DEG = 0.03  # drop islets smaller than ~3 km in both axes


def zz(v):
    return (v << 1) ^ (v >> 63) if v < 0 else v << 1


def varint(n, out):
    while True:
        b = n & 0x7F
        n >>= 7
        if n:
            out.append(b | 0x80)
        else:
            out.append(b)
            return


def seg_dist(p, a, b):
    ax, ay = a
    bx, by = b
    dx, dy = bx - ax, by - ay
    L = dx * dx + dy * dy
    if L == 0:
        return math.hypot(p[0] - ax, p[1] - ay)
    t = max(0.0, min(1.0, ((p[0] - ax) * dx + (p[1] - ay) * dy) / L))
    return math.hypot(p[0] - (ax + t * dx), p[1] - (ay + t * dy))


def dp(pts, tol):
    keep = [False] * len(pts)
    keep[0] = keep[-1] = True
    stack = [(0, len(pts) - 1)]
    while stack:
        i, j = stack.pop()
        best, bi = -1.0, -1
        for k in range(i + 1, j):
            d = seg_dist(pts[k], pts[i], pts[j])
            if d > best:
                best, bi = d, k
        if best > tol:
            keep[bi] = True
            stack.append((i, bi))
            stack.append((bi, j))
    return [p for p, k in zip(pts, keep) if k]


def rings_of(geom):
    if geom["type"] == "Polygon":
        polys = [geom["coordinates"]]
    else:
        polys = geom["coordinates"]
    for poly in polys:
        for ring in poly:
            yield ring


def main():
    src, regions, out_path = sys.argv[1:4]
    wanted = {k for k in json.load(open(regions))["regions"] if not k.startswith("_")}
    feats = json.load(open(src))["features"]
    blob = bytearray(b"AZB1")
    by_code = {}
    for f in feats:
        code = f["properties"].get("ISO_A2_EH")
        if code in wanted:
            by_code.setdefault(code, []).append(f["geometry"])
    chosen = sorted(by_code.items())
    if set(by_code) != wanted:
        sys.exit("missing countries: %s" % (wanted - set(by_code)))
    varint(len(chosen), blob)
    total_pts_in = total_pts_out = 0
    for code, geoms in chosen:
        rings = []
        for ring in (r for g in geoms for r in rings_of(g)):
            q = [(round(x * 100), round(y * 100)) for x, y in ring]
            dedup = [q[0]]
            for pt in q[1:]:
                if pt != dedup[-1]:
                    dedup.append(pt)
            total_pts_in += len(ring)
            xs = [a for a, _ in dedup]
            ys = [b for _, b in dedup]
            if (max(xs) - min(xs)) < MIN_RING_SPAN_DEG * 100 and (max(ys) - min(ys)) < MIN_RING_SPAN_DEG * 100:
                continue
            s = dp([(float(a), float(b)) for a, b in dedup], TOL_DEG * 100)
            if len(s) < 4:
                continue
            total_pts_out += len(s)
            rings.append([(int(a), int(b)) for a, b in s])
        blob += code.encode("ascii")
        varint(len(rings), blob)
        for r in rings:
            varint(len(r), blob)
            px, py = r[0]
            varint(zz(px), blob)
            varint(zz(py), blob)
            for x, y in r[1:]:
                varint(zz(x - px), blob)
                varint(zz(y - py), blob)
                px, py = x, y
    open(out_path, "wb").write(bytes(blob))
    print("countries=%d points_in=%d points_out=%d bytes=%d" % (len(chosen), total_pts_in, total_pts_out, len(blob)))


main()
