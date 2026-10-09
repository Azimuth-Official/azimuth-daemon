#!/usr/bin/env python3
"""Measure border accuracy of country-boundaries.bin against Natural Earth 1:10m.

Usage: python3 -I verify_boundaries.py <ne_10m_admin_0_countries.geojson> <country-boundaries.bin> [seed]

For sampled 10m border vertices of each database country, takes the two points
5 km either side of the border (along the local normal). The inland point must
resolve to that country; the outland point must NOT resolve to it. Reports the
mismatch rate (points within 5 km of a border) and the worst-case displacement
between the compact polygon vertices and the 10m boundary.
"""
import json, math, random, sys

def varint(b, p):
    v = s = 0
    while True:
        x = b[p]; p += 1
        v |= (x & 0x7F) << s
        if not x & 0x80:
            return v, p
        s += 7

def unzz(v):
    return (v >> 1) ^ -(v & 1)

def load(path):
    b = open(path, "rb").read()
    assert b[:4] == b"AZB1"
    p = 4
    n, p = varint(b, p)
    out = {}
    for _ in range(n):
        code = b[p:p+2].decode(); p += 2
        nr, p = varint(b, p)
        rings = []
        for _ in range(nr):
            npts, p = varint(b, p)
            pts = []
            x = y = 0
            for i in range(npts):
                dx, p = varint(b, p); dy, p = varint(b, p)
                dx, dy = unzz(dx), unzz(dy)
                if i == 0: x, y = dx, dy
                else: x += dx; y += dy
                pts.append((x, y))
            rings.append(pts)
        out[code] = rings
    return out

def inside(rings, x, y):
    ins = False
    for r in rings:
        j = len(r) - 1
        for i in range(len(r)):
            xi, yi = r[i]; xj, yj = r[j]
            if (yi > y) != (yj > y) and x < (xj - xi) * (y - yi) / (yj - yi) + xi:
                ins = not ins
            j = i
    return ins

def main():
    src, binp = sys.argv[1:3]
    rnd = random.Random(int(sys.argv[3]) if len(sys.argv) > 3 else 1)
    shapes = load(binp)
    feats = json.load(open(src))["features"]
    allpolys = []
    for f in feats:
        g = f["geometry"]
        ps = [g["coordinates"]] if g["type"] == "Polygon" else g["coordinates"]
        for poly in ps:
            xs = [c[0] for c in poly[0]]; ys = [c[1] for c in poly[0]]
            allpolys.append((min(xs), max(xs), min(ys), max(ys), poly))
    def on_land(x, y):
        for (x0, x1, y0, y1, poly) in allpolys:
            if x0 <= x <= x1 and y0 <= y <= y1 and inside_10([poly], x, y):
                return True
        return False
    stats = {"land": [0, 0], "coast": [0, 0]}
    ok = bad = 0
    bad_by = {}
    for f in feats:
        code = f["properties"].get("ISO_A2_EH")
        if code not in shapes: continue
        g = f["geometry"]
        polys = [g["coordinates"]] if g["type"] == "Polygon" else g["coordinates"]
        verts = [(ring[i], ring[i+1]) for poly in polys for ring in poly[:1] for i in range(len(ring)-1)]
        verts = [v for v in verts if abs(v[0][0]-v[1][0]) + abs(v[0][1]-v[1][1]) > 0.02]  # skip tiny edges
        for (a, b) in rnd.sample(verts, min(300, len(verts))):
            mx, my = (a[0]+b[0])/2, (a[1]+b[1])/2
            dx, dy = b[0]-a[0], b[1]-a[1]
            kx = 111.32 * math.cos(math.radians(my)); ky = 110.57
            nx, ny = -dy * ky, dx * kx
            nl = math.hypot(nx, ny)
            if nl == 0: continue
            # 5 km along normal in km space -> degrees
            ox, oy = 5.0 * nx / nl / kx, 5.0 * ny / nl / ky
            p1 = (mx + ox, my + oy); p2 = (mx - ox, my - oy)
            in10 = [inside_10(polys, *p) for p in (p1, p2)]
            if in10[0] == in10[1]:
                continue  # border is not a real edge (coastline sliver / hole)
            inland = p1 if in10[0] else p2
            outland = p2 if in10[0] else p1
            kind = "land" if on_land(*outland) else "coast"
            a_ok = inside(shapes[code], inland[0]*100, inland[1]*100)
            b_ok = not inside(shapes[code], outland[0]*100, outland[1]*100)
            stats[kind][0 if (a_ok and b_ok) else 1] += 1
            if a_ok and b_ok: ok += 1
            else:
                bad += 1; bad_by[code] = bad_by.get(code, 0) + 1
    print("5km-offset probes: ok=%d mismatch=%d (%.2f%%)" % (ok, bad, 100.0*bad/max(1, ok+bad)))
    for k, (o, m) in stats.items():
        print("%s probes: ok=%d mismatch=%d (%.2f%%)" % (k, o, m, 100.0 * m / max(1, o + m)))
    print("mismatches by country:", dict(sorted(bad_by.items(), key=lambda kv: -kv[1])))

def inside_10(polys, x, y):
    ins = False
    for poly in polys:
        for ring in poly:
            j = len(ring) - 1
            for i in range(len(ring)):
                xi, yi = ring[i]; xj, yj = ring[j]
                if (yi > y) != (yj > y) and x < (xj - xi) * (y - yi) / (yj - yi) + xi:
                    ins = not ins
                j = i
    return ins

main()
