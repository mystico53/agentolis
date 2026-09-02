//! voronoi-organic: the city as an organic partition of the plane.
//!
//! Pipeline (PRD §7.2 ordering, non-negotiable):
//!   terrain field -> district partition -> block partition -> road graph
//!   (= the borders of that partition) -> blocks (= its faces) -> lots -> buildings
//!
//! The road network is *derived from* the partition rather than grown into
//! empty space, so it is connected, planar and full of closed faces by
//! construction. Nothing here can produce an island or a dangling stub.

use std::collections::{BTreeMap, BTreeSet, BinaryHeap, VecDeque};

use polis_layout::determinism as det;

use crate::geom::{self, Pt};

pub const WORLD: f64 = 1000.0;
pub const RES: usize = 1900;
pub const CELL: f64 = WORLD / RES as f64;
pub const NOLABEL: u32 = u32::MAX;
/// Smallest block the raster can carry a clean polygon for, in pixels.
pub const MIN_BLOCK_CELLS: usize = 700;
/// The city-block grain: how big a block wants to be, in raster cells.
pub const TARGET_BLOCK_CELLS: f64 = 1500.0;
/// A quarter smaller than this cannot be read, labelled or navigated to, so
/// the subtree below it is drawn as one district instead.
pub const MIN_DISTRICT_CELLS: usize = 1700;
/// A child that comes out below this is left as a leaf rather than split.
pub const MIN_TINY_CELLS: usize = 850;

const SEED_WARP_X: u64 = 0x5741_5250_5800_0001;
const SEED_WARP_Y: u64 = 0x5741_5250_5900_0002;
const SEED_WARP2X: u64 = 0x5741_5250_5800_0003;
const SEED_WARP2Y: u64 = 0x5741_5250_5900_0004;
const SEED_COAST: u64 = 0x434F_4153_5400_0005;
const SEED_TOWN: u64 = 0x544F_574E_0000_0006;

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct FileRec {
    pub path: String,
    pub size: u64,
    /// Index of the commit that first added the file (PRD §7.1). `u32::MAX` for
    /// a file git has never seen.
    pub growth: u32,
}

#[derive(Clone, Debug, Default)]
pub struct Repo {
    pub files: Vec<FileRec>,
    /// Cross-district import edges, aggregated: (from district, to district, count).
    pub streets: Vec<(String, String, u32)>,
}

pub fn dir_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[..i],
        None => "",
    }
}

// ---------------------------------------------------------------------------
// The augmented directory tree
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Dir,
    District,
}

#[derive(Clone, Debug)]
pub struct TNode {
    pub key: String,
    pub kind: Kind,
    pub parent: u32,
    pub children: Vec<u32>,
    pub files: Vec<u32>,
    pub count: u32,
    pub qcount: u32,
    pub age: u32,
    pub site: Pt,
    pub radius: f64,
    pub m: f64,
    pub depth: u32,
    /// Index into `City::districts`, for `Kind::District` only.
    pub district: u32,
}

/// Weight quantisation: adding one file to a large directory must not resize it
/// (PRD §7.7 — never move the ground while the operator is looking at it).
pub fn quantise_count(n: u32) -> u32 {
    if n <= 8 {
        return n;
    }
    let mut b = 8u32;
    while b < n {
        b = (b as f64 * 1.18).ceil() as u32;
    }
    b
}

// ---------------------------------------------------------------------------
// Roads, blocks, lots, buildings
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RoadClass {
    /// Border between two districts. Wide.
    Arterial,
    /// Border between two blocks of the same district. Narrow.
    Secondary,
    /// The city limit: the outer edge of the settled area.
    Perimeter,
}

impl RoadClass {
    pub fn half_width(self) -> f64 {
        match self {
            RoadClass::Arterial => 1.75,
            RoadClass::Secondary => 0.85,
            RoadClass::Perimeter => 1.15,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Chain {
    pub a: u32,
    pub b: u32,
    pub pts: Vec<Pt>,
    pub left: u32,
    pub right: u32,
    pub class: RoadClass,
    pub alive: bool,
}

#[derive(Clone, Debug)]
pub struct BlockData {
    pub district: u32,
    /// Rank within the district, 0 = nearest the district core (oldest files).
    pub rank: u32,
    /// Seed position in SCREEN space (ranking, rendering).
    pub site: Pt,
    /// Seed position in WARPED space — the space the partition is computed in.
    /// Mixing the two silently offsets every block by the warp amplitude.
    pub site_w: Pt,
    pub px: u32,
    pub poly: Vec<Pt>,
    pub edge_class: Vec<RoadClass>,
    pub buildable: Vec<Pt>,
    pub cap: u32,
    pub files: Vec<u32>,
    pub lots: Vec<Vec<Pt>>,
}

#[derive(Clone, Debug)]
pub struct Building {
    pub file: u32,
    pub block: u32,
    pub lot_idx: u32,
    pub poly: Vec<Pt>,
}

#[derive(Clone, Debug)]
pub struct DistrictData {
    pub node: u32,
    pub key: String,
    pub px: u32,
    pub centroid: Pt,
    pub age_rank: f64,
    pub blocks: Vec<u32>,
    /// Every file the quarter holds. For a collapsed subtree this is the whole
    /// subtree, not just one directory.
    pub files: Vec<u32>,
}

// ---------------------------------------------------------------------------
// City
// ---------------------------------------------------------------------------

pub struct City {
    pub repo: Repo,
    pub nodes: Vec<TNode>,
    pub districts: Vec<DistrictData>,
    pub blocks: Vec<BlockData>,
    pub buildings: Vec<Building>,
    pub chains: Vec<Chain>,
    pub jpos: Vec<Pt>,
    pub street_paths: Vec<(Vec<Pt>, u32)>,
    pub land: Vec<bool>,
    pub dlabel: Vec<u32>,
    pub blabel: Vec<u32>,
    pub k_claim: f64,
    pub k_iters: u32,
    pub size_median: f64,
    pub notes: Vec<String>,
}

struct Warp {
    wx: Vec<f32>,
    wy: Vec<f32>,
}

fn warp_point(x: f64, y: f64) -> (f64, f64) {
    const F1: f64 = 1.0 / 235.0;
    const A1: f64 = 52.0;
    const F2: f64 = 1.0 / 61.0;
    const A2: f64 = 9.5;
    let ux = det::fbm2_f64(SEED_WARP_X, x * F1, y * F1, 3);
    let uy = det::fbm2_f64(SEED_WARP_Y, x * F1, y * F1, 3);
    let vx = det::fbm2_f64(SEED_WARP2X, x * F2, y * F2, 2);
    let vy = det::fbm2_f64(SEED_WARP2Y, x * F2, y * F2, 2);
    (x + A1 * ux + A2 * vx, y + A1 * uy + A2 * vy)
}

fn is_land(wx: f64, wy: f64) -> bool {
    let dx = wx - WORLD * 0.5;
    let dy = wy - WORLD * 0.5;
    let d = (dx * dx + dy * dy).sqrt() / 486.0;
    let n = det::fbm2_f64(SEED_COAST, wx / 255.0, wy / 255.0, 4);
    1.0 - d * d * d + 0.40 * n > 0.0
}

pub fn generate(repo: Repo) -> City {
    let mut city = City {
        repo,
        nodes: Vec::new(),
        districts: Vec::new(),
        blocks: Vec::new(),
        buildings: Vec::new(),
        chains: Vec::new(),
        jpos: Vec::new(),
        street_paths: Vec::new(),
        land: Vec::new(),
        dlabel: Vec::new(),
        blabel: Vec::new(),
        k_claim: 0.0,
        k_iters: 0,
        size_median: 1.0,
        notes: Vec::new(),
    };
    city.build_tree();
    city.weigh();
    let warp = city.compute_terrain();
    city.pass_a(&warp);
    city.pass_b(&warp);
    city.extract_roads();
    city.assemble_faces();
    city.lots_and_buildings();
    city.route_streets();
    city
}

impl City {
    // -----------------------------------------------------------------
    // 1. Tree
    // -----------------------------------------------------------------
    fn build_tree(&mut self) {
        // Canonical order first: input permutation cannot reach the layout.
        self.repo.files.sort_by(|a, b| a.path.cmp(&b.path));
        self.repo.files.dedup_by(|a, b| a.path == b.path);

        let mut dirs: BTreeSet<String> = BTreeSet::new();
        dirs.insert(String::new());
        for f in &self.repo.files {
            let mut d = dir_of(&f.path).to_string();
            loop {
                dirs.insert(d.clone());
                match d.rfind('/') {
                    Some(i) => d.truncate(i),
                    None => {
                        break;
                    }
                }
            }
        }

        let mut id_of: BTreeMap<String, u32> = BTreeMap::new();
        for (i, d) in dirs.iter().enumerate() {
            id_of.insert(d.clone(), u32::try_from(i).unwrap());
            self.nodes.push(TNode {
                key: d.clone(),
                kind: Kind::Dir,
                parent: NOLABEL,
                children: Vec::new(),
                files: Vec::new(),
                count: 0,
                qcount: 0,
                age: u32::MAX,
                site: geom::pt(0.0, 0.0),
                radius: 0.0,
                m: 1.0,
                depth: 0,
                district: NOLABEL,
            });
        }
        for d in dirs.iter() {
            if d.is_empty() {
                continue;
            }
            let parent = match d.rfind('/') {
                Some(i) => d[..i].to_string(),
                None => String::new(),
            };
            let (me, pa) = (id_of[d], id_of[&parent]);
            self.nodes[me as usize].parent = pa;
            self.nodes[pa as usize].children.push(me);
        }

        // Files grouped by their own directory -> one District ("self") node per
        // directory that directly holds files. The self node goes FIRST among a
        // directory's children so it lands at the region's core: a directory's
        // own files are the heart of its quarter, subdirectories ring it.
        let mut by_dir: BTreeMap<String, Vec<u32>> = BTreeMap::new();
        for (i, f) in self.repo.files.iter().enumerate() {
            by_dir
                .entry(dir_of(&f.path).to_string())
                .or_default()
                .push(u32::try_from(i).unwrap());
        }
        for (d, files) in by_dir {
            let pa = id_of[&d];
            let key = if d.is_empty() {
                "/·".to_string()
            } else {
                format!("{d}/·")
            };
            let me = u32::try_from(self.nodes.len()).unwrap();
            self.nodes.push(TNode {
                key,
                kind: Kind::District,
                parent: pa,
                children: Vec::new(),
                files,
                count: 0,
                qcount: 0,
                age: u32::MAX,
                site: geom::pt(0.0, 0.0),
                radius: 0.0,
                m: 1.0,
                depth: 0,
                district: NOLABEL,
            });
            self.nodes[pa as usize].children.insert(0, me);
        }

        // Roll up counts, ages and depths.
        let order = self.postorder(0);
        for &n in &order {
            let (mut c, mut age) = (0u32, u32::MAX);
            for &fi in &self.nodes[n as usize].files {
                c += 1;
                age = age.min(self.repo.files[fi as usize].growth);
            }
            let kids = self.nodes[n as usize].children.clone();
            for k in kids {
                c += self.nodes[k as usize].count;
                age = age.min(self.nodes[k as usize].age);
            }
            self.nodes[n as usize].count = c;
            self.nodes[n as usize].qcount = quantise_count(c);
            self.nodes[n as usize].age = age;
        }
        let mut stack = vec![0u32];
        while let Some(n) = stack.pop() {
            let d = self.nodes[n as usize].depth;
            for k in self.nodes[n as usize].children.clone() {
                self.nodes[k as usize].depth = d + 1;
                stack.push(k);
            }
        }

    }

    fn postorder(&self, root: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut stack = vec![(root, false)];
        while let Some((n, done)) = stack.pop() {
            if done {
                out.push(n);
            } else {
                stack.push((n, true));
                for &k in &self.nodes[n as usize].children {
                    stack.push((k, false));
                }
            }
        }
        out
    }

    // -----------------------------------------------------------------
    // 2. Site weights (PRD §9: the tree determines position)
    // -----------------------------------------------------------------
    /// Multiplicative site weights. Cell area scales with `m`, so a 400-file
    /// directory claims proportionally more ground than a 3-file one, and the
    /// site always wins its own pixel so no cell can be empty.
    ///
    /// Weights are normalised **per sibling group**, because the partition only
    /// ever compares siblings. The `WEIGHT_FLOOR` caps the spread: strict
    /// proportionality would shrink a 2-file directory next to a 1400-file one
    /// below a pixel, and a district that cannot be seen cannot be navigated to.
    fn weigh(&mut self) {
        const GAMMA: f64 = 0.85;
        const WEIGHT_FLOOR: f64 = 1.0 / 110.0;
        for n in 0..self.nodes.len() {
            let kids = self.nodes[n].children.clone();
            if kids.is_empty() {
                continue;
            }
            let maxq = kids
                .iter()
                .map(|&k| f64::from(self.nodes[k as usize].qcount.max(1)))
                .fold(1.0f64, f64::max)
                .powf(GAMMA);
            for k in kids {
                let qc = f64::from(self.nodes[k as usize].qcount.max(1)).powf(GAMMA);
                self.nodes[k as usize].m = det::quantize_f64((qc / maxq).max(WEIGHT_FLOOR));
            }
        }
    }

    fn place_children(&mut self, n: u32) {
        let kids = self.nodes[n as usize].children.clone();
        if kids.is_empty() {
            return;
        }
        let base = self.nodes[n as usize].site;
        let radius = self.nodes[n as usize].radius;
        let qtotal = f64::from(self.nodes[n as usize].qcount.max(1));

        if kids.len() == 1 {
            let k = kids[0] as usize;
            self.nodes[k].site = base;
            self.nodes[k].radius = radius * 0.97;
            return;
        }

        // Age rank among siblings: an older subtree sits nearer the parent core,
        // which is nearer the historic centre. Git history is the growth order.
        let mut by_age: Vec<u32> = kids.clone();
        by_age.sort_by_key(|&k| (self.nodes[k as usize].age, self.nodes[k as usize].key.clone()));
        let mut agerank: BTreeMap<u32, f64> = BTreeMap::new();
        let denom = (kids.len().max(2) - 1) as f64;
        for (i, &k) in by_age.iter().enumerate() {
            agerank.insert(k, i as f64 / denom);
        }

        let selfk: Vec<u32> = kids
            .iter()
            .copied()
            .filter(|&k| self.nodes[k as usize].kind == Kind::District)
            .collect();
        let dirk: Vec<u32> = kids
            .iter()
            .copied()
            .filter(|&k| self.nodes[k as usize].kind == Kind::Dir)
            .collect();

        let mut rng = det::SeededRng::for_seed(
            det::fnv1a64_str(&self.nodes[n as usize].key),
            "place-children",
        );
        let base_angle = rng.range_f64(0.0, det::TAU);

        // The self-district takes the core of the region.
        for &k in &selfk {
            let jr = radius * 0.06;
            let a = rng.range_f64(0.0, det::TAU);
            let (s, c) = det::det_sin_cos(a);
            self.nodes[k as usize].site = geom::q(geom::pt(base.x + c * jr, base.y + s * jr));
            let qc = f64::from(self.nodes[k as usize].qcount.max(1));
            self.nodes[k as usize].radius = radius * (qc / qtotal).sqrt().clamp(0.20, 0.75);
        }

        if dirk.is_empty() {
            return;
        }
        // Angular wedges proportional to subtree weight, in canonical key order,
        // so alphabetically adjacent siblings are adjacent on the map.
        let wsum: f64 = dirk
            .iter()
            .map(|&k| f64::from(self.nodes[k as usize].qcount.max(1)).powf(0.55))
            .sum();
        let mut acc = 0.0f64;
        for &k in &dirk {
            let w = f64::from(self.nodes[k as usize].qcount.max(1)).powf(0.55) / wsum;
            let span = det::TAU * w;
            let mid = base_angle + acc + span * 0.5;
            acc += span;

            let mut krng =
                det::SeededRng::for_seed(det::fnv1a64_str(&self.nodes[k as usize].key), "site");
            let ang = mid + krng.range_f64(-0.34, 0.34) * span;
            let ar = agerank[&k];
            let core_pull = if selfk.is_empty() { 0.30 } else { 0.46 };
            let off = radius * (core_pull + 0.30 * ar) * krng.range_f64(0.88, 1.12);
            let (s, c) = det::det_sin_cos(ang);
            self.nodes[k as usize].site = geom::q(geom::pt(base.x + c * off, base.y + s * off));
            let qc = f64::from(self.nodes[k as usize].qcount.max(1));
            self.nodes[k as usize].radius = radius * (qc / qtotal).sqrt().clamp(0.16, 0.78);
        }
    }

    // -----------------------------------------------------------------
    // 3. Terrain (PRD §7.2 stage 1) — never rendered, only warps the metric
    // -----------------------------------------------------------------
    fn compute_terrain(&mut self) -> Warp {
        let n = (RES + 1) * (RES + 1);
        let mut wx = vec![0f32; n];
        let mut wy = vec![0f32; n];
        self.land = vec![false; RES * RES];
        // Warp is sampled on the pixel-centre lattice.
        for j in 0..RES {
            for i in 0..RES {
                let x = (i as f64 + 0.5) * CELL;
                let y = (j as f64 + 0.5) * CELL;
                let (px, py) = warp_point(x, y);
                let k = j * (RES + 1) + i;
                wx[k] = px as f32;
                wy[k] = py as f32;
                self.land[j * RES + i] = is_land(px, py);
            }
        }
        // Keep only the largest land mass: an offshore speck is not a city.
        let comp = components(&self.land, RES);
        if let Some(best) = comp.1 {
            for k in 0..RES * RES {
                if self.land[k] && comp.0[k] != best {
                    self.land[k] = false;
                }
            }
        }
        Warp { wx, wy }
    }

    /// A district whose site landed in the sea would get an empty cell. Move it
    /// to the nearest settled-capable pixel; deterministic, and it happens
    /// before anything else looks at the sites.
    fn snap_sites_to_land(&mut self, warp: &Warp) {
        let mut moved = 0u32;
        for i in 0..self.districts.len() {
            let n = self.districts[i].node as usize;
            let s = self.nodes[n].site;
            // The site is a warped-space coordinate; find the nearest land pixel
            // whose warped position is closest to it.
            let mut best = (f64::MAX, s);
            let mut on_land = false;
            for j in (0..RES).step_by(4) {
                for k in (0..RES).step_by(4) {
                    let idx = j * RES + k;
                    if !self.land[idx] {
                        continue;
                    }
                    let wk = j * (RES + 1) + k;
                    let p = geom::pt(f64::from(warp.wx[wk]), f64::from(warp.wy[wk]));
                    let d = geom::dist2(p, s);
                    if d < best.0 {
                        best = (d, p);
                    }
                    if d < (CELL * 3.0) * (CELL * 3.0) {
                        on_land = true;
                    }
                }
            }
            if !on_land && best.0 < f64::MAX {
                self.nodes[n].site = geom::q(best.1);
                moved += 1;
            }
        }
        if moved > 0 {
            self.notes.push(format!("{moved} district sites snapped inland"));
        }
    }

    // -----------------------------------------------------------------
    // 4. Pass A — recursive split of the settled ground by the directory tree
    //
    // Each node owns a set of pixels. Its children's sites are PROJECTED ONTO
    // that pixel set, so a child's seed always lies inside its parent's cell and
    // no cell can come out empty. Splitting stops when a region can no longer
    // carry a legible quarter: below that scale the whole subtree becomes one
    // district. That is semantic zoom, not a failure.
    // -----------------------------------------------------------------
    fn pass_a(&mut self, warp: &Warp) {
        let n = RES * RES;
        // The settled ground: an organic town outline inside the coast, so the
        // countryside ring between the two reads as a real edge of settlement.
        let mut settled = vec![false; n];
        for j in 0..RES {
            for i in 0..RES {
                let k = j * RES + i;
                if !self.land[k] {
                    continue;
                }
                let wk = j * (RES + 1) + i;
                let (x, y) = (f64::from(warp.wx[wk]), f64::from(warp.wy[wk]));
                let dx = x - WORLD * 0.5;
                let dy = y - WORLD * 0.5;
                let d = (dx * dx + dy * dy).sqrt() / 424.0;
                let nz = det::fbm2_f64(SEED_TOWN, x / 190.0, y / 190.0, 3);
                settled[k] = 1.0 - d * d * d + 0.46 * nz > 0.0;
            }
        }
        let (comp, best) = components(&settled, RES);
        if let Some(b) = best {
            for k in 0..n {
                if settled[k] && comp[k] != b {
                    settled[k] = false;
                }
            }
        }
        // Fill interior pockets: a hole inside the town leaves an orphan ring in
        // the road graph and reads as a bug, not as a park.
        let unset: Vec<bool> = (0..n).map(|i| !settled[i]).collect();
        let (uc, _) = components(&unset, RES);
        let mut outside: BTreeSet<u32> = BTreeSet::new();
        for i in 0..RES {
            for &k in &[i, (RES - 1) * RES + i, i * RES, i * RES + RES - 1] {
                if unset[k] {
                    outside.insert(uc[k]);
                }
            }
        }
        for k in 0..n {
            if unset[k] && !outside.contains(&uc[k]) {
                settled[k] = true;
            }
        }
        self.k_claim = settled.iter().filter(|b| **b).count() as f64 * CELL * CELL;

        let wpos = |k: usize| -> Pt {
            let (i, j) = (k % RES, k / RES);
            let wk = j * (RES + 1) + i;
            geom::pt(f64::from(warp.wx[wk]), f64::from(warp.wy[wk]))
        };

        let mut lab = vec![NOLABEL; n];
        let root_px: Vec<u32> = (0..n)
            .filter(|&k| settled[k])
            .map(|k| u32::try_from(k).unwrap())
            .collect();
        let mut leaves: Vec<(u32, Vec<u32>)> = Vec::new();
        let mut queue: VecDeque<(u32, Vec<u32>)> = VecDeque::new();
        queue.push_back((0, root_px));
        let mut tiny = 0u32;
        // Files of subtrees that were too small to draw, rehomed onto a sibling.
        let mut absorbed: BTreeMap<u32, Vec<u32>> = BTreeMap::new();

        while let Some((nd, px)) = queue.pop_front() {
            let kids = self.nodes[nd as usize].children.clone();
            if kids.is_empty() || px.len() < kids.len() * MIN_DISTRICT_CELLS {
                leaves.push((nd, px));
                continue;
            }
            let mut cx = 0.0;
            let mut cy = 0.0;
            for &k in &px {
                let p = wpos(k as usize);
                cx += p.x;
                cy += p.y;
            }
            let c = geom::pt(cx / px.len() as f64, cy / px.len() as f64);
            let radius = ((px.len() as f64 * CELL * CELL) / std::f64::consts::PI).sqrt();

            let mut by_age: Vec<u32> = kids.clone();
            by_age
                .sort_by_key(|&k| (self.nodes[k as usize].age, self.nodes[k as usize].key.clone()));
            let mut agerank: BTreeMap<u32, f64> = BTreeMap::new();
            let denom = (kids.len().max(2) - 1) as f64;
            for (i, &k) in by_age.iter().enumerate() {
                agerank.insert(k, i as f64 / denom);
            }
            let selfk: Vec<u32> = kids
                .iter()
                .copied()
                .filter(|&k| self.nodes[k as usize].kind == Kind::District)
                .collect();
            let dirk: Vec<u32> = kids
                .iter()
                .copied()
                .filter(|&k| self.nodes[k as usize].kind == Kind::Dir)
                .collect();
            let mut rng = det::SeededRng::for_seed(
                det::fnv1a64_str(&self.nodes[nd as usize].key),
                "place-children",
            );
            let base_angle = rng.range_f64(0.0, det::TAU);
            let mut targets: Vec<(u32, Pt)> = Vec::new();
            for &k in &selfk {
                // A directory's own files are the core of its quarter; the
                // subdirectories ring it.
                targets.push((k, c));
            }
            let wsum: f64 = dirk
                .iter()
                .map(|&k| f64::from(self.nodes[k as usize].qcount.max(1)).powf(0.55))
                .sum();
            let mut acc = 0.0f64;
            for &k in &dirk {
                let w = f64::from(self.nodes[k as usize].qcount.max(1)).powf(0.55) / wsum.max(1e-9);
                let span = det::TAU * w;
                let mid = base_angle + acc + span * 0.5;
                acc += span;
                let mut krng =
                    det::SeededRng::for_seed(det::fnv1a64_str(&self.nodes[k as usize].key), "site");
                let ang = mid + krng.range_f64(-0.30, 0.30) * span;
                let ar = agerank[&k];
                let core_pull = if selfk.is_empty() { 0.30 } else { 0.46 };
                // Older subtrees sit nearer the parent core, which is nearer the
                // historic centre. Git history is the growth order (PRD 7.1).
                let off = radius * (core_pull + 0.30 * ar) * krng.range_f64(0.85, 1.15);
                let (s, co) = det::det_sin_cos(ang);
                targets.push((k, geom::pt(c.x + co * off, c.y + s * off)));
            }
            let mut used: BTreeSet<u32> = BTreeSet::new();
            for (k, t) in &targets {
                let mut best = (f64::MAX, px[0]);
                for &q in &px {
                    if used.contains(&q) {
                        continue;
                    }
                    let d = geom::dist2(wpos(q as usize), *t);
                    if d < best.0 {
                        best = (d, q);
                    }
                }
                used.insert(best.1);
                self.nodes[*k as usize].site = geom::q(wpos(best.1 as usize));
                self.nodes[*k as usize].radius = radius;
            }
            let mut groups: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
            for &q in &px {
                let p = wpos(q as usize);
                let mut bi = kids[0];
                let mut bd = f64::MAX;
                for &k in &kids {
                    let nk = &self.nodes[k as usize];
                    let dx = p.x - nk.site.x;
                    let dy = p.y - nk.site.y;
                    let d = (dx * dx + dy * dy) / nk.m;
                    if d < bd {
                        bd = d;
                        bi = k;
                    }
                }
                groups.entry(bi).or_default().push(q);
            }
            let mut viable: Vec<(u32, Vec<u32>)> = Vec::new();
            let mut starved: Vec<(u32, Vec<u32>)> = Vec::new();
            for &k in &kids {
                let g = groups.remove(&k).unwrap_or_default();
                if g.len() < MIN_TINY_CELLS {
                    starved.push((k, g));
                } else {
                    viable.push((k, g));
                }
            }
            if viable.is_empty() {
                // Nothing here can be split legibly: the whole node is one quarter.
                leaves.push((nd, px));
                continue;
            }
            for (k, g) in starved {
                tiny += 1;
                // Absorb into the nearest viable sibling, files and ground both.
                let sk = self.nodes[k as usize].site;
                let mut best = (f64::MAX, 0usize);
                for (i, (v, _)) in viable.iter().enumerate() {
                    let d = geom::dist2(self.nodes[*v as usize].site, sk);
                    if d < best.0 {
                        best = (d, i);
                    }
                }
                viable[best.1].1.extend(g);
                let extra = self.subtree_files(k);
                absorbed.entry(viable[best.1].0).or_default().extend(extra);
            }
            for (k, g) in viable {
                queue.push_back((k, g));
            }
        }
        if tiny > 0 {
            self.notes
                .push(format!("{tiny} subtrees fell below the legible-quarter size"));
        }

        leaves.sort_by(|a, b| self.nodes[a.0 as usize].key.cmp(&self.nodes[b.0 as usize].key));
        let mut ages: Vec<(u32, String, usize)> = Vec::new();
        for (i, (nd, _)) in leaves.iter().enumerate() {
            ages.push((
                self.nodes[*nd as usize].age,
                self.nodes[*nd as usize].key.clone(),
                i,
            ));
        }
        ages.sort();
        let mut rank = vec![0.0f64; leaves.len()];
        let denom = (leaves.len().max(2) - 1) as f64;
        for (r, (_, _, i)) in ages.iter().enumerate() {
            rank[*i] = r as f64 / denom;
        }
        // Rehomed files land on the first leaf, in canonical order, under the
        // node that swallowed them. Claimed once, so nothing is lost or doubled.
        let mut claim: Vec<Vec<u32>> = vec![Vec::new(); leaves.len()];
        for (i, (nd, _)) in leaves.iter().enumerate() {
            let mut anc = *nd;
            loop {
                if let Some(extra) = absorbed.remove(&anc) {
                    claim[i].extend(extra);
                }
                let par = self.nodes[anc as usize].parent;
                if par == NOLABEL {
                    break;
                }
                anc = par;
            }
        }
        for (i, (nd, px)) in leaves.iter().enumerate() {
            let id = u32::try_from(i).unwrap();
            // A collapsed subtree becomes one district holding every file below it.
            let mut files = self.subtree_files(*nd);
            files.extend(claim[i].iter().copied());
            files.sort_unstable();
            files.dedup();
            self.nodes[*nd as usize].district = id;
            let mut cx = 0.0;
            let mut cy = 0.0;
            for &k in px {
                lab[k as usize] = id;
                cx += ((k as usize % RES) as f64 + 0.5) * CELL;
                cy += ((k as usize / RES) as f64 + 0.5) * CELL;
            }
            let np = px.len().max(1) as f64;
            self.districts.push(DistrictData {
                node: *nd,
                key: self.nodes[*nd as usize].key.clone(),
                px: u32::try_from(px.len()).unwrap(),
                centroid: geom::pt(cx / np, cy / np),
                age_rank: rank[i],
                blocks: Vec::new(),
                files,
            });
        }
        cleanup_labels(&mut lab, RES);
        self.dlabel = lab;
    }

    fn subtree_files(&self, n: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut stack = vec![n];
        while let Some(k) = stack.pop() {
            out.extend_from_slice(&self.nodes[k as usize].files);
            for &c in &self.nodes[k as usize].children {
                stack.push(c);
            }
        }
        out.sort_unstable();
        out
    }


    // -----------------------------------------------------------------
    // 5. Pass B — block partition inside each district
    // -----------------------------------------------------------------
    fn pass_b(&mut self, warp: &Warp) {
        // Per-district pixel lists (canonical: raster order).
        let nd = self.districts.len();
        let mut pix: Vec<Vec<u32>> = vec![Vec::new(); nd];
        for k in 0..RES * RES {
            let l = self.dlabel[k];
            if l != NOLABEL {
                pix[l as usize].push(u32::try_from(k).unwrap());
            }
        }

        for di in 0..nd {
            let node = self.districts[di].node as usize;
            let nfiles = u32::try_from(self.districts[di].files.len()).unwrap().max(1);
            let ar = self.districts[di].age_rank;
            // Old town: fine grain, many small blocks. New periphery: coarser,
            // more planned. PRD §7.1, made structural rather than decorative.
            // Block grain is area-driven, so the street texture stays the same
            // density at 100 files and at 5000. The old town gets a finer grain
            // and the new periphery a coarser, more planned one: PRD 7.1's age
            // structure made structural rather than decorative.
            let grain = TARGET_BLOCK_CELLS * (0.45 + 1.20 * ar);
            let cells = pix[di].len();
            let want = (cells as f64 / grain).round().max(1.0) as usize;
            // Never more blocks than there are files to fill them, and never a
            // block too small for the raster to carry a clean polygon.
            let cap_files = ((f64::from(nfiles) / 2.2).ceil() as usize).max(1);
            let cap_area = (cells / MIN_BLOCK_CELLS).max(1);
            let nblocks = want.min(cap_files).min(cap_area).max(1);

            let mut sites: Vec<(Pt, Pt)> = Vec::new();
            if cells == 0 {
                sites.push((self.districts[di].centroid, self.nodes[node].site));
            } else {
                let mut rng = det::SeededRng::for_seed(
                    det::fnv1a64_str(&self.nodes[node].key),
                    "block-sites",
                );
                let area = cells as f64 * CELL * CELL;
                let mut mind = (area / nblocks as f64).sqrt() * 0.86;
                let mut fails = 0u32;
                let mut tries = 0u32;
                while sites.len() < nblocks && tries < 24_000 {
                    tries += 1;
                    let idx = rng.below(cells as u64) as usize;
                    let k = pix[di][idx] as usize;
                    let (ci, cj) = (k % RES, k / RES);
                    let p = geom::pt((ci as f64 + 0.5) * CELL, (cj as f64 + 0.5) * CELL);
                    let wk = cj * (RES + 1) + ci;
                    let pw = geom::pt(f64::from(warp.wx[wk]), f64::from(warp.wy[wk]));
                    if sites.iter().all(|s| geom::dist(s.1, pw) >= mind) {
                        sites.push((geom::q(p), geom::q(pw)));
                        fails = 0;
                    } else {
                        fails += 1;
                        if fails > 220 {
                            mind *= 0.84;
                            fails = 0;
                        }
                    }
                }
                if sites.is_empty() {
                    sites.push((self.districts[di].centroid, self.nodes[node].site));
                }
            }
            // Rank by distance from the district core: block 0 holds the oldest
            // files and sits at the heart of the quarter.
            let c = self.districts[di].centroid;
            det::sort_by_f64_key(&mut sites, |p| geom::dist(p.0, c));
            for (rank, s) in sites.iter().enumerate() {
                let id = u32::try_from(self.blocks.len()).unwrap();
                self.blocks.push(BlockData {
                    district: u32::try_from(di).unwrap(),
                    rank: u32::try_from(rank).unwrap(),
                    site: s.0,
                    site_w: s.1,
                    px: 0,
                    poly: Vec::new(),
                    edge_class: Vec::new(),
                    buildable: Vec::new(),
                    cap: 0,
                    files: Vec::new(),
                    lots: Vec::new(),
                });
                self.districts[di].blocks.push(id);
            }
        }

        let mut blab = vec![NOLABEL; RES * RES];
        for di in 0..nd {
            let ids = self.districts[di].blocks.clone();
            for &k in &pix[di] {
                let k = k as usize;
                let (i, j) = (k % RES, k / RES);
                let wk = j * (RES + 1) + i;
                let px = f64::from(warp.wx[wk]);
                let py = f64::from(warp.wy[wk]);
                let mut bi = ids[0];
                let mut bd = f64::MAX;
                for &b in &ids {
                    let s = self.blocks[b as usize].site_w;
                    let dx = px - s.x;
                    let dy = py - s.y;
                    let d = dx * dx + dy * dy;
                    if d < bd {
                        bd = d;
                        bi = b;
                    }
                }
                blab[k] = bi;
            }
        }
        cleanup_labels(&mut blab, RES);
        for k in 0..RES * RES {
            if blab[k] != NOLABEL {
                self.blocks[blab[k] as usize].px += 1;
            }
        }
        self.blabel = blab;
    }

    // -----------------------------------------------------------------
    // 6. Roads = the borders of the partition
    // -----------------------------------------------------------------
    fn extract_roads(&mut self) {
        let lab = |i: i64, j: i64| -> u32 {
            if i < 0 || j < 0 || i >= RES as i64 || j >= RES as i64 {
                NOLABEL
            } else {
                self.blabel[j as usize * RES + i as usize]
            }
        };
        let vw = RES + 1;
        let mut hedge = vec![false; vw * RES];
        let mut vedge = vec![false; RES * vw];
        for j in 0..=RES {
            for i in 0..RES {
                let a = lab(i as i64, j as i64 - 1);
                let b = lab(i as i64, j as i64);
                hedge[j * RES + i] = a != b;
            }
        }
        for j in 0..RES {
            for i in 0..=RES {
                let a = lab(i as i64 - 1, j as i64);
                let b = lab(i as i64, j as i64);
                vedge[j * vw + i] = a != b;
            }
        }
        let deg = |i: usize, j: usize| -> u32 {
            let mut d = 0;
            if i > 0 && hedge[j * RES + i - 1] {
                d += 1;
            }
            if i < RES && hedge[j * RES + i] {
                d += 1;
            }
            if j > 0 && vedge[(j - 1) * vw + i] {
                d += 1;
            }
            if j < RES && vedge[j * vw + i] {
                d += 1;
            }
            d
        };

        let mut jid = vec![NOLABEL; vw * vw];
        let mut jpos: Vec<Pt> = Vec::new();
        for j in 0..=RES {
            for i in 0..=RES {
                let d = deg(i, j);
                if d != 0 && d != 2 {
                    jid[j * vw + i] = u32::try_from(jpos.len()).unwrap();
                    jpos.push(geom::pt(i as f64 * CELL, j as f64 * CELL));
                }
            }
        }

        let mut hused = vec![false; vw * RES];
        let mut vused = vec![false; RES * vw];
        let mut chains: Vec<Chain> = Vec::new();

        // Walk every chain that starts at a junction.
        let dirs: [(i64, i64); 4] = [(1, 0), (-1, 0), (0, 1), (0, -1)];
        for j in 0..=RES {
            for i in 0..=RES {
                if jid[j * vw + i] == NOLABEL {
                    continue;
                }
                for &(dx, dy) in &dirs {
                    if let Some(c) = walk_chain(
                        i, j, dx, dy, &hedge, &vedge, &mut hused, &mut vused, &jid, &deg, &lab,
                    ) {
                        chains.push(c);
                    }
                }
            }
        }
        // Pure loops: a region completely enclosed by exactly one neighbour has
        // no junction anywhere on its border.
        for j in 0..=RES {
            for i in 0..RES {
                if hedge[j * RES + i] && !hused[j * RES + i] {
                    let id = u32::try_from(jpos.len()).unwrap();
                    jid[j * vw + i] = id;
                    jpos.push(geom::pt(i as f64 * CELL, j as f64 * CELL));
                    if let Some(c) = walk_chain(
                        i, j, 1, 0, &hedge, &vedge, &mut hused, &mut vused, &jid, &deg, &lab,
                    ) {
                        chains.push(c);
                    }
                }
            }
        }
        for j in 0..RES {
            for i in 0..=RES {
                if vedge[j * vw + i] && !vused[j * vw + i] {
                    let id = u32::try_from(jpos.len()).unwrap();
                    jid[j * vw + i] = id;
                    jpos.push(geom::pt(i as f64 * CELL, j as f64 * CELL));
                    if let Some(c) = walk_chain(
                        i, j, 0, 1, &hedge, &vedge, &mut hused, &mut vused, &jid, &deg, &lab,
                    ) {
                        chains.push(c);
                    }
                }
            }
        }

        // Simplify the staircase away, then collapse the short chains.
        for c in &mut chains {
            c.pts = geom::simplify(&c.pts, CELL * 2.0);
        }

        // Short-edge collapse. This is PRD §7.2's snap rule applied to the
        // border graph: merging two nearby degree-3 nodes makes ONE degree-4
        // junction, and simultaneously removes the sliver face between them.
        let mut lens: Vec<f64> = chains
            .iter()
            .filter(|c| c.a != c.b)
            .map(|c| geom::polyline_len(&c.pts))
            .collect();
        det::sort_by_f64_key(&mut lens, |l| *l);
        let median = if lens.is_empty() {
            0.0
        } else {
            lens[lens.len() / 2]
        };
        let thresh = median * 0.34;

        let mut order: Vec<u32> = (0..chains.len() as u32).collect();
        det::sort_by_f64_key(&mut order, |&c| geom::polyline_len(&chains[c as usize].pts));
        let mut uf: Vec<u32> = (0..jpos.len() as u32).collect();
        fn find(uf: &mut Vec<u32>, x: u32) -> u32 {
            let mut r = x;
            while uf[r as usize] != r {
                r = uf[r as usize];
            }
            let mut c = x;
            while uf[c as usize] != c {
                let n = uf[c as usize];
                uf[c as usize] = r;
                c = n;
            }
            r
        }
        // How many chains still bound each face; never starve a face below 3.
        let mut face_deg: BTreeMap<u32, u32> = BTreeMap::new();
        for c in &chains {
            *face_deg.entry(c.left).or_insert(0) += 1;
            *face_deg.entry(c.right).or_insert(0) += 1;
        }
        for &ci in &order {
            let c = &chains[ci as usize];
            if !c.alive {
                continue;
            }
            if geom::polyline_len(&c.pts) > thresh {
                break;
            }
            let (ra, rb) = (find(&mut uf, c.a), find(&mut uf, c.b));
            if ra == rb {
                continue;
            }
            let (l, r) = (c.left, c.right);
            if face_deg.get(&l).copied().unwrap_or(0) <= 3
                || face_deg.get(&r).copied().unwrap_or(0) <= 3
            {
                continue;
            }
            *face_deg.get_mut(&l).unwrap() -= 1;
            *face_deg.get_mut(&r).unwrap() -= 1;
            let mid = geom::pt(
                (jpos[ra as usize].x + jpos[rb as usize].x) * 0.5,
                (jpos[ra as usize].y + jpos[rb as usize].y) * 0.5,
            );
            let keep = ra.min(rb);
            let gone = ra.max(rb);
            uf[gone as usize] = keep;
            jpos[keep as usize] = mid;
            chains[ci as usize].alive = false;
        }

        for c in chains.iter_mut() {
            c.a = find(&mut uf, c.a);
            c.b = find(&mut uf, c.b);
            if c.alive && c.a == c.b && c.pts.len() < 4 {
                c.alive = false;
            }
            if c.alive {
                let n = c.pts.len();
                c.pts[0] = jpos[c.a as usize];
                c.pts[n - 1] = jpos[c.b as usize];
                c.pts = geom::chaikin_open(&c.pts, 2);
            }
        }

        // Classify: the border between two districts is an arterial road; the
        // border with unsettled ground is the city limit.
        for c in chains.iter_mut() {
            let dl = if c.left == NOLABEL {
                NOLABEL
            } else {
                self.blocks[c.left as usize].district
            };
            let dr = if c.right == NOLABEL {
                NOLABEL
            } else {
                self.blocks[c.right as usize].district
            };
            c.class = if c.left == NOLABEL || c.right == NOLABEL {
                RoadClass::Perimeter
            } else if dl != dr {
                RoadClass::Arterial
            } else {
                RoadClass::Secondary
            };
        }

        self.chains = chains;
        self.jpos = jpos;
    }

    // -----------------------------------------------------------------
    // 7. Blocks = the closed faces of the road graph
    // -----------------------------------------------------------------
    fn assemble_faces(&mut self) {
        // Directed refs per face, keyed by start junction.
        let nb = self.blocks.len();
        let mut refs: Vec<Vec<(u32, bool)>> = vec![Vec::new(); nb];
        for (ci, c) in self.chains.iter().enumerate() {
            if !c.alive {
                continue;
            }
            if c.left != NOLABEL {
                refs[c.left as usize].push((u32::try_from(ci).unwrap(), true));
            }
            if c.right != NOLABEL {
                refs[c.right as usize].push((u32::try_from(ci).unwrap(), false));
            }
        }
        let mut dropped = 0u32;
        for b in 0..nb {
            let list = &refs[b];
            if list.is_empty() {
                dropped += 1;
                continue;
            }
            let mut used = vec![false; list.len()];
            let mut best_poly: Vec<Pt> = Vec::new();
            let mut best_cls: Vec<RoadClass> = Vec::new();
            let mut best_area = 0.0f64;
            loop {
                let Some(start) = (0..list.len()).find(|&i| !used[i]) else {
                    break;
                };
                let mut poly: Vec<Pt> = Vec::new();
                let mut cls: Vec<RoadClass> = Vec::new();
                let mut cur = start;
                let first_node = self.ref_start(list[start]);
                let mut guard = 0;
                loop {
                    used[cur] = true;
                    let (ci, fwd) = list[cur];
                    let ch = &self.chains[ci as usize];
                    let n = ch.pts.len();
                    if fwd {
                        for p in ch.pts.iter().take(n - 1) {
                            poly.push(*p);
                            cls.push(ch.class);
                        }
                    } else {
                        for k in (1..n).rev() {
                            poly.push(ch.pts[k]);
                            cls.push(ch.class);
                        }
                    }
                    let end = self.ref_end(list[cur]);
                    if end == first_node {
                        break;
                    }
                    let incoming = if fwd {
                        geom::sub(ch.pts[n - 1], ch.pts[n - 2])
                    } else {
                        geom::sub(ch.pts[0], ch.pts[1])
                    };
                    let mut pick = None;
                    let mut best_ang = f64::MAX;
                    for (i, r) in list.iter().enumerate() {
                        if used[i] || self.ref_start(*r) != end {
                            continue;
                        }
                        let out = self.ref_dir(*r);
                        let a = ccw_angle(geom::scale(incoming, -1.0), out);
                        if a < best_ang {
                            best_ang = a;
                            pick = Some(i);
                        }
                    }
                    match pick {
                        Some(i) => cur = i,
                        None => break,
                    }
                    guard += 1;
                    if guard > 4000 {
                        break;
                    }
                }
                let a = geom::area(&poly);
                if poly.len() >= 3 && a > best_area {
                    best_area = a;
                    best_poly = poly;
                    best_cls = cls;
                }
            }
            if best_poly.len() < 3 {
                dropped += 1;
                continue;
            }
            if geom::signed_area(&best_poly) < 0.0 {
                best_poly.reverse();
                best_cls.reverse();
                best_cls.rotate_right(1);
            }
            self.blocks[b].poly = best_poly;
            self.blocks[b].edge_class = best_cls;
        }
        if dropped > 0 {
            self.notes
                .push(format!("{dropped} blocks had no traceable face"));
        }
    }

    fn ref_start(&self, r: (u32, bool)) -> u32 {
        let c = &self.chains[r.0 as usize];
        if r.1 {
            c.a
        } else {
            c.b
        }
    }
    fn ref_end(&self, r: (u32, bool)) -> u32 {
        let c = &self.chains[r.0 as usize];
        if r.1 {
            c.b
        } else {
            c.a
        }
    }
    fn ref_dir(&self, r: (u32, bool)) -> Pt {
        let c = &self.chains[r.0 as usize];
        let n = c.pts.len();
        if r.1 {
            geom::sub(c.pts[1], c.pts[0])
        } else {
            geom::sub(c.pts[n - 2], c.pts[n - 1])
        }
    }

    // -----------------------------------------------------------------
    // 8. Lots and buildings (PRD §7.2 stages 4 and 5)
    // -----------------------------------------------------------------
    fn lots_and_buildings(&mut self) {
        const SETBACK: f64 = 0.55;
        let mut sizes: Vec<f64> = self.repo.files.iter().map(|f| (f.size as f64).sqrt()).collect();
        det::sort_by_f64_key(&mut sizes, |x| *x);
        self.size_median = if sizes.is_empty() { 1.0 } else { sizes[sizes.len() / 2].max(1.0) };
        for b in 0..self.blocks.len() {
            let poly = self.blocks[b].poly.clone();
            if poly.len() < 3 {
                continue;
            }
            // The road centreline runs along the block edge, so the buildable
            // core is the block inset by the road's half-width plus a setback.
            let cls = &self.blocks[b].edge_class;
            let ds: Vec<f64> = (0..poly.len())
                .map(|i| {
                    cls.get(i).copied().unwrap_or(RoadClass::Secondary).half_width()
                        + SETBACK
                        + CELL * 1.4
                })
                .collect();
            let simp = simplify_closed(&poly, &ds, CELL * 0.9);
            let mut core = geom::inset_robust(&simp.0, &simp.1);
            if core.len() < 3 {
                // Retry with the road clearance alone, dropping the setback.
                // Clearance from the carriageway is a guarantee; the setback is
                // a nicety, and a block too tight for both keeps the guarantee.
                let bare: Vec<f64> = (0..simp.0.len())
                    .map(|i| cls.get(i).copied().unwrap_or(RoadClass::Secondary).half_width())
                    .collect();
                let bare = if bare.len() == simp.1.len() { bare } else { simp.1.clone() };
                core = geom::inset_robust(&simp.0, &bare);
            }
            self.blocks[b].buildable = core;
        }

        let mut rescued = 0u32;
        let mut homeless = 0u32;
        // A block whose face came out degenerate cannot hold anything; drop it
        // so its files fall to a real block instead of vanishing.
        for b in 0..self.blocks.len() {
            // A block narrower than a road cannot hold a building that is not
            // on one. It stays a face of the road graph -- a courtyard -- and
            // its files go to a block that can take them.
            if geom::area(&self.blocks[b].buildable) < 10.0 {
                self.blocks[b].buildable.clear();
            }
        }

        // Capacity from area, so that appending a file lands in the first block
        // with room and nothing else moves (PRD §7.4 incremental growth).
        for di in 0..self.districts.len() {
            let ids = self.districts[di].blocks.clone();
            let q = f64::from(quantise_count(
                u32::try_from(self.districts[di].files.len()).unwrap().max(1),
            ));
            let mut valid: Vec<u32> = ids
                .iter()
                .copied()
                .filter(|&b| geom::area(&self.blocks[b as usize].buildable) >= 10.0)
                .collect();
            if valid.is_empty() {
                // Every block in this quarter refused an offset. Rescue the
                // largest one so its files still get a home rather than
                // vanishing off the map.
                let mut best = (0.0f64, None);
                for &b in &ids {
                    let a = geom::area(&self.blocks[b as usize].poly);
                    if a > best.0 {
                        best = (a, Some(b));
                    }
                }
                if let Some(b) = best.1 {
                    let poly = self.blocks[b as usize].poly.clone();
                    let ecls = self.blocks[b as usize].edge_class.clone();
                    // The setback is dropped, the road clearance never is.
                    let ds: Vec<f64> = (0..poly.len())
                        .map(|i| {
                            ecls.get(i).copied().unwrap_or(RoadClass::Arterial).half_width()
                                + CELL * 1.4
                        })
                        .collect();
                    let core = geom::inset_robust(&poly, &ds);
                    if geom::area(&core) >= 6.0 {
                        self.blocks[b as usize].buildable = core;
                        valid.push(b);
                        rescued += 1;
                    }
                }
            }
            if valid.is_empty() {
                homeless += self.districts[di].files.len() as u32;
                continue;
            }
            let total: f64 = valid
                .iter()
                .map(|&b| geom::area(&self.blocks[b as usize].buildable))
                .sum();
            for &b in &valid {
                let a = geom::area(&self.blocks[b as usize].buildable);
                let share = if total > 1e-9 { a / total } else { 0.0 };
                self.blocks[b as usize].cap = (share * q * 1.04).round().max(1.0) as u32;
            }
            let ids = valid;
            // Files in growth order fill blocks from the core outwards.
            let mut files = self.districts[di].files.clone();
            files.sort_by_key(|&f| {
                (
                    self.repo.files[f as usize].growth,
                    self.repo.files[f as usize].path.clone(),
                )
            });
            let mut it = files.into_iter().peekable();
            for &b in &ids {
                let cap = self.blocks[b as usize].cap;
                let mut taken = Vec::new();
                while taken.len() < cap as usize {
                    match it.next() {
                        Some(f) => taken.push(f),
                        None => break,
                    }
                }
                self.blocks[b as usize].files = taken;
            }
            // Overflow (every block full) goes to the largest block.
            let rest: Vec<u32> = it.collect();
            if !rest.is_empty() {
                let mut biggest = ids[0];
                let mut ba = -1.0;
                for &b in &ids {
                    let a = geom::area(&self.blocks[b as usize].buildable);
                    if a > ba {
                        ba = a;
                        biggest = b;
                    }
                }
                self.blocks[biggest as usize].files.extend(rest);
            }
        }

        if rescued > 0 || homeless > 0 {
            self.notes.push(format!(
                "{rescued} quarters needed a rescued block; {homeless} files could not be housed"
            ));
        }
        for b in 0..self.blocks.len() {
            self.rebuild_block(b, SETBACK);
        }
    }

    /// Recompute one block's lots and buildings. This is the incremental step:
    /// adding a file touches exactly this much of the world.
    pub fn rebuild_block(&mut self, b: usize, setback: f64) {
        let _ = setback;
        self.blocks[b].lots.clear();
        self.buildings.retain(|x| x.block != b as u32);
        let core = self.blocks[b].buildable.clone();
        let files = self.blocks[b].files.clone();
        if core.len() < 3 || files.is_empty() {
            return;
        }
        let seed = det::combine_seeds(
            det::fnv1a64_str(&self.districts[self.blocks[b].district as usize].key),
            u64::from(self.blocks[b].rank) * 0x9E37_79B9,
        );
        let mut lots = geom::subdivide(&core, files.len(), seed);
        det::sort_by_f64_key(&mut lots, |l| {
            let c = geom::centroid(l);
            det::quantize_f64(c.y) * 4096.0 + det::quantize_f64(c.x)
        });
        while lots.len() < files.len() {
            lots.push(lots[0].clone());
        }
        for (i, f) in files.iter().enumerate() {
            let lot = &lots[i];
            if lot.len() < 3 {
                continue;
            }
            let la = geom::area(lot);
            let inset = (0.16 * la.sqrt()).clamp(0.35, 2.2);
            let mut poly = geom::inset_robust(lot, &vec![inset; lot.len()]);
            if poly.len() < 3 {
                // The lot is already clear of every road, so a plain shrink
                // inside it cannot put a building on one.
                poly = geom::shrink_to(lot, 0.70);
            }
            if poly.len() < 3 {
                continue;
            }
            // A building reads as a building when it is a block, not a wedge:
            // take the lot's oriented bounding box and clip it back into the
            // buildable footprint. Guarantees containment, keeps rectilinearity.
            let obb = geom::min_area_box(&poly);
            let clipped = geom::clip_to(&obb, &poly);
            let poly = if clipped.len() >= 3 && geom::area(&clipped) > 0.30 * geom::area(&poly) {
                clipped
            } else {
                poly
            };
            let ia = geom::area(&poly);
            // Footprint area proportional to sqrt(file size) (PRD §7.3), expressed
            // as a fraction of the lot so it stays legible at any city scale.
            let fr = &self.repo.files[*f as usize];
            let rel = ((fr.size as f64).sqrt() / self.size_median.max(1.0)).clamp(0.40, 2.3);
            let frac = (0.28 * rel).clamp(0.14, 0.48);
            let target = la * frac;
            let fscale = (target / ia.max(1e-6)).sqrt().clamp(0.34, 1.0);
            let mut poly = geom::shrink_to(&poly, fscale);
            // ±4° rotation, refused when it would leave the lot.
            let mut rng = det::SeededRng::for_seed(det::fnv1a64_str(&fr.path), "roof-rot");
            let ang = rng.range_f64(-0.0698, 0.0698);
            let rot = geom::rotate_about(&poly, geom::centroid(&poly), ang);
            if rot.iter().all(|p| geom::contains(lot, *p)) {
                poly = rot;
            }
            // Containment is a guarantee, not a hope: shrink until it holds.
            let mut guard = 0;
            while guard < 10 && !poly.iter().all(|p| geom::contains(lot, *p)) {
                poly = geom::shrink_to(&poly, 0.86);
                guard += 1;
            }
            if !poly.iter().all(|p| geom::contains(lot, *p)) {
                continue;
            }
            let li = u32::try_from(self.blocks[b].lots.len()).unwrap();
            self.blocks[b].lots.push(lot.clone());
            self.buildings.push(Building {
                file: *f,
                block: u32::try_from(b).unwrap(),
                lot_idx: li,
                poly,
            });
        }
    }

    // -----------------------------------------------------------------
    // 9. Streets: cross-district imports routed ON the road network
    // -----------------------------------------------------------------
    fn route_streets(&mut self) {
        if self.chains.is_empty() {
            return;
        }
        let nj = self.jpos.len();
        let mut adj: Vec<Vec<(u32, f64, u32)>> = vec![Vec::new(); nj];
        for (ci, c) in self.chains.iter().enumerate() {
            if !c.alive || c.a == c.b {
                continue;
            }
            let w = geom::polyline_len(&c.pts);
            adj[c.a as usize].push((c.b, w, u32::try_from(ci).unwrap()));
            adj[c.b as usize].push((c.a, w, u32::try_from(ci).unwrap()));
        }
        let key_to_d: BTreeMap<&str, usize> = self
            .districts
            .iter()
            .enumerate()
            .map(|(i, d)| (d.key.as_str(), i))
            .collect();
        // A district's gateway is the junction nearest its centroid.
        let mut gate = vec![0u32; self.districts.len()];
        for (i, d) in self.districts.iter().enumerate() {
            let mut best = (0u32, f64::MAX);
            for (j, p) in self.jpos.iter().enumerate() {
                let dd = geom::dist2(*p, d.centroid);
                if dd < best.1 {
                    best = (u32::try_from(j).unwrap(), dd);
                }
            }
            gate[i] = best.0;
        }
        let mut streets = self.repo.streets.clone();
        streets.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)).then(a.1.cmp(&b.1)));
        streets.truncate(160);
        for (from, to, count) in streets {
            let fk = format!("{from}/·");
            let tk = format!("{to}/·");
            let (Some(&fi), Some(&ti)) = (key_to_d.get(fk.as_str()), key_to_d.get(tk.as_str()))
            else {
                continue;
            };
            if let Some(path) = dijkstra(&adj, gate[fi], gate[ti]) {
                let mut pts: Vec<Pt> = Vec::new();
                for &ci in &path {
                    let c = &self.chains[ci as usize];
                    let seg: Vec<Pt> = if pts.last().map_or(false, |l| {
                        geom::dist2(*l, c.pts[c.pts.len() - 1]) < geom::dist2(*l, c.pts[0])
                    }) {
                        c.pts.iter().rev().copied().collect()
                    } else {
                        c.pts.clone()
                    };
                    for p in seg {
                        if pts.last().map_or(true, |l| geom::dist2(*l, p) > 1e-9) {
                            pts.push(p);
                        }
                    }
                }
                if pts.len() > 1 {
                    self.street_paths.push((pts, count));
                }
            }
        }
    }
}

fn ccw_angle(from: Pt, to: Pt) -> f64 {
    let a = geom::cross(from, to).atan2(geom::dot(from, to));
    if a < 0.0 {
        a + std::f64::consts::TAU
    } else {
        a
    }
}

fn simplify_closed(poly: &[Pt], ds: &[f64], tol: f64) -> (Vec<Pt>, Vec<f64>) {
    // Keep a vertex when it survives Douglas-Peucker of the open chain, and
    // always keep vertices where the road class changes.
    let n = poly.len();
    let mut keep = vec![false; n];
    keep[0] = true;
    for i in 0..n {
        if (ds[i] - ds[(i + n - 1) % n]).abs() > 1e-9 {
            keep[i] = true;
        }
    }
    let anchors: Vec<usize> = (0..n).filter(|i| keep[*i]).collect();
    for w in 0..anchors.len() {
        let i = anchors[w];
        let j = anchors[(w + 1) % anchors.len()];
        let seg: Vec<Pt> = if j > i {
            poly[i..=j].to_vec()
        } else {
            poly[i..].iter().chain(poly[..=j].iter()).copied().collect()
        };
        let simp = geom::simplify(&seg, tol);
        let mut k = i;
        let mut si = 0;
        loop {
            if si < simp.len() && geom::dist2(poly[k % n], simp[si]) < 1e-12 {
                keep[k % n] = true;
                si += 1;
            }
            if k % n == j {
                break;
            }
            k += 1;
        }
    }
    let mut op = Vec::new();
    let mut od = Vec::new();
    for i in 0..n {
        if keep[i] {
            op.push(poly[i]);
            od.push(ds[i]);
        }
    }
    if op.len() < 3 {
        return (poly.to_vec(), ds.to_vec());
    }
    (op, od)
}

fn dijkstra(adj: &[Vec<(u32, f64, u32)>], s: u32, t: u32) -> Option<Vec<u32>> {
    #[derive(PartialEq)]
    struct E(f64, u32);
    impl Eq for E {}
    impl Ord for E {
        fn cmp(&self, o: &Self) -> std::cmp::Ordering {
            o.0.partial_cmp(&self.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(o.1.cmp(&self.1))
        }
    }
    impl PartialOrd for E {
        fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
            Some(self.cmp(o))
        }
    }
    let mut dist = vec![f64::MAX; adj.len()];
    let mut prev = vec![(NOLABEL, NOLABEL); adj.len()];
    let mut heap = BinaryHeap::new();
    dist[s as usize] = 0.0;
    heap.push(E(0.0, s));
    while let Some(E(d, u)) = heap.pop() {
        if u == t {
            break;
        }
        if d > dist[u as usize] {
            continue;
        }
        for &(v, w, ci) in &adj[u as usize] {
            let nd = d + w;
            if nd < dist[v as usize] - 1e-12 {
                dist[v as usize] = nd;
                prev[v as usize] = (u, ci);
                heap.push(E(nd, v));
            }
        }
    }
    if dist[t as usize] == f64::MAX {
        return None;
    }
    let mut out = Vec::new();
    let mut cur = t;
    while cur != s {
        let (p, ci) = prev[cur as usize];
        if p == NOLABEL {
            return None;
        }
        out.push(ci);
        cur = p;
    }
    out.reverse();
    Some(out)
}

// ---------------------------------------------------------------------------
// Raster helpers
// ---------------------------------------------------------------------------

/// 4-connected components of a boolean mask. Returns (component id per pixel,
/// id of the largest).
fn components(mask: &[bool], res: usize) -> (Vec<u32>, Option<u32>) {
    let mut comp = vec![NOLABEL; res * res];
    let mut sizes: Vec<u32> = Vec::new();
    let mut q = VecDeque::new();
    for start in 0..res * res {
        if !mask[start] || comp[start] != NOLABEL {
            continue;
        }
        let id = u32::try_from(sizes.len()).unwrap();
        sizes.push(0);
        comp[start] = id;
        q.push_back(start);
        while let Some(k) = q.pop_front() {
            sizes[id as usize] += 1;
            let (i, j) = (k % res, k / res);
            let mut push = |ni: usize, nj: usize, comp: &mut Vec<u32>, q: &mut VecDeque<usize>| {
                let nk = nj * res + ni;
                if mask[nk] && comp[nk] == NOLABEL {
                    comp[nk] = id;
                    q.push_back(nk);
                }
            };
            if i > 0 {
                push(i - 1, j, &mut comp, &mut q);
            }
            if i + 1 < res {
                push(i + 1, j, &mut comp, &mut q);
            }
            if j > 0 {
                push(i, j - 1, &mut comp, &mut q);
            }
            if j + 1 < res {
                push(i, j + 1, &mut comp, &mut q);
            }
        }
    }
    let best = sizes
        .iter()
        .enumerate()
        .max_by_key(|(i, s)| (**s, std::cmp::Reverse(*i)))
        .map(|(i, _)| u32::try_from(i).unwrap());
    (comp, best)
}

/// Keep only the largest connected component of every label; re-flood the rest
/// from their neighbours. Guarantees every district and every block is one
/// simply-tractable region.
fn cleanup_labels(lab: &mut [u32], res: usize) {
    let n = res * res;
    let mut comp = vec![NOLABEL; n];
    let mut owner: Vec<u32> = Vec::new();
    let mut sizes: Vec<u32> = Vec::new();
    let mut q = VecDeque::new();
    for start in 0..n {
        if lab[start] == NOLABEL || comp[start] != NOLABEL {
            continue;
        }
        let l = lab[start];
        let id = u32::try_from(sizes.len()).unwrap();
        sizes.push(0);
        owner.push(l);
        comp[start] = id;
        q.push_back(start);
        while let Some(k) = q.pop_front() {
            sizes[id as usize] += 1;
            let (i, j) = (k % res, k / res);
            let mut push = |ni: usize, nj: usize, comp: &mut Vec<u32>, q: &mut VecDeque<usize>| {
                let nk = nj * res + ni;
                if lab[nk] == l && comp[nk] == NOLABEL {
                    comp[nk] = id;
                    q.push_back(nk);
                }
            };
            if i > 0 {
                push(i - 1, j, &mut comp, &mut q);
            }
            if i + 1 < res {
                push(i + 1, j, &mut comp, &mut q);
            }
            if j > 0 {
                push(i, j - 1, &mut comp, &mut q);
            }
            if j + 1 < res {
                push(i, j + 1, &mut comp, &mut q);
            }
        }
    }
    let mut best: BTreeMap<u32, (u32, u32)> = BTreeMap::new();
    for (ci, &l) in owner.iter().enumerate() {
        let e = best.entry(l).or_insert((0, 0));
        if sizes[ci] > e.0 {
            *e = (sizes[ci], u32::try_from(ci).unwrap());
        }
    }
    let mut doomed = vec![false; n];
    for k in 0..n {
        if lab[k] != NOLABEL && comp[k] != best[&lab[k]].1 {
            doomed[k] = true;
        }
    }
    // Multi-source BFS from the surviving pixels.
    let mut q: VecDeque<usize> = VecDeque::new();
    let mut next = vec![NOLABEL; n];
    for k in 0..n {
        if doomed[k] {
            continue;
        }
        let (i, j) = (k % res, k / res);
        let mut chk = |ni: usize, nj: usize| -> bool { doomed[nj * res + ni] };
        let touch = (i > 0 && chk(i - 1, j))
            || (i + 1 < res && chk(i + 1, j))
            || (j > 0 && chk(i, j - 1))
            || (j + 1 < res && chk(i, j + 1));
        if touch {
            q.push_back(k);
        }
    }
    while let Some(k) = q.pop_front() {
        let src = if next[k] != NOLABEL { next[k] } else { lab[k] };
        let (i, j) = (k % res, k / res);
        let mut push = |ni: usize, nj: usize, q: &mut VecDeque<usize>, next: &mut Vec<u32>| {
            let nk = nj * res + ni;
            if doomed[nk] && next[nk] == NOLABEL {
                next[nk] = src;
                q.push_back(nk);
            }
        };
        if i > 0 {
            push(i - 1, j, &mut q, &mut next);
        }
        if i + 1 < res {
            push(i + 1, j, &mut q, &mut next);
        }
        if j > 0 {
            push(i, j - 1, &mut q, &mut next);
        }
        if j + 1 < res {
            push(i, j + 1, &mut q, &mut next);
        }
    }
    for k in 0..n {
        if doomed[k] {
            lab[k] = next[k];
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn walk_chain(
    si: usize,
    sj: usize,
    sdx: i64,
    sdy: i64,
    hedge: &[bool],
    vedge: &[bool],
    hused: &mut [bool],
    vused: &mut [bool],
    jid: &[u32],
    deg: &dyn Fn(usize, usize) -> u32,
    lab: &dyn Fn(i64, i64) -> u32,
) -> Option<Chain> {
    let vw = RES + 1;
    let edge_at = |i: usize, j: usize, dx: i64, dy: i64| -> Option<(bool, usize)> {
        if dx == 1 {
            if i < RES && hedge[j * RES + i] {
                return Some((true, j * RES + i));
            }
        } else if dx == -1 {
            if i > 0 && hedge[j * RES + i - 1] {
                return Some((true, j * RES + i - 1));
            }
        } else if dy == 1 {
            if j < RES && vedge[j * vw + i] {
                return Some((false, j * vw + i));
            }
        } else if dy == -1 && j > 0 && vedge[(j - 1) * vw + i] {
            return Some((false, (j - 1) * vw + i));
        }
        None
    };
    let (h0, e0) = edge_at(si, sj, sdx, sdy)?;
    if h0 && hused[e0] {
        return None;
    }
    if !h0 && vused[e0] {
        return None;
    }
    // Flanking labels, from the first edge and the direction of travel.
    let (left, right) = if h0 {
        let i = e0 % RES;
        let j = e0 / RES;
        let above = lab(i as i64, j as i64 - 1);
        let below = lab(i as i64, j as i64);
        if sdx == 1 {
            (above, below)
        } else {
            (below, above)
        }
    } else {
        let i = e0 % vw;
        let j = e0 / vw;
        let lft = lab(i as i64 - 1, j as i64);
        let rgt = lab(i as i64, j as i64);
        if sdy == 1 {
            (rgt, lft)
        } else {
            (lft, rgt)
        }
    };

    let mut pts = vec![geom::pt(si as f64 * CELL, sj as f64 * CELL)];
    let (mut i, mut j) = (si, sj);
    let (mut dx, mut dy) = (sdx, sdy);
    loop {
        let Some((h, e)) = edge_at(i, j, dx, dy) else {
            break;
        };
        if h {
            if hused[e] {
                break;
            }
            hused[e] = true;
        } else {
            if vused[e] {
                break;
            }
            vused[e] = true;
        }
        i = (i as i64 + dx) as usize;
        j = (j as i64 + dy) as usize;
        pts.push(geom::pt(i as f64 * CELL, j as f64 * CELL));
        if deg(i, j) != 2 {
            break;
        }
        // Degree 2: continue through, never back the way we came.
        let back = (-dx, -dy);
        let mut moved = false;
        for &(ndx, ndy) in &[(1i64, 0i64), (-1, 0), (0, 1), (0, -1)] {
            if (ndx, ndy) == back {
                continue;
            }
            if edge_at(i, j, ndx, ndy).is_some() {
                dx = ndx;
                dy = ndy;
                moved = true;
                break;
            }
        }
        if !moved {
            break;
        }
    }
    if pts.len() < 2 {
        return None;
    }
    let a = jid[sj * vw + si];
    let b = jid[j * vw + i];
    if a == NOLABEL || b == NOLABEL {
        return None;
    }
    Some(Chain {
        a,
        b,
        pts,
        left,
        right,
        class: RoadClass::Secondary,
        alive: true,
    })
}

/// Re-flood pixels carrying `marker` from their labelled neighbours.
fn fill_marked(lab: &mut [u32], res: usize, marker: u32) {
    let n = res * res;
    let mut q: VecDeque<usize> = VecDeque::new();
    let mut next = vec![NOLABEL; n];
    for k in 0..n {
        if lab[k] == marker || lab[k] == NOLABEL {
            continue;
        }
        let (i, j) = (k % res, k / res);
        let hit = (i > 0 && lab[k - 1] == marker)
            || (i + 1 < res && lab[k + 1] == marker)
            || (j > 0 && lab[k - res] == marker)
            || (j + 1 < res && lab[k + res] == marker);
        if hit {
            q.push_back(k);
        }
    }
    while let Some(k) = q.pop_front() {
        let src = if next[k] != NOLABEL { next[k] } else { lab[k] };
        let (i, j) = (k % res, k / res);
        let mut push = |nk: usize, q: &mut VecDeque<usize>, next: &mut Vec<u32>| {
            if lab[nk] == marker && next[nk] == NOLABEL {
                next[nk] = src;
                q.push_back(nk);
            }
        };
        if i > 0 {
            push(k - 1, &mut q, &mut next);
        }
        if i + 1 < res {
            push(k + 1, &mut q, &mut next);
        }
        if j > 0 {
            push(k - res, &mut q, &mut next);
        }
        if j + 1 < res {
            push(k + res, &mut q, &mut next);
        }
    }
    for k in 0..n {
        if lab[k] == marker {
            lab[k] = next[k];
        }
    }
}
