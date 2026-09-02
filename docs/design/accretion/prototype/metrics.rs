//! Structural measurement. Every number in the writeup is produced here.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::accrete::Settlement;
use crate::city::City;
use crate::geom::*;
use crate::graph::Graph;

#[derive(Default)]
pub struct Metrics {
    pub nodes: usize,
    pub edges: usize,
    pub components: usize,
    pub crossings: usize,
    pub cycles: i64,
    pub deg: BTreeMap<usize, usize>,
    pub dangling: usize,
    pub junction_nodes: usize,

    pub blocks: usize,
    pub open_blocks: usize,
    pub block_area: (f64, f64, f64),
    pub block_aspect: (f64, f64, f64),
    pub slivers: usize,

    pub lots: usize,
    pub buildings: usize,
    pub files: usize,
    pub files_without_building: usize,
    pub buildings_outside_lot: usize,
    pub buildings_on_road: usize,
    pub on_road_fallback: usize,

    pub districts: usize,
    pub districts_with_shared_border: usize,
    pub districts_fragmented: usize,
    pub district_graph_components: usize,

    pub gen_ms: f64,
    pub incr_ms_median: f64,
    pub incr_ms_p95: f64,
    pub incr_moved_nodes_median: f64,
    pub streets: usize,
    pub street_chord_violations: usize,
}

fn stats(mut v: Vec<f64>) -> (f64, f64, f64) {
    if v.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    (v[0], v[v.len() / 2], v[v.len() - 1])
}

struct SegGrid {
    cell: f64,
    b: BTreeMap<(i32, i32), Vec<u32>>,
}
impl SegGrid {
    fn new(cell: f64) -> Self {
        Self {
            cell,
            b: BTreeMap::new(),
        }
    }
    fn ins(&mut self, a: P, c: P, id: u32) {
        let x0 = (a[0].min(c[0]) / self.cell).floor() as i32;
        let x1 = (a[0].max(c[0]) / self.cell).floor() as i32;
        let y0 = (a[1].min(c[1]) / self.cell).floor() as i32;
        let y1 = (a[1].max(c[1]) / self.cell).floor() as i32;
        for y in y0..=y1 {
            for x in x0..=x1 {
                self.b.entry((x, y)).or_default().push(id);
            }
        }
    }
}

pub fn measure(s: &Settlement, g: &Graph, c: &City, sep: f64) -> Metrics {
    let mut m = Metrics::default();
    m.nodes = g.nodes.len();
    m.edges = g.edges.len();

    // --- connectivity -----------------------------------------------------
    let n = g.nodes.len();
    let mut parent: Vec<u32> = (0..n as u32).collect();
    fn find(p: &mut Vec<u32>, mut x: u32) -> u32 {
        while p[x as usize] != x {
            p[x as usize] = p[p[x as usize] as usize];
            x = p[x as usize];
        }
        x
    }
    for &(a, b) in &g.edges {
        let ra = find(&mut parent, a);
        let rb = find(&mut parent, b);
        if ra != rb {
            parent[ra.max(rb) as usize] = ra.min(rb);
        }
    }
    let mut roots = std::collections::BTreeSet::new();
    for i in 0..n as u32 {
        roots.insert(find(&mut parent, i));
    }
    m.components = roots.len();
    m.cycles = m.edges as i64 - m.nodes as i64 + m.components as i64;

    for i in 0..n {
        let d = g.degree(i);
        *m.deg.entry(d).or_insert(0) += 1;
        if d == 1 {
            m.dangling += 1;
        }
        if d != 2 {
            m.junction_nodes += 1;
        }
    }

    // --- planarity: a crossing without a node ----------------------------
    let mut segs: Vec<(P, P)> = Vec::new();
    for e in 0..g.edges.len() {
        let poly = &g.curve[e];
        for k in 0..poly.len() - 1 {
            segs.push((poly[k], poly[k + 1]));
        }
    }
    let avg: f64 = if segs.is_empty() {
        1.0
    } else {
        segs.iter().map(|(a, b)| dist(*a, *b)).sum::<f64>() / segs.len() as f64
    };
    let mut grid = SegGrid::new(avg.max(1e-6) * 2.0);
    for (i, (a, b)) in segs.iter().enumerate() {
        grid.ins(*a, *b, i as u32);
    }
    let mut seen: std::collections::BTreeSet<(u32, u32)> = std::collections::BTreeSet::new();
    for list in grid.b.values() {
        for i in 0..list.len() {
            for j in i + 1..list.len() {
                let (a, b) = (list[i].min(list[j]), list[i].max(list[j]));
                if a == b || !seen.insert((a, b)) {
                    continue;
                }
                let (p0, p1) = segs[a as usize];
                let (q0, q1) = segs[b as usize];
                if segments_properly_cross(p0, p1, q0, q1) {
                    m.crossings += 1;
                }
            }
        }
    }

    // --- blocks ----------------------------------------------------------
    m.blocks = c.blocks.len();
    m.open_blocks = c.blocks.iter().filter(|b| b.open).count();
    let areas: Vec<f64> = c.blocks.iter().map(|b| area(&b.ring)).collect();
    let aspects: Vec<f64> = c.blocks.iter().map(|b| aspect_ratio(&b.ring)).collect();
    m.block_area = stats(areas.clone());
    m.block_aspect = stats(aspects.clone());
    let med_a = m.block_area.1.max(1e-12);
    m.slivers = c
        .blocks
        .iter()
        .filter(|b| {
            let a = area(&b.ring);
            aspect_ratio(&b.ring) > 6.0 || a < med_a * 0.06
        })
        .count();

    // --- lots and buildings ----------------------------------------------
    m.lots = c.lots.len();
    m.buildings = c.buildings.len();
    m.files = s.files.len();
    let mut has: Vec<bool> = vec![false; s.files.len()];
    for b in &c.buildings {
        has[b.file as usize] = true;
    }
    m.files_without_building = has.iter().filter(|x| !**x).count();
    for b in &c.buildings {
        let lot = &c.lots[b.lot as usize];
        if !b.ring.iter().all(|p| contains(&lot.ring, *p)) {
            m.buildings_outside_lot += 1;
        }
    }
    // Buildings vs roads: the drawn lane corridor.
    let corridor = sep * crate::city::ROAD_HALF;
    let mut rgrid = SegGrid::new(avg.max(1e-6) * 2.0);
    for (i, (a, b)) in segs.iter().enumerate() {
        rgrid.ins(*a, *b, i as u32);
    }
    for b in &c.buildings {
        let mut hit = false;
        'v: for p in &b.ring {
            let kx = (p[0] / rgrid.cell).floor() as i32;
            let ky = (p[1] / rgrid.cell).floor() as i32;
            for dy in -1..=1 {
                for dx in -1..=1 {
                    if let Some(l) = rgrid.b.get(&(kx + dx, ky + dy)) {
                        for &si in l {
                            let (a0, a1) = segs[si as usize];
                            if dist_to_seg(*p, a0, a1) < corridor {
                                hit = true;
                                break 'v;
                            }
                        }
                    }
                }
            }
        }
        if hit {
            m.buildings_on_road += 1;
            if b.fallback {
                m.on_road_fallback += 1;
            }
        }
    }

    // --- districts --------------------------------------------------------
    let placed: std::collections::BTreeSet<u32> = c
        .blocks
        .iter()
        .filter_map(|b| b.district)
        .collect();
    m.districts = placed.len();
    // Block adjacency across shared edges.
    let mut edge_blocks: BTreeMap<usize, Vec<u32>> = BTreeMap::new();
    for (bi, b) in c.blocks.iter().enumerate() {
        for &h in &b.half_edges {
            edge_blocks.entry(h / 2).or_default().push(bi as u32);
        }
    }
    let mut shares: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
    let mut dadj: BTreeMap<u32, std::collections::BTreeSet<u32>> = BTreeMap::new();
    let nb = c.blocks.len();
    let mut bp: Vec<u32> = (0..nb as u32).collect();
    let mut same_district_union: Vec<u32> = (0..nb as u32).collect();
    for bs in edge_blocks.values() {
        if bs.len() != 2 {
            continue;
        }
        let (x, y) = (bs[0], bs[1]);
        let rx = find(&mut bp, x);
        let ry = find(&mut bp, y);
        if rx != ry {
            bp[rx.max(ry) as usize] = rx.min(ry);
        }
        let (dx, dy) = (c.blocks[x as usize].district, c.blocks[y as usize].district);
        if let (Some(a), Some(b2)) = (dx, dy) {
            if a != b2 {
                shares.insert(a);
                shares.insert(b2);
                dadj.entry(a).or_default().insert(b2);
                dadj.entry(b2).or_default().insert(a);
            } else {
                let ra = find(&mut same_district_union, x);
                let rb = find(&mut same_district_union, y);
                if ra != rb {
                    same_district_union[ra.max(rb) as usize] = ra.min(rb);
                }
            }
        }
    }
    m.districts_with_shared_border = shares.len();
    // A district is fragmented if its blocks fall into >1 same-district
    // component.
    let mut comp_of_district: BTreeMap<u32, std::collections::BTreeSet<u32>> = BTreeMap::new();
    for (bi, b) in c.blocks.iter().enumerate() {
        if let Some(d) = b.district {
            let r = find(&mut same_district_union, bi as u32);
            comp_of_district.entry(d).or_default().insert(r);
        }
    }
    m.districts_fragmented = comp_of_district.values().filter(|s| s.len() > 1).count();
    // Is the district-adjacency graph itself one piece?
    {
        let ds: Vec<u32> = placed.iter().copied().collect();
        let pos: BTreeMap<u32, usize> = ds.iter().enumerate().map(|(i, d)| (*d, i)).collect();
        let mut dp: Vec<u32> = (0..ds.len() as u32).collect();
        for (a, set) in &dadj {
            for b in set {
                if let (Some(&i), Some(&j)) = (pos.get(a), pos.get(b)) {
                    let ri = find(&mut dp, i as u32);
                    let rj = find(&mut dp, j as u32);
                    if ri != rj {
                        dp[ri.max(rj) as usize] = ri.min(rj);
                    }
                }
            }
        }
        let mut r = std::collections::BTreeSet::new();
        for i in 0..ds.len() as u32 {
            r.insert(find(&mut dp, i));
        }
        m.district_graph_components = r.len();
    }

    // --- streets ----------------------------------------------------------
    m.streets = c.streets.len();
    // A street must follow the roads. Any street segment whose midpoint is far
    // from a road centre-line is a chord across open ground.
    for st in &c.streets {
        let mut bad = false;
        for k in 0..st.polyline.len().saturating_sub(1) {
            let mid = mul(add(st.polyline[k], st.polyline[k + 1]), 0.5);
            let kx = (mid[0] / rgrid.cell).floor() as i32;
            let ky = (mid[1] / rgrid.cell).floor() as i32;
            let mut best = f64::INFINITY;
            for dy in -1..=1 {
                for dx in -1..=1 {
                    if let Some(l) = rgrid.b.get(&(kx + dx, ky + dy)) {
                        for &si in l {
                            let (a0, a1) = segs[si as usize];
                            best = best.min(dist_to_seg(mid, a0, a1));
                        }
                    }
                }
            }
            if best > sep * 0.02 {
                bad = true;
                break;
            }
        }
        if bad {
            m.street_chord_violations += 1;
        }
    }
    m
}

impl Metrics {
    pub fn report(&self, title: &str) -> String {
        let mut o = String::new();
        let _ = writeln!(o, "===== {title} =====");
        let _ = writeln!(
            o,
            "ROAD GRAPH  nodes={} segments={} components={} cycles(E-V+C)={} crossings_without_node={}",
            self.nodes, self.edges, self.components, self.cycles, self.crossings
        );
        let mut degs = String::new();
        let tot: usize = self.deg.values().sum();
        let mut buckets: std::collections::BTreeMap<usize, usize> =
            std::collections::BTreeMap::new();
        for (d, c) in &self.deg {
            *buckets.entry((*d).min(5)).or_insert(0) += *c;
        }
        for (d, c) in &buckets {
            let label = if *d >= 5 { "5+".to_string() } else { d.to_string() };
            let _ = write!(
                degs,
                "{}:{} ({:.1}%)  ",
                label,
                c,
                100.0 * *c as f64 / tot.max(1) as f64
            );
        }
        let five_plus: usize = self.deg.iter().filter(|(d, _)| **d >= 5).map(|(_, c)| *c).sum();
        let four: usize = self.deg.get(&4).copied().unwrap_or(0);
        let _ = writeln!(o, "DEGREE HIST {degs}");
        let _ = writeln!(
            o,
            "            junction nodes (deg!=2)={}  4-way={}  5+-way={}  4-and-5+ share of junctions={:.1}%  dangling(deg1)={}",
            self.junction_nodes,
            four,
            five_plus,
            100.0 * (four + five_plus) as f64 / self.junction_nodes.max(1) as f64,
            self.dangling
        );
        let _ = writeln!(
            o,
            "BLOCKS      count={} (open/plaza={})  area min/med/max={:.4}/{:.4}/{:.4}  aspect min/med/max={:.2}/{:.2}/{:.2}  slivers(aspect>6 or area<6% median)={}",
            self.blocks,
            self.open_blocks,
            self.block_area.0,
            self.block_area.1,
            self.block_area.2,
            self.block_aspect.0,
            self.block_aspect.1,
            self.block_aspect.2,
            self.slivers
        );
        let _ = writeln!(
            o,
            "BUILDINGS   files={} lots={} buildings={}  files_without_building={}  outside_their_lot={}  intersecting_a_road={}",
            self.files,
            self.lots,
            self.buildings,
            self.files_without_building,
            self.buildings_outside_lot,
            self.buildings_on_road
        );
        let _ = writeln!(o, "            (of those, {} used the degenerate-lot fallback)", self.on_road_fallback);
        let _ = writeln!(
            o,
            "DISTRICTS   count={}  sharing_a_border={}  fragmented(non-contiguous)={}  district-adjacency components={}",
            self.districts,
            self.districts_with_shared_border,
            self.districts_fragmented,
            self.district_graph_components
        );
        let _ = writeln!(
            o,
            "STREETS     routed={}  not_following_roads={}",
            self.streets, self.street_chord_violations
        );
        let _ = writeln!(
            o,
            "TIMING      full_generation={:.1} ms  incremental_add median={:.3} ms p95={:.3} ms  road nodes moved per add (median)={:.1}",
            self.gen_ms, self.incr_ms_median, self.incr_ms_p95, self.incr_moved_nodes_median
        );
        o
    }
}
