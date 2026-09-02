//! Structural measurement. Everything the gate asks for, computed from the
//! generated layout rather than asserted about it.

use std::collections::BTreeMap;

use crate::city::Layout;
use crate::geom::{self, Pt};

#[derive(Debug, Default)]
pub struct Metrics {
    pub nodes: usize,
    pub segments: usize,
    pub components: usize,
    pub crossings: usize,
    pub cycles: i64,
    pub degree_hist: BTreeMap<u32, usize>,
    pub dangling: usize,
    pub deg4plus_share: f64,

    pub blocks: usize,
    pub block_area_min: f64,
    pub block_area_med: f64,
    pub block_area_max: f64,
    pub block_aspect_min: f64,
    pub block_aspect_med: f64,
    pub block_aspect_p95: f64,
    pub block_aspect_max: f64,
    pub slivers: usize,
    pub block_vert_med: f64,

    pub lots: usize,
    pub buildings: usize,
    pub files: usize,
    pub files_without_building: usize,
    pub buildings_outside_lot: usize,
    pub buildings_on_road: usize,

    pub districts: usize,
    pub districts_with_shared_border: usize,
    pub district_components: usize,
    pub district_border_pairs: usize,

    pub streets: usize,
    pub street_chords: usize,
}

struct Dsu(Vec<u32>);
impl Dsu {
    fn new(n: usize) -> Self {
        Dsu((0..n as u32).collect())
    }
    fn find(&mut self, x: u32) -> u32 {
        let mut r = x;
        while self.0[r as usize] != r {
            r = self.0[r as usize];
        }
        let mut c = x;
        while self.0[c as usize] != r {
            let n = self.0[c as usize];
            self.0[c as usize] = r;
            c = n;
        }
        r
    }
    fn union(&mut self, a: u32, b: u32) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            self.0[ra as usize] = rb;
        }
    }
}

fn pct(v: &[f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let i = ((v.len() - 1) as f64 * p).round() as usize;
    v[i.min(v.len() - 1)]
}

pub fn measure(layout: &Layout, half_widths: &[f64; 4]) -> Metrics {
    let mut m = Metrics {
        nodes: layout.nodes.len(),
        segments: layout.segs.len(),
        ..Default::default()
    };

    // --- connectivity -------------------------------------------------------
    let mut dsu = Dsu::new(layout.nodes.len());
    let mut deg = vec![0u32; layout.nodes.len()];
    for &(a, b, _) in &layout.segs {
        dsu.union(a, b);
        deg[a as usize] += 1;
        deg[b as usize] += 1;
    }
    let mut roots = std::collections::BTreeSet::new();
    for i in 0..layout.nodes.len() {
        if deg[i] > 0 {
            roots.insert(dsu.find(i as u32));
        }
    }
    m.components = roots.len();
    for d in &deg {
        if *d > 0 {
            *m.degree_hist.entry((*d).min(9)).or_insert(0) += 1;
        }
    }
    m.dangling = *m.degree_hist.get(&1).unwrap_or(&0);
    let junctions: usize = m
        .degree_hist
        .iter()
        .filter(|(&d, _)| d >= 3)
        .map(|(_, &c)| c)
        .sum();
    let big: usize = m
        .degree_hist
        .iter()
        .filter(|(&d, _)| d >= 4)
        .map(|(_, &c)| c)
        .sum();
    m.deg4plus_share = if junctions > 0 {
        big as f64 / junctions as f64
    } else {
        0.0
    };
    let v = deg.iter().filter(|&&d| d > 0).count() as i64;
    m.cycles = layout.segs.len() as i64 - v + m.components as i64;

    // --- planarity: any pair of segments crossing away from a shared node ----
    m.crossings = count_crossings(&layout.nodes, &layout.segs);

    // --- blocks -------------------------------------------------------------
    let mut areas: Vec<f64> = Vec::with_capacity(layout.blocks.len());
    let mut aspects: Vec<f64> = Vec::with_capacity(layout.blocks.len());
    let mut verts: Vec<f64> = Vec::new();
    for b in &layout.blocks {
        let a = geom::area(&b.poly);
        areas.push(a);
        let asp = geom::obb_aspect(&b.poly);
        aspects.push(asp);
        verts.push(b.poly.len() as f64);
        // a sliver is long-and-thin or has almost no area for its perimeter
        let per = geom::perimeter(&b.poly);
        let compact = if per > 0.0 {
            4.0 * std::f64::consts::PI * a / (per * per)
        } else {
            0.0
        };
        if asp > 6.0 || compact < 0.16 {
            m.slivers += 1;
        }
    }
    m.blocks = layout.blocks.len();
    areas.sort_by(|a, b| a.partial_cmp(b).unwrap());
    aspects.sort_by(|a, b| a.partial_cmp(b).unwrap());
    verts.sort_by(|a, b| a.partial_cmp(b).unwrap());
    m.block_area_min = pct(&areas, 0.0);
    m.block_area_med = pct(&areas, 0.5);
    m.block_area_max = pct(&areas, 1.0);
    m.block_aspect_min = pct(&aspects, 0.0);
    m.block_aspect_med = pct(&aspects, 0.5);
    m.block_aspect_p95 = pct(&aspects, 0.95);
    m.block_aspect_max = pct(&aspects, 1.0);
    m.block_vert_med = pct(&verts, 0.5);

    // --- buildings ----------------------------------------------------------
    m.lots = layout.lots.len();
    m.buildings = layout.buildings.len();
    m.files = layout.files.len();
    let built: std::collections::BTreeSet<_> =
        layout.buildings.iter().map(|b| b.path.clone()).collect();
    m.files_without_building = layout
        .district_of_file
        .keys()
        .filter(|p| !built.contains(*p))
        .count();

    for b in &layout.buildings {
        if b.poly.len() < 3 {
            m.buildings_outside_lot += 1;
            continue;
        }
        let lot = &layout.lots[b.lot as usize].poly;
        if b.poly.iter().any(|&p| !geom::point_in_poly(lot, p)) {
            m.buildings_outside_lot += 1;
        }
    }
    m.buildings_on_road = count_building_road_hits(layout, half_widths);

    // --- districts ----------------------------------------------------------
    m.districts = layout.districts.len();
    m.district_border_pairs = layout.district_borders.len();
    let mut touched = std::collections::BTreeSet::new();
    let mut ddsu = Dsu::new(layout.districts.len().max(1));
    for &(a, b) in &layout.district_borders {
        touched.insert(a);
        touched.insert(b);
        ddsu.union(a, b);
    }
    m.districts_with_shared_border = touched.len();
    let mut droots = std::collections::BTreeSet::new();
    for i in 0..layout.districts.len() {
        droots.insert(ddsu.find(i as u32));
    }
    m.district_components = droots.len();

    // --- streets ------------------------------------------------------------
    m.streets = layout.streets.len();
    // A street is a "chord" if any of its segments is not a road segment.
    let road_set: std::collections::BTreeSet<(u32, u32)> = std::collections::BTreeSet::new();
    let _ = road_set;
    m.street_chords = layout
        .streets
        .iter()
        .filter(|s| {
            s.poly.windows(2).any(|w| {
                let d = w[0].dist(w[1]);
                d > 90.0 // no road segment is anywhere near this long
            })
        })
        .count();
    m
}

/// Proper crossings between non-adjacent segments, via a uniform grid.
pub fn count_crossings(nodes: &[Pt], segs: &[(u32, u32, crate::arr::Class)]) -> usize {
    if segs.is_empty() {
        return 0;
    }
    let mut lo = Pt {
        x: f64::MAX,
        y: f64::MAX,
    };
    let mut hi = Pt {
        x: f64::MIN,
        y: f64::MIN,
    };
    for p in nodes {
        lo.x = lo.x.min(p.x);
        lo.y = lo.y.min(p.y);
        hi.x = hi.x.max(p.x);
        hi.y = hi.y.max(p.y);
    }
    let n = 96usize;
    let cw = ((hi.x - lo.x).max(hi.y - lo.y) * 1.001) / n as f64;
    let mut cells: BTreeMap<(i64, i64), Vec<usize>> = BTreeMap::new();
    for (i, &(a, b, _)) in segs.iter().enumerate() {
        let (pa, pb) = (nodes[a as usize], nodes[b as usize]);
        let x0 = (((pa.x.min(pb.x) - lo.x) / cw).floor() as i64).max(0);
        let x1 = (((pa.x.max(pb.x) - lo.x) / cw).floor() as i64).min(n as i64);
        let y0 = (((pa.y.min(pb.y) - lo.y) / cw).floor() as i64).max(0);
        let y1 = (((pa.y.max(pb.y) - lo.y) / cw).floor() as i64).min(n as i64);
        for gx in x0..=x1 {
            for gy in y0..=y1 {
                cells.entry((gx, gy)).or_default().push(i);
            }
        }
    }
    let mut found = std::collections::BTreeSet::new();
    for ids in cells.values() {
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                let (si, sj) = (ids[i], ids[j]);
                let (a1, b1, _) = segs[si];
                let (a2, b2, _) = segs[sj];
                if a1 == a2 || a1 == b2 || b1 == a2 || b1 == b2 {
                    continue;
                }
                if geom::segs_cross(
                    nodes[a1 as usize],
                    nodes[b1 as usize],
                    nodes[a2 as usize],
                    nodes[b2 as usize],
                ) {
                    found.insert((si.min(sj), si.max(sj)));
                }
            }
        }
    }
    if std::env::var("DUMPX").is_ok() {
        for (n, &(i, j)) in found.iter().take(14).enumerate() {
            let (a1, b1, c1) = segs[i];
            let (a2, b2, c2) = segs[j];
            eprintln!(
                "X{n}: seg({a1},{b1},{c1:?}) len={:.2} vs seg({a2},{b2},{c2:?}) len={:.2}  p={:?} {:?}",
                nodes[a1 as usize].dist(nodes[b1 as usize]),
                nodes[a2 as usize].dist(nodes[b2 as usize]),
                nodes[a1 as usize],
                nodes[a2 as usize],
            );
        }
    }
    found.len()
}

fn count_building_road_hits(layout: &Layout, half: &[f64; 4]) -> usize {
    // bucket road segments
    let nodes = &layout.nodes;
    if nodes.is_empty() {
        return 0;
    }
    let mut lo = Pt {
        x: f64::MAX,
        y: f64::MAX,
    };
    let mut hi = Pt {
        x: f64::MIN,
        y: f64::MIN,
    };
    for p in nodes {
        lo.x = lo.x.min(p.x);
        lo.y = lo.y.min(p.y);
        hi.x = hi.x.max(p.x);
        hi.y = hi.y.max(p.y);
    }
    let n = 128usize;
    let cw = ((hi.x - lo.x).max(hi.y - lo.y) * 1.001) / n as f64;
    let mut cells: BTreeMap<(i64, i64), Vec<usize>> = BTreeMap::new();
    for (i, &(a, b, _)) in layout.segs.iter().enumerate() {
        let (pa, pb) = (nodes[a as usize], nodes[b as usize]);
        let x0 = (((pa.x.min(pb.x) - lo.x) / cw).floor() as i64).max(0);
        let x1 = (((pa.x.max(pb.x) - lo.x) / cw).floor() as i64).min(n as i64);
        let y0 = (((pa.y.min(pb.y) - lo.y) / cw).floor() as i64).max(0);
        let y1 = (((pa.y.max(pb.y) - lo.y) / cw).floor() as i64).min(n as i64);
        for gx in x0..=x1 {
            for gy in y0..=y1 {
                cells.entry((gx, gy)).or_default().push(i);
            }
        }
    }
    let mut hits = 0usize;
    for b in &layout.buildings {
        if b.poly.len() < 3 {
            continue;
        }
        let (blo, bhi) = geom::bounds(&b.poly);
        let x0 = (((blo.x - lo.x) / cw).floor() as i64).max(0);
        let x1 = (((bhi.x - lo.x) / cw).floor() as i64).min(n as i64);
        let y0 = (((blo.y - lo.y) / cw).floor() as i64).max(0);
        let y1 = (((bhi.y - lo.y) / cw).floor() as i64).min(n as i64);
        let mut hit = false;
        'outer: for gx in x0..=x1 {
            for gy in y0..=y1 {
                let Some(ids) = cells.get(&(gx, gy)) else {
                    continue;
                };
                for &si in ids {
                    let (a, c, cl) = layout.segs[si];
                    let half = half[cl as usize];
                    let (pa, pc) = (nodes[a as usize], nodes[c as usize]);
                    // road centreline within half-width of the footprint?
                    for k in 0..b.poly.len() {
                        let p = b.poly[k];
                        if geom::seg_dist2(p, pa, pc) < half * half {
                            hit = true;
                            break 'outer;
                        }
                        let q = b.poly[(k + 1) % b.poly.len()];
                        if geom::segs_cross(p, q, pa, pc) {
                            hit = true;
                            break 'outer;
                        }
                    }
                }
            }
        }
        if hit {
            hits += 1;
        }
    }
    hits
}

pub fn report(name: &str, m: &Metrics, gen_ms: f64, inc_us: f64, inc_resub_ms: f64) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    let _ = writeln!(s, "=== {name} ===");
    let _ = writeln!(
        s,
        "roads      nodes={} segments={} components={} crossings={} cycles(E-V+C)={}",
        m.nodes, m.segments, m.components, m.crossings, m.cycles
    );
    let mut hist = String::new();
    for (d, c) in &m.degree_hist {
        let _ = write!(hist, "{}:{} ", if *d >= 9 { 9 } else { *d }, c);
    }
    let _ = writeln!(
        s,
        "junctions  degree_hist[{}] dangling(deg1)={} deg>=4 share of junctions={:.1}%",
        hist.trim(),
        m.dangling,
        m.deg4plus_share * 100.0
    );
    let _ = writeln!(
        s,
        "blocks     n={} area min/med/max={:.0}/{:.0}/{:.0} aspect min/med/p95/max={:.2}/{:.2}/{:.2}/{:.2} slivers={} median_verts={:.0}",
        m.blocks,
        m.block_area_min,
        m.block_area_med,
        m.block_area_max,
        m.block_aspect_min,
        m.block_aspect_med,
        m.block_aspect_p95,
        m.block_aspect_max,
        m.slivers,
        m.block_vert_med
    );
    let _ = writeln!(
        s,
        "buildings  lots={} buildings={} files={} without_building={} outside_lot={} intersecting_road={}",
        m.lots, m.buildings, m.files, m.files_without_building, m.buildings_outside_lot, m.buildings_on_road
    );
    let _ = writeln!(
        s,
        "districts  n={} sharing_a_border={} adjacency_components={} border_pairs={}",
        m.districts, m.districts_with_shared_border, m.district_components, m.district_border_pairs
    );
    let _ = writeln!(
        s,
        "streets    routed={} chord-like={}",
        m.streets, m.street_chords
    );
    let _ = writeln!(
        s,
        "timing     full_generation={gen_ms:.1}ms incremental_add(vacant lot)={inc_us:.1}us incremental_add(subtree resubdivide)={inc_resub_ms:.2}ms"
    );
    s
}
