#!/usr/bin/env python3
"""Rasterise a serialized CityLayout so a human can look at it.

    usage: python tests/render-city.py <city.json> <out.png> [pixels]

PRD §15's M1 gate is "byte-identical layout across two runs and across two
machines". A determinism test proves the map does not move. It does not prove
the map looks like a city, and the failure mode PRD §7.2 warns about —

    Place buildings first and connect them afterward and you get suburbia or a
    circuit board, every time.

— compiles, runs, passes every assertion and simply looks wrong. So this exists
to be *looked at*, not to be asserted on.

Why Python and not Rust: `polis-render` does not exist yet, `polis-layout` has no
image dependency, and PNG needs a real DEFLATE encoder. Python's `zlib` is in the
standard library, so this is forty lines instead of a hand-written compressor
inside a determinism test. Nothing here is part of the product; the input is the
same snapshot the golden files hold, so the picture cannot disagree with what was
tested.

Standard library only: zlib, struct, json, sys.
"""

import json
import struct
import sys
import zlib

# --- palette ---------------------------------------------------------------
#
# PRD §10.3: layers 1-2 (terrain, vacant lots, the city itself) live in roughly
# the bottom fifth of the contrast range; the ink is spent on what changes. There
# are no agents at M1, so the top of the range goes to the two things that are
# genuinely about attention: building height (PRD §7.3's unreviewed pile) and
# monuments (PRD §8's orientation anchors).

BACKGROUND = (16, 18, 22)
DISTRICT_FILL = (26, 30, 37)
DISTRICT_EDGE = (46, 54, 66)
BLOCK_FILL = (22, 25, 31)
VACANT_FILL = (30, 34, 30)
LOT_EDGE = (34, 39, 47)
ROAD_ALLEY = (52, 58, 68)
ROAD_STREET = (86, 95, 110)
ROAD_ARTERIAL = (140, 152, 172)
STREET_LINE = (70, 120, 130)
BUILDING_LOW = (108, 116, 128)
BUILDING_HIGH = (236, 210, 150)
MONUMENT = (240, 244, 250)
MONUMENT_EDGE = (120, 190, 210)


class Canvas:
    def __init__(self, size, colour):
        self.size = size
        self.px = bytearray(colour * (size * size))

    def blend(self, x, y, colour, alpha=1.0):
        if not (0 <= x < self.size and 0 <= y < self.size):
            return
        i = (y * self.size + x) * 3
        if alpha >= 1.0:
            self.px[i] = colour[0]
            self.px[i + 1] = colour[1]
            self.px[i + 2] = colour[2]
            return
        for c in range(3):
            old = self.px[i + c]
            self.px[i + c] = int(old + (colour[c] - old) * alpha)

    def fill_polygon(self, pts, colour, alpha=1.0):
        """Even-odd scanline fill. Concave polygons are the normal case here."""
        if len(pts) < 3:
            return
        ys = [p[1] for p in pts]
        top = max(0, int(min(ys)))
        bottom = min(self.size - 1, int(max(ys)) + 1)
        n = len(pts)
        for y in range(top, bottom + 1):
            centre = y + 0.5
            xs = []
            for i in range(n):
                x0, y0 = pts[i]
                x1, y1 = pts[(i + 1) % n]
                if (y0 <= centre) != (y1 <= centre):
                    t = (centre - y0) / (y1 - y0)
                    xs.append(x0 + (x1 - x0) * t)
            xs.sort()
            for i in range(0, len(xs) - 1, 2):
                for x in range(max(0, int(xs[i])), min(self.size - 1, int(xs[i + 1])) + 1):
                    self.blend(x, y, colour, alpha)

    def line(self, a, b, colour, width=1.0, alpha=1.0):
        x0, y0 = a
        x1, y1 = b
        steps = int(max(abs(x1 - x0), abs(y1 - y0))) + 1
        half = max(0, int(width / 2))
        for s in range(steps + 1):
            t = s / steps
            x = int(x0 + (x1 - x0) * t)
            y = int(y0 + (y1 - y0) * t)
            for dy in range(-half, half + 1):
                for dx in range(-half, half + 1):
                    self.blend(x + dx, y + dy, colour, alpha)

    def outline(self, pts, colour, width=1.0, alpha=1.0):
        for i in range(len(pts)):
            self.line(pts[i], pts[(i + 1) % len(pts)], colour, width, alpha)

    def write_png(self, path):
        raw = bytearray()
        stride = self.size * 3
        for y in range(self.size):
            raw.append(0)  # filter type 0
            raw += self.px[y * stride:(y + 1) * stride]

        def chunk(tag, data):
            return (
                struct.pack(">I", len(data))
                + tag
                + data
                + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
            )

        png = b"\x89PNG\r\n\x1a\n"
        png += chunk(b"IHDR", struct.pack(">IIBBBBB", self.size, self.size, 8, 2, 0, 0, 0))
        png += chunk(b"IDAT", zlib.compress(bytes(raw), 9))
        png += chunk(b"IEND", b"")
        with open(path, "wb") as f:
            f.write(png)


def lerp(a, b, t):
    t = max(0.0, min(1.0, t))
    return tuple(int(a[c] + (b[c] - a[c]) * t) for c in range(3))


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        return 2
    src, dst = sys.argv[1], sys.argv[2]
    size = int(sys.argv[3]) if len(sys.argv) > 3 else 1600

    with open(src, encoding="utf-8") as f:
        city = json.load(f)
    layout = city["layout"]

    # Fit the whole city, roads included, with a small margin.
    xs, ys = [], []
    for node in layout["roads"]["nodes"]:
        xs.append(node[0])
        ys.append(node[1])
    for block in layout["blocks"]:
        for p in block["boundary"]:
            xs.append(p[0])
            ys.append(p[1])
    if not xs:
        print("empty city")
        return 1
    lo_x, hi_x, lo_y, hi_y = min(xs), max(xs), min(ys), max(ys)
    span = max(hi_x - lo_x, hi_y - lo_y) or 1.0
    margin = 0.04 * size
    scale = (size - 2 * margin) / span
    cx, cy = (lo_x + hi_x) / 2, (lo_y + hi_y) / 2

    def to_px(p):
        # y is flipped: city space has y up, an image has y down.
        return (
            size / 2 + (p[0] - cx) * scale,
            size / 2 - (p[1] - cy) * scale,
        )

    canvas = Canvas(size, BACKGROUND)

    # 1. Districts, faintest of all (PRD §10.3 layer 1).
    for d in layout["districts"]:
        pts = [to_px(p) for p in d["boundary"]]
        canvas.fill_polygon(pts, DISTRICT_FILL, 0.55)
    for d in layout["districts"]:
        pts = [to_px(p) for p in d["boundary"]]
        canvas.outline(pts, DISTRICT_EDGE, 1, 0.6)

    # 2. Blocks.
    for b in layout["blocks"]:
        canvas.fill_polygon([to_px(p) for p in b["boundary"]], BLOCK_FILL, 0.9)

    # 3. Lots. Only a lot in the vacancy ledger has gone to seed (PRD §7.5);
    #    a lot with no occupant and no history was simply never built on, and
    #    tinting the two the same way makes a young city look derelict.
    gone_to_seed = {v["lot"] for v in city["vacancies"]}
    for lot in layout["lots"]:
        pts = [to_px(p) for p in lot["boundary"]]
        if lot["id"] in gone_to_seed:
            canvas.fill_polygon(pts, VACANT_FILL, 0.8)
        canvas.outline(pts, LOT_EDGE, 1, 0.35)

    # 4. Roads. Class sets the width, which is the PRD §12 decluttering channel.
    widths = {"arterial": 3.0, "street": 2.0, "alley": 1.0}
    colours = {"arterial": ROAD_ARTERIAL, "street": ROAD_STREET, "alley": ROAD_ALLEY}
    nodes = [to_px(n) for n in layout["roads"]["nodes"]]
    for seg in layout["roads"]["segments"]:
        canvas.line(
            nodes[seg["from"]],
            nodes[seg["to"]],
            colours[seg["class"]],
            widths[seg["class"]],
        )

    # 5. Streets (PRD §9), width proportional to distinct import edges.
    for street in layout["streets"]:
        poly = [to_px(p) for p in street["polyline"]]
        w = 1.0 + min(4.0, street["edges"] * 0.5)
        for i in range(len(poly) - 1):
            canvas.line(poly[i], poly[i + 1], STREET_LINE, w, 0.45)

    # 6. Buildings. Height gets the top of the contrast range (PRD §7.3, §10.3).
    monuments = {m["path"] for m in city["monuments"]}
    heights = [b["height"] for b in layout["buildings"]] or [1.0]
    hi_h = max(heights)
    lo_h = min(heights)
    span_h = (hi_h - lo_h) or 1.0
    for b in layout["buildings"]:
        pts = [to_px(p) for p in b["footprint"]]
        t = (b["height"] - lo_h) / span_h
        if b["path"] in monuments:
            canvas.fill_polygon(pts, MONUMENT, 1.0)
            canvas.outline(pts, MONUMENT_EDGE, 2, 1.0)
        else:
            canvas.fill_polygon(pts, lerp(BUILDING_LOW, BUILDING_HIGH, t), 1.0)

    canvas.write_png(dst)
    print(
        f"{dst}: {size}x{size}, {len(layout['buildings'])} buildings, "
        f"{len(layout['blocks'])} blocks, {len(layout['roads']['segments'])} segments, "
        f"{len(layout['districts'])} districts, {len(monuments)} monuments"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
