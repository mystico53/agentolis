//! Diagnostic renders. Structure legibility beats beauty here: blocks, lots
//! and buildings must be told apart at a glance.

use std::collections::BTreeMap;

use polis_layout::determinism::fnv1a64_str;

use crate::accrete::Settlement;
use crate::city::City;
use crate::geom::*;
use crate::graph::{Graph, RoadClass};
use crate::metrics::Metrics;
use crate::raster::{Canvas, Rgb};

pub struct View {
    pub s: f64,
    pub ox: f64,
    pub oy: f64,
    pub h: f64,
}

impl View {
    pub fn fit(lo: P, hi: P, w: usize, h: usize, margin: f64) -> Self {
        let dw = (hi[0] - lo[0]).max(1e-6);
        let dh = (hi[1] - lo[1]).max(1e-6);
        let s = ((w as f64 - 2.0 * margin) / dw).min((h as f64 - 2.0 * margin) / dh);
        Self {
            s,
            ox: margin - lo[0] * s + ((w as f64 - 2.0 * margin) - dw * s) * 0.5,
            oy: margin - lo[1] * s + ((h as f64 - 2.0 * margin) - dh * s) * 0.5,
            h: h as f64,
        }
    }
    #[inline]
    pub fn p(&self, q: P) -> P {
        [q[0] * self.s + self.ox, self.h - (q[1] * self.s + self.oy)]
    }
    pub fn poly(&self, q: &[P]) -> Vec<P> {
        q.iter().map(|x| self.p(*x)).collect()
    }
}

fn hsl(h: f64, s: f64, l: f64) -> Rgb {
    let h = h.rem_euclid(1.0) * 6.0;
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h % 2.0) - 1.0).abs());
    let m = l - c * 0.5;
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    [
        ((r + m) * 255.0).clamp(0.0, 255.0) as u8,
        ((g + m) * 255.0).clamp(0.0, 255.0) as u8,
        ((b + m) * 255.0).clamp(0.0, 255.0) as u8,
    ]
}

/// Hue by top-level directory, lightness by depth: the directory tree, as
/// colour. Adjacent quarters of the same package read as one family.
pub fn district_colors(s: &Settlement) -> Vec<Rgb> {
    let mut tops: Vec<String> = Vec::new();
    for d in &s.districts {
        let t = d.path.as_str().split('/').next().unwrap_or("").to_string();
        if !tops.contains(&t) {
            tops.push(t);
        }
    }
    tops.sort();
    let n = tops.len().max(1) as f64;
    let mut out = Vec::with_capacity(s.districts.len());
    for d in &s.districts {
        let t = d.path.as_str().split('/').next().unwrap_or("").to_string();
        let i = tops.iter().position(|x| *x == t).unwrap_or(0) as f64;
        // Evenly spaced hues, then a per-district nudge so siblings differ.
        let nudge = ((fnv1a64_str(d.path.as_str()) >> 11) & 0xFFFF) as f64 / 65535.0;
        let hue = i / n + (nudge - 0.5) * (0.30 / n);
        let depth = d.path.as_str().matches('/').count() as f64;
        // Low saturation and low lightness: the ground is a tint, not a
        // statement. The roads and the buildings carry the reading; the colour
        // is only there to say which package you are standing in.
        let sat = if d.industrial {
            0.03
        } else {
            (0.34 - depth * 0.020).clamp(0.16, 0.36)
        };
        let lit = if d.industrial {
            0.17
        } else {
            (0.20 + nudge * 0.030 + depth * 0.010).clamp(0.16, 0.28)
        };
        out.push(hsl(hue, sat, lit));
    }
    out
}

fn shade(c: Rgb, k: f64) -> Rgb {
    [
        (f64::from(c[0]) * k).clamp(0.0, 255.0) as u8,
        (f64::from(c[1]) * k).clamp(0.0, 255.0) as u8,
        (f64::from(c[2]) * k).clamp(0.0, 255.0) as u8,
    ]
}

const BG: Rgb = [12, 13, 17];
const ROAD_HI: Rgb = [236, 235, 228];
const ROAD_MID: Rgb = [196, 198, 200];
const ROAD_LO: Rgb = [142, 147, 156];
const LOTLINE: Rgb = [96, 102, 112];
const BLOCKLINE: Rgb = [16, 17, 21];
const BLDG: Rgb = [238, 233, 220];
const MONU: Rgb = [255, 206, 84];
const INDUS: Rgb = [92, 96, 104];
const STREET: Rgb = [92, 232, 216];
const DBOUND: Rgb = [255, 246, 214];
const CIVIC: Rgb = [242, 226, 176];

#[allow(clippy::too_many_arguments)]
pub fn render_city(
    s: &Settlement,
    g: &Graph,
    c: &City,
    m: &Metrics,
    title: &str,
    out_px: usize,
    ss: usize,
) -> Canvas {
    let w = out_px * ss;
    let mut cv = Canvas::new(w, w, BG);
    let mut lo = [f64::INFINITY, f64::INFINITY];
    let mut hi = [f64::NEG_INFINITY, f64::NEG_INFINITY];
    for b in &c.blocks {
        let (l, h) = bounds(&b.ring);
        lo[0] = lo[0].min(l[0]);
        lo[1] = lo[1].min(l[1]);
        hi[0] = hi[0].max(h[0]);
        hi[1] = hi[1].max(h[1]);
    }
    let legend_h = (w as f64) * 0.085;
    let v = View::fit(
        lo,
        hi,
        w,
        w - legend_h as usize,
        (w as f64) * 0.022,
    );
    let colors = district_colors(s);
    let px = v.s;
    // The median block's diameter, in pixels. Every stroke below is a fraction
    // of it, so the drawing reads the same at 90 files and at 5000.
    let unit = {
        let mut d: Vec<f64> = c.blocks.iter().map(|b| area(&b.ring).sqrt()).collect();
        d.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        px * if d.is_empty() { 1.0 } else { d[d.len() / 2] }
    };

    // 1. The ground of each district, block by block. Districts tile: every
    //    block belongs to exactly one, so this layer has no gaps.
    for b in &c.blocks {
        let poly = v.poly(&b.ring);
        let col = match b.district {
            Some(d) => colors[d as usize],
            None => [26, 28, 33],
        };
        cv.fill_poly(&poly, col, 1.0);
        if b.industrial {
            // One mass, deliberately dull (PRD 8).
            cv.fill_poly(&poly, INDUS, 0.80);
        }
        cv.stroke_poly(&poly, (unit * 0.011).max(0.6), BLOCKLINE, 0.75);
    }
    // 2. Lots: the buildable parcels inside each block, a shade lighter than
    //    the ground so a block reads as a group of lots.
    for l in &c.lots {
        if c.blocks[l.block as usize].industrial {
            continue;
        }
        let poly = v.poly(&l.ring);
        let base = match c.blocks[l.block as usize].district {
            Some(d) => shade(colors[d as usize], 1.55),
            None => [34, 36, 42],
        };
        cv.fill_poly(&poly, base, 1.0);
        cv.stroke_poly(&poly, (unit * 0.010).max(0.6), LOTLINE, 0.50);
    }
    // 3. Buildings.
    for b in &c.buildings {
        if c.blocks[c.lots[b.lot as usize].block as usize].industrial {
            continue;
        }
        let poly = v.poly(&b.ring);
        let col = if b.monument {
            MONU
        } else if b.industrial {
            INDUS
        } else {
            shade(BLDG, 0.80 + b.height * 0.11)
        };
        cv.fill_poly(&poly, col, 1.0);
        // A dark keyline so neighbouring buildings do not merge into one blob.
        cv.stroke_poly(&poly, (unit * 0.007).max(0.5), [22, 22, 26], 0.60);
        if b.monument {
            cv.stroke_poly(&poly, (unit * 0.018).max(1.1), [255, 236, 170], 0.90);
        }
    }
    // 4. Civic square (PRD 8) - open ground at the historic centre.
    if let Some(ci) = c.civic_block {
        let poly = v.poly(&c.blocks[ci as usize].ring);
        cv.fill_poly(&poly, CIVIC, 0.92);
    }
    // 5. Roads, by class. The network is the thing the eye should find first.
    for e in 0..g.edges.len() {
        let poly = v.poly(&g.curve[e]);
        let (wdt, col) = match g.class[e] {
            RoadClass::Arterial => (unit * 0.078, ROAD_HI),
            RoadClass::Street => (unit * 0.044, ROAD_MID),
            RoadClass::Lane => (unit * 0.024, ROAD_LO),
        };
        cv.polyline(&poly, wdt.max(1.0), col, 0.96);
    }
    // 6. District boundaries: a bright centre stripe on the streets that
    //    separate two quarters, so the district skeleton is traceable at any
    //    zoom without hiding the network under it (PRD 8).
    for &e in &c.district_edges {
        let poly = v.poly(&g.curve[e]);
        cv.polyline(&poly, (unit * 0.012).max(0.8), DBOUND, 0.28);
    }
    for &e in &c.package_edges {
        let poly = v.poly(&g.curve[e]);
        cv.polyline(&poly, (unit * 0.024).max(1.2), DBOUND, 0.92);
    }
    // 7. Streets: cross-district imports, routed along the roads (PRD 9).
    //    Thin and few - a diagnostic layer, not the structure.
    let mut st: Vec<&crate::city::Street> = c.streets.iter().collect();
    st.sort_by_key(|x| std::cmp::Reverse(x.weight));
    st.truncate((c.blocks.len() / 9).clamp(6, 60));
    for s2 in st {
        let poly = v.poly(&s2.polyline);
        let wdt = (unit * (0.010 + 0.004 * f64::from(s2.weight.min(8)))).max(0.9);
        cv.polyline(&poly, wdt, STREET, 0.55);
    }
    // 8. Names of the largest quarters: PRD 8 asks the district skeleton to
    //    stay readable, and a label is the cheapest way to make it so.
    {
        let mut acc: BTreeMap<u32, (P, f64, usize)> = BTreeMap::new();
        for b in &c.blocks {
            if let Some(d) = b.district {
                let a = area(&b.ring);
                let cn = centroid(&b.ring);
                let e = acc.entry(d).or_insert(([0.0, 0.0], 0.0, 0));
                e.0 = add(e.0, mul(cn, a));
                e.1 += a;
                e.2 += 1;
            }
        }
        let mut ranked: Vec<(u32, P, f64)> = acc
            .iter()
            .filter(|(_, (_, a, n))| *n >= 3 && *a > 0.0)
            .map(|(d, (sm, a, _))| (*d, mul(*sm, 1.0 / *a), *a))
            .collect();
        ranked.sort_by(|x, y| {
            y.2.partial_cmp(&x.2)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(x.0.cmp(&y.0))
        });
        let mut taken: Vec<(P, f64, f64)> = Vec::new();
        let mut used_leaf: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let fs = (unit * 0.038).clamp(2.0 * ss as f64, 3.2 * ss as f64);
        for (d, at, _) in ranked.into_iter().take(60) {
            if taken.len() >= 16 {
                break;
            }
            let full = s.districts[d as usize].path.as_str();
            let mut it = full.rsplitn(3, '/');
            let leaf = it.next().unwrap_or("");
            let up = it.next();
            let name: String = if full.is_empty() {
                "ROOT".to_string()
            } else if used_leaf.contains(&leaf.to_string()) {
                match up {
                    Some(u) => format!("{u}/{leaf}"),
                    None => leaf.to_string(),
                }
            } else {
                leaf.to_string()
            };
            used_leaf.insert(leaf.to_string());
            let name = name.as_str();
            let q = v.p(at);
            let tw = Canvas::text_width(name, fs);
            let th = 10.0 * ss as f64;
            if taken
                .iter()
                .any(|(o, ow, oh)| (o[0] - q[0]).abs() < (ow + tw) * 0.5 + 4.0 && (o[1] - q[1]).abs() < (oh + th) * 0.6)
            {
                continue;
            }
            taken.push((q, tw, th));
            cv.rect(
                q[0] - tw * 0.5 - 3.0 * ss as f64,
                q[1] - 6.0 * ss as f64,
                q[0] + tw * 0.5 + 3.0 * ss as f64,
                q[1] + 9.0 * ss as f64,
                [8, 9, 12],
                0.62,
            );
            cv.text(q[0] - tw * 0.5, q[1] - 3.0 * ss as f64, name, fs, [232, 232, 236]);
        }
    }

    legend(&mut cv, s, m, title, w, legend_h, ss);
    cv.downsample(ss)
}

fn legend(cv: &mut Canvas, s: &Settlement, m: &Metrics, title: &str, w: usize, lh: f64, ss: usize) {
    let y0 = w as f64 - lh;
    cv.rect(0.0, y0, w as f64, w as f64, [8, 9, 12], 0.92);
    cv.rect(0.0, y0, w as f64, y0 + 2.0 * ss as f64, [70, 74, 84], 1.0);
    let fs = 2.0 * ss as f64;
    let pad = 14.0 * ss as f64;
    cv.text(pad, y0 + 10.0 * ss as f64, title, fs * 1.5, [240, 240, 240]);

    let sw = [
        ("DISTRICT GROUND", [46u8, 52u8, 64u8]),
        ("LOT", [64, 72, 88]),
        ("BUILDING", BLDG),
        ("MONUMENT", MONU),
        ("INDUSTRIAL", INDUS),
        ("ARTERIAL", ROAD_HI),
        ("STREET", ROAD_MID),
        ("LANE", ROAD_LO),
        ("PACKAGE BORDER", DBOUND),
        ("IMPORT ROUTE", STREET),
        ("CIVIC SQUARE", CIVIC),
    ];
    let mut x = pad;
    let ly = y0 + 34.0 * ss as f64;
    for (name, col) in sw {
        cv.rect(x, ly, x + 11.0 * ss as f64, ly + 11.0 * ss as f64, col, 1.0);
        cv.text(
            x + 15.0 * ss as f64,
            ly + 2.0 * ss as f64,
            name,
            fs * 0.95,
            [205, 208, 214],
        );
        x += 15.0 * ss as f64 + Canvas::text_width(name, fs * 0.95) + 16.0 * ss as f64;
        if x > w as f64 - 200.0 * ss as f64 {
            break;
        }
    }
    let l1 = format!(
        "ROADS V={} E={} COMPONENTS={} CYCLES={} CROSSINGS={}  DEG 3={} 4={} 5+={} DANGLING={}",
        m.nodes,
        m.edges,
        m.components,
        m.cycles,
        m.crossings,
        m.deg.get(&3).copied().unwrap_or(0),
        m.deg.get(&4).copied().unwrap_or(0),
        m.deg.iter().filter(|(d, _)| **d >= 5).map(|(_, c)| *c).sum::<usize>(),
        m.dangling
    );
    // Deliberately no timing here: a wall-clock number in the image would make
    // the PNG itself non-reproducible, which is exactly the class of leak PRD
    // 7.4 is about. Timings go to stdout and to the writeup.
    let l2 = format!(
        "BLOCKS={} SLIVERS={} LOTS={} BUILDINGS={}/{} OUTSIDE-LOT={} ON-ROAD={} PLOTS={} DISTRICTS={} SHARING-BORDER={} FRAGMENTED={}",
        m.blocks,
        m.slivers,
        m.lots,
        m.buildings,
        m.files,
        m.buildings_outside_lot,
        m.buildings_on_road,
        s.plots.len(),
        m.districts,
        m.districts_with_shared_border,
        m.districts_fragmented
    );
    cv.text(pad, y0 + 56.0 * ss as f64, &l1, fs * 0.95, [168, 174, 186]);
    cv.text(pad, y0 + 72.0 * ss as f64, &l2, fs * 0.95, [168, 174, 186]);
}

pub fn render_junctions(g: &Graph, m: &Metrics, title: &str, out_px: usize, ss: usize) -> Canvas {
    let w = out_px * ss;
    let mut cv = Canvas::new(w, w, [10, 11, 14]);
    let (lo, hi) = bounds(&g.nodes);
    let legend_h = (w as f64) * 0.075;
    let v = View::fit(lo, hi, w, w - legend_h as usize, (w as f64) * 0.025);
    // Scale everything to the median road segment, so the graph is legible at
    // any repository size.
    let unit = {
        let mut l: Vec<f64> = (0..g.edges.len()).map(|e| g.edge_len(e) * v.s).collect();
        l.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        if l.is_empty() { 8.0 } else { l[l.len() / 2] }
    };

    for e in 0..g.edges.len() {
        let poly = v.poly(&g.curve[e]);
        cv.polyline(&poly, (unit * 0.055).max(1.0), [124, 134, 152], 0.95);
    }
    let pal: BTreeMap<usize, Rgb> = [
        (1, [255, 70, 70]),
        (2, [70, 100, 190]),
        (3, [70, 200, 170]),
        (4, [255, 205, 60]),
        (5, [255, 110, 235]),
    ]
    .into_iter()
    .collect();
    let mut order: Vec<(usize, usize)> = (0..g.nodes.len())
        .map(|i| (g.degree(i), i))
        .collect();
    // Draw low degrees first so the rare high-degree nodes sit on top.
    order.sort_unstable();
    for (d, i) in order {
        let key = d.min(5);
        let col = *pal.get(&key).unwrap_or(&[255, 255, 255]);
        let r = match key {
            1 => unit * 0.20,
            2 => unit * 0.065,
            3 => unit * 0.105,
            4 => unit * 0.155,
            _ => unit * 0.195,
        };
        cv.disc(v.p(g.nodes[i]), r.max(1.1), col, 1.0);
    }

    let y0 = w as f64 - legend_h;
    cv.rect(0.0, y0, w as f64, w as f64, [7, 8, 10], 0.95);
    cv.rect(0.0, y0, w as f64, y0 + 2.0 * ss as f64, [70, 74, 84], 1.0);
    let fs = 2.0 * ss as f64;
    let pad = 14.0 * ss as f64;
    cv.text(pad, y0 + 9.0 * ss as f64, title, fs * 1.5, [240, 240, 240]);
    let tot = m.junction_nodes.max(1);
    let mut x = pad;
    let ly = y0 + 32.0 * ss as f64;
    for d in 1..=5usize {
        let cnt: usize = if d == 5 {
            m.deg.iter().filter(|(k, _)| **k >= 5).map(|(_, c)| *c).sum()
        } else {
            m.deg.get(&d).copied().unwrap_or(0)
        };
        let label = if d == 5 {
            format!("DEG 5+ : {cnt}")
        } else if d == 2 {
            format!("DEG 2 (BEND) : {cnt}")
        } else if d == 1 {
            format!("DEG 1 (DANGLING) : {cnt}")
        } else {
            format!("DEG {d} : {cnt}")
        };
        cv.disc(
            [x + 6.0 * ss as f64, ly + 6.0 * ss as f64],
            6.0 * ss as f64,
            pal[&d],
            1.0,
        );
        cv.text(
            x + 17.0 * ss as f64,
            ly + 2.0 * ss as f64,
            &label,
            fs * 0.95,
            [205, 208, 214],
        );
        x += 17.0 * ss as f64 + Canvas::text_width(&label, fs * 0.95) + 20.0 * ss as f64;
    }
    let l = format!(
        "V={} E={} COMPONENTS={} CYCLES(E-V+C)={} CROSSINGS WITHOUT A NODE={}  4-AND-5+ SHARE OF JUNCTIONS={}%  A TREE WOULD HAVE CYCLES=0",
        m.nodes,
        m.edges,
        m.components,
        m.cycles,
        m.crossings,
        (100.0
            * (m.deg.get(&4).copied().unwrap_or(0)
                + m.deg.iter().filter(|(k, _)| **k >= 5).map(|(_, c)| *c).sum::<usize>())
                as f64
            / tot as f64) as i64
    );
    cv.text(pad, y0 + 54.0 * ss as f64, &l, fs * 0.95, [168, 174, 186]);
    cv.downsample(ss)
}
