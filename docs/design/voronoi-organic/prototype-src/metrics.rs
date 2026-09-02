//! Measured structural metrics. Everything here is computed from the produced
//! geometry, never asserted.

use std::collections::{BTreeMap, BTreeSet};

use polis_layout::determinism as det;

use crate::city::{City, RoadClass, NOLABEL};
use crate::geom::{self, Pt};

pub struct Metrics {
    pub lines: Vec<String>,
    pub deg_hist: BTreeMap<u32, u32>,
    pub components: u32,
    pub crossings: u32,
    pub cycles: i64,
    pub nodes: u32,
    pub segments: u32,
}

fn grid_key(p: Pt, cell: f64) -> (i64, i64) {
    ((p.x / cell).floor() as i64, (p.y / cell).floor() as i64)
}

fn stat(v: &mut Vec<f64>) -> (f64, f64, f64) {
    if v.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    det::sort_by_f64_key(v, |x| *x);
    (v[0], v[v.len() / 2], v[v.len() - 1])
}

pub fn measure(c: &City, gen_ms: f64, inc_us: f64, label: &str) -> Metrics {
    let mut out: Vec<String> = Vec::new();
    out.push(format!("=== {label} ==="));
    out.push(format!(
        "files {}  districts {}  blocks {}  buildings {}",
        c.repo.files.len(),
        c.districts.len(),
        c.blocks.len(),
        c.buildings.len()
    ));

    // ---- road graph -------------------------------------------------
    let alive: Vec<usize> = (0..c.chains.len()).filter(|&i| c.chains[i].alive).collect();
    let mut used_nodes: BTreeSet<u32> = BTreeSet::new();
    let mut deg: BTreeMap<u32, u32> = BTreeMap::new();
    for &i in &alive {
        let ch = &c.chains[i];
        used_nodes.insert(ch.a);
        used_nodes.insert(ch.b);
        *deg.entry(ch.a).or_insert(0) += 1;
        *deg.entry(ch.b).or_insert(0) += 1;
    }
    let v = used_nodes.len() as i64;
    let e = alive.len() as i64;

    // components over the junction graph
    let mut parent: BTreeMap<u32, u32> = used_nodes.iter().map(|&n| (n, n)).collect();
    fn find(p: &mut BTreeMap<u32, u32>, x: u32) -> u32 {
        let mut r = x;
        while p[&r] != r {
            r = p[&r];
        }
        let mut cur = x;
        while p[&cur] != cur {
            let n = p[&cur];
            p.insert(cur, r);
            cur = n;
        }
        r
    }
    for &i in &alive {
        let ch = &c.chains[i];
        let (a, b) = (find(&mut parent, ch.a), find(&mut parent, ch.b));
        if a != b {
            parent.insert(a.max(b), a.min(b));
        }
    }
    let comps: BTreeSet<u32> = used_nodes.iter().map(|&n| find(&mut parent, n)).collect();
    let ncomp = comps.len() as i64;

    let mut hist: BTreeMap<u32, u32> = BTreeMap::new();
    for n in &used_nodes {
        let d = deg[n];
        *hist.entry(d.min(9)).or_insert(0) += 1;
    }
    let dangling = hist.get(&1).copied().unwrap_or(0);

    // planarity: proper crossings between road polyline segments
    let mut segs: Vec<(Pt, Pt, usize)> = Vec::new();
    for &i in &alive {
        let p = &c.chains[i].pts;
        for k in 0..p.len().saturating_sub(1) {
            segs.push((p[k], p[k + 1], i));
        }
    }
    let cell = 12.0;
    let mut grid: BTreeMap<(i64, i64), Vec<u32>> = BTreeMap::new();
    for (si, s) in segs.iter().enumerate() {
        let (a, b) = (grid_key(s.0, cell), grid_key(s.1, cell));
        for gx in a.0.min(b.0)..=a.0.max(b.0) {
            for gy in a.1.min(b.1)..=a.1.max(b.1) {
                grid.entry((gx, gy)).or_default().push(si as u32);
            }
        }
    }
    let mut crossings: BTreeSet<(u32, u32)> = BTreeSet::new();
    for list in grid.values() {
        for x in 0..list.len() {
            for y in (x + 1)..list.len() {
                let (i, j) = (list[x] as usize, list[y] as usize);
                if segs[i].2 == segs[j].2 {
                    continue;
                }
                if geom::segments_cross(segs[i].0, segs[i].1, segs[j].0, segs[j].1) {
                    crossings.insert((list[x].min(list[y]), list[x].max(list[y])));
                }
            }
        }
    }

    let cycles = e - v + ncomp;
    out.push(format!(
        "ROAD GRAPH  nodes(junctions) {v}  segments(chains) {e}  polyline-segments {}",
        segs.len()
    ));
    out.push(format!(
        "ROAD GRAPH  connected components {ncomp}   cycles (E-V+C) {cycles}   crossings-without-a-node {}",
        crossings.len()
    ));
    let hstr: Vec<String> = hist.iter().map(|(d, n)| format!("{d}:{n}")).collect();
    out.push(format!(
        "ROAD GRAPH  degree histogram  {}   dangling(deg1) {dangling}",
        hstr.join("  ")
    ));
    let hi = hist
        .iter()
        .filter(|(d, _)| **d >= 4)
        .map(|(_, n)| *n)
        .sum::<u32>();
    out.push(format!(
        "ROAD GRAPH  4-way-or-more junctions {hi} ({:.1}% of nodes)",
        100.0 * f64::from(hi) / (v.max(1) as f64)
    ));
    let mut cls: BTreeMap<&str, u32> = BTreeMap::new();
    for &i in &alive {
        let k = match c.chains[i].class {
            RoadClass::Arterial => "arterial",
            RoadClass::Secondary => "secondary",
            RoadClass::Perimeter => "perimeter",
        };
        *cls.entry(k).or_insert(0) += 1;
    }
    out.push(format!(
        "ROAD GRAPH  classes  {}",
        cls.iter()
            .map(|(k, n)| format!("{k} {n}"))
            .collect::<Vec<_>>()
            .join("  ")
    ));

    // ---- blocks ------------------------------------------------------
    let mut areas: Vec<f64> = Vec::new();
    let mut ars: Vec<f64> = Vec::new();
    let mut slivers = 0u32;
    let mut noface = 0u32;
    for b in &c.blocks {
        if b.poly.len() < 3 {
            noface += 1;
            continue;
        }
        let a = geom::area(&b.poly);
        let ar = geom::aspect_ratio(&b.poly);
        areas.push(a);
        ars.push(ar);
        if ar > 6.0 || a < 12.0 {
            slivers += 1;
        }
    }
    let zero_px = c.blocks.iter().filter(|b| b.px == 0).count();
    let empty_blocks = c.blocks.iter().filter(|b| b.files.is_empty()).count();
    let no_core = c.blocks.iter().filter(|b| b.buildable.len() < 3).count();
    let (amin, amed, amax) = stat(&mut areas.clone());
    let (rmin, rmed, rmax) = stat(&mut ars.clone());
    out.push(format!(
        "BLOCKS  count {}  no-face {noface} (of which zero-pixel {zero_px})  area min/med/max {amin:.1} / {amed:.1} / {amax:.1}",
        areas.len()
    ));
    out.push(format!(
        "BLOCKS  unbuilt(open ground) {empty_blocks}  no-buildable-core {no_core}"
    ));
    out.push(format!(
        "BLOCKS  aspect min/med/max {rmin:.2} / {rmed:.2} / {rmax:.2}   slivers(ar>6 or area<12) {slivers}"
    ));

    // ---- buildings ---------------------------------------------------
    let placed: BTreeSet<u32> = c.buildings.iter().map(|b| b.file).collect();
    let missing = c.repo.files.len() as u32 - placed.len() as u32;
    let mut outside = 0u32;
    let mut on_road = 0u32;
    for bl in &c.buildings {
        let blk = &c.blocks[bl.block as usize];
        if let Some(lot) = blk.lots.get(bl.lot_idx as usize) {
            if !bl.poly.iter().all(|p| geom::contains(lot, *p)) {
                outside += 1;
            }
        } else {
            outside += 1;
        }
    }
    // road proximity: any building vertex within the road's half width
    for bl in &c.buildings {
        let mut hit = false;
        'outer: for p in &bl.poly {
            let g = grid_key(*p, cell);
            for gx in g.0 - 1..=g.0 + 1 {
                for gy in g.1 - 1..=g.1 + 1 {
                    if let Some(list) = grid.get(&(gx, gy)) {
                        for &si in list {
                            let s = &segs[si as usize];
                            let hw = c.chains[s.2].class.half_width();
                            if point_seg_dist(*p, s.0, s.1) < hw {
                                hit = true;
                                break 'outer;
                            }
                        }
                    }
                }
            }
        }
        if hit {
            on_road += 1;
        }
    }
    out.push(format!(
        "BUILDINGS  count {}  files-with-no-building {missing}  outside-their-lot {outside}  intersecting-a-road {on_road}",
        c.buildings.len()
    ));

    // ---- districts ---------------------------------------------------
    let mut border: BTreeSet<(u32, u32)> = BTreeSet::new();
    for &i in &alive {
        let ch = &c.chains[i];
        if ch.left == NOLABEL || ch.right == NOLABEL {
            continue;
        }
        let (dl, dr) = (
            c.blocks[ch.left as usize].district,
            c.blocks[ch.right as usize].district,
        );
        if dl != dr {
            border.insert((dl.min(dr), dl.max(dr)));
        }
    }
    let mut has_border = vec![false; c.districts.len()];
    for (a, b) in &border {
        has_border[*a as usize] = true;
        has_border[*b as usize] = true;
    }
    let shared = has_border.iter().filter(|b| **b).count();
    // district adjacency connectivity
    let mut dp: Vec<u32> = (0..c.districts.len() as u32).collect();
    fn find2(p: &mut Vec<u32>, x: u32) -> u32 {
        let mut r = x;
        while p[r as usize] != r {
            r = p[r as usize];
        }
        r
    }
    for (a, b) in &border {
        let (ra, rb) = (find2(&mut dp, *a), find2(&mut dp, *b));
        if ra != rb {
            dp[ra.max(rb) as usize] = ra.min(rb);
        }
    }
    let dcomp: BTreeSet<u32> = (0..c.districts.len() as u32)
        .map(|i| find2(&mut dp, i))
        .collect();
    let empty = c.districts.iter().filter(|d| d.px == 0).count();
    out.push(format!(
        "DISTRICTS  count {}  sharing-a-border {shared}  adjacency-components {}  empty-cells {empty}",
        c.districts.len(),
        dcomp.len()
    ));
    let mut dpx: Vec<f64> = c.districts.iter().map(|d| f64::from(d.px)).collect();
    let (pmin, pmed, pmax) = stat(&mut dpx);
    out.push(format!(
        "DISTRICTS  cell pixels min/med/max {pmin:.0} / {pmed:.0} / {pmax:.0}  (settlement K={:.0}, grown {}x)",
        c.k_claim, c.k_iters
    ));

    out.push(format!(
        "TIMING  full generation {gen_ms:.0} ms   incremental single-file add {inc_us:.0} us"
    ));
    for n in &c.notes {
        out.push(format!("NOTE  {n}"));
    }

    Metrics {
        lines: out,
        deg_hist: hist,
        components: ncomp as u32,
        crossings: crossings.len() as u32,
        cycles,
        nodes: v as u32,
        segments: e as u32,
    }
}

fn point_seg_dist(p: Pt, a: Pt, b: Pt) -> f64 {
    let ab = geom::sub(b, a);
    let l2 = geom::dot(ab, ab);
    if l2 < 1e-12 {
        return geom::dist(p, a);
    }
    let t = (geom::dot(geom::sub(p, a), ab) / l2).clamp(0.0, 1.0);
    geom::dist(p, geom::add(a, geom::scale(ab, t)))
}
