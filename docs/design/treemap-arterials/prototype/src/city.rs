//! The pipeline: directory tree -> space-filling subdivision -> arterials ->
//! blocks -> lots -> buildings -> streets.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};

use polis_events::LogicalPath;
use polis_layout::determinism::{fbm2_f64, simplex2_with_gradient, SeededRng};
use polis_repo::{FileMeta, RepoTree};

use crate::arr::{key, Arr, Class, FaceId, NodeId};
use crate::geom::{self, pt, Pt};

fn stage_check(tag: &str, arr: &Arr) {
    if std::env::var("STAGES").is_err() {
        return;
    }
    let segs: Vec<(NodeId, NodeId, Class)> =
        arr.edges.iter().map(|(&(a, b), i)| (a, b, i.class)).collect();
    let x = crate::metrics::count_crossings(&arr.nodes, &segs);
    let nonconvex = arr
        .live_faces()
        .iter()
        .filter(|&&f| {
            let p = arr.poly(f);
            p.len() >= 3 && !is_convex(&p)
        })
        .count();
    eprintln!(
        "[stage {tag}] nodes={} edges={} faces={} crossings={x} nonconvex_faces={nonconvex}",
        arr.nodes.len(),
        arr.edges.len(),
        arr.live_faces().len()
    );
    if x > 0 && std::env::var("DEEP").is_ok() {
        for i in 0..segs.len() {
            for j in (i + 1)..segs.len() {
                let (a1, b1, _) = segs[i];
                let (a2, b2, _) = segs[j];
                if a1 == a2 || a1 == b2 || b1 == a2 || b1 == b2 {
                    continue;
                }
                if !geom::segs_cross(
                    arr.nodes[a1 as usize],
                    arr.nodes[b1 as usize],
                    arr.nodes[a2 as usize],
                    arr.nodes[b2 as usize],
                ) {
                    continue;
                }
                let f1 = arr.edges.get(&key(a1, b1)).map(|e| e.faces.clone());
                let f2 = arr.edges.get(&key(a2, b2)).map(|e| e.faces.clone());
                eprintln!("  CROSS e({a1},{b1}) faces={f1:?}  e({a2},{b2}) faces={f2:?}");
                for f in f1.iter().flatten().chain(f2.iter().flatten()) {
                    let poly = arr.poly(*f);
                    eprintln!(
                        "    face {f} alive={} signed_area={:.2} convex={} ring={:?}",
                        arr.faces[*f as usize].alive,
                        geom::signed_area(&poly),
                        is_convex(&poly),
                        arr.faces[*f as usize].ring
                    );
                    for (k, v) in arr.faces[*f as usize].ring.iter().enumerate() {
                        eprintln!("       [{k}] n{v} = {:?}", arr.nodes[*v as usize]);
                    }
                }
                let neg = arr
                    .live_faces()
                    .iter()
                    .filter(|&&f| geom::signed_area(&arr.poly(f)) < 0.0)
                    .count();
                eprintln!("  faces with negative signed area: {neg}");
                return;
            }
        }
    }
}

fn is_convex(p: &[Pt]) -> bool {
    let a = geom::signed_area(p);
    let s = if a > 0.0 { 1.0 } else { -1.0 };
    for i in 0..p.len() {
        let u = p[(i + 1) % p.len()].sub(p[i]);
        let v = p[(i + 2) % p.len()].sub(p[(i + 1) % p.len()]);
        if u.cross(v) * s < -1e-6 {
            return false;
        }
    }
    true
}

/// How many blocks got street-fronting perimeter lots vs. the chord-split
/// fallback. Reported, because the fallback is what makes a block read as a fan
/// of triangles instead of a city block.
pub static RING_OK: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
pub static RING_FALLBACK: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// Blocks that could not be given any buildable ground at all.
pub static BLOCK_NO_GROUND: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Faces the retry ladder could not split. Reported, not swallowed.
pub static UNSPLITTABLE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

// ---------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Params {
    pub seed: u64,
    /// City rim vertex count.
    pub rim_verts: usize,
    /// City area per file, in city units. The city grows with the repository
    /// instead of being squeezed into a fixed disc, so a lot is the same size in
    /// a 90-file repo and a 5000-file one and the fabric reads the same at both.
    pub area_per_file: f64,
    /// Lots per block in the oldest / newest districts. Old towns have small
    /// dense blocks; new development has larger, more regular ones.
    pub lots_per_block_old: f64,
    pub lots_per_block_new: f64,
    /// Snap radius as a fraction of the face diameter — PRD §7.2's organic
    /// signature: a chord endpoint this close to an existing vertex reuses it,
    /// making a 4- or 5-way junction instead of two T-junctions.
    pub snap_frac: f64,
    /// Warp amplitude, in *lot* units (`sqrt(area_per_file)`), before the age
    /// multiplier. Expressed this way the warp bends a block by the same visible
    /// amount in a 90-file city and a 5000-file one.
    pub warp_amp: f64,
    /// Warp wavelength, in lot units. amp/wavelength stays well under 0.5 so the
    /// warp is a diffeomorphism and cannot introduce road crossings.
    pub warp_len: f64,
    /// Amplitude and wavelength of the city-scale bend applied to the arterials,
    /// as fractions of the city radius.
    pub arterial_bend: f64,
    pub arterial_bend_len: f64,
    /// Length an edge is subdivided to before warping.
    pub seg_len: f64,
    pub road_half_width: [f64; 4],
    pub setback: f64,
    /// Depth of the street-fronting lot ring, in city units.
    pub lot_depth: f64,
    /// How far a block outline may be straightened before lots are laid out.
    pub simplify_tol: f64,
    /// A directory becomes its own district only once its subtree reaches this
    /// many files; smaller subtrees are absorbed into the nearest ancestor
    /// district. `0` derives it from the repository size.
    ///
    /// Without this a 5000-file monorepo yields 600+ districts, which is both
    /// illegible as a wayfinding skeleton (PRD §8) and gives every block four
    /// buildings, so the fabric never reads as built-up.
    pub min_district_files: u32,
    /// Fraction of interior block edges dissolved to make irregular blocks.
    pub merge_p_old: f64,
    pub merge_p_new: f64,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            seed: 0x9E37_79B9_7F4A_7C15,
            rim_verts: 19,
            area_per_file: 275.0,
            lots_per_block_old: 11.0,
            lots_per_block_new: 19.0,
            snap_frac: 0.11,
            warp_amp: 0.22,
            warp_len: 8.0,
            arterial_bend: 0.045,
            arterial_bend_len: 0.55,
            seg_len: 7.0,
            road_half_width: [3.6, 3.2, 2.4, 1.5],
            setback: 1.0,
            lot_depth: 7.0,
            simplify_tol: 2.4,
            min_district_files: 0,
            merge_p_old: 0.22,
            merge_p_new: 0.07,
        }
    }
}

// ---------------------------------------------------------------------------
// Districts
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct DistrictInfo {
    pub path: LogicalPath,
    pub files_under: u32,
    pub direct_files: u32,
    /// 1.0 = oldest file in the repo, 0.0 = newest.
    pub oldness: f64,
    pub top: u32,
    pub depth: usize,
    /// Pre-warp polygon of the face this district was given.
    pub poly: Vec<Pt>,
    pub lots: u32,
}

/// Quantised subtree weight. Adding one file usually does not move the bucket,
/// so the arterial skeleton does not move either (PRD §7.7).
pub fn quant(n: u32) -> u32 {
    if n <= 24 {
        return n.max(1);
    }
    let bits = 32 - n.leading_zeros();
    let e = bits - 5; // keep 5 significant bits => <= ~3% steps
    let mask = (1u32 << e) - 1;
    let base = n >> e;
    let up = u32::from(n & mask != 0);
    (base + up) << e
}

/// Lots to survey in a district holding `n` files.
///
/// The headroom is what makes growth incremental: a new file takes a lot that
/// was already surveyed, so no road and no block moves (PRD §7.4, §7.7). The
/// spare lots are not waste — PRD §7.5 wants vacant lots on the map anyway.
pub fn lots_for(n: u32) -> u32 {
    let base = quant(n.max(1));
    base + (base / 6).max(2)
}

struct TreeNode {
    path: LogicalPath,
    children: Vec<usize>,
    direct: Vec<LogicalPath>,
    files_under: u32,
    oldest: u32,
    depth: usize,
    top: u32,
}

fn build_tree(repo: &RepoTree) -> Vec<TreeNode> {
    let mut index: BTreeMap<LogicalPath, usize> = BTreeMap::new();
    let mut nodes: Vec<TreeNode> = Vec::new();
    let root = LogicalPath::root();
    index.insert(root.clone(), 0);
    nodes.push(TreeNode {
        path: root,
        children: Vec::new(),
        direct: Vec::new(),
        files_under: 0,
        oldest: u32::MAX,
        depth: 0,
        top: u32::MAX,
    });
    // Ensure every ancestor directory exists, in path order (BTreeMap = stable).
    for path in repo.files.keys() {
        let mut chain: Vec<LogicalPath> = Vec::new();
        let mut cur = path.parent();
        while let Some(p) = cur {
            chain.push(p.clone());
            if p.is_root() {
                break;
            }
            cur = p.parent();
        }
        chain.reverse();
        for p in &chain {
            if index.contains_key(p) {
                continue;
            }
            let parent = p.parent().unwrap_or_else(LogicalPath::root);
            let pi = *index.get(&parent).expect("parent inserted first");
            let depth = p.depth();
            nodes.push(TreeNode {
                path: p.clone(),
                children: Vec::new(),
                direct: Vec::new(),
                files_under: 0,
                oldest: u32::MAX,
                depth,
                top: u32::MAX,
            });
            let id = nodes.len() - 1;
            index.insert(p.clone(), id);
            nodes[pi].children.push(id);
        }
        let parent = path.parent().unwrap_or_else(LogicalPath::root);
        let pi = *index.get(&parent).unwrap();
        nodes[pi].direct.push(path.clone());
    }
    // roll up counts and ages, deepest first
    let order: Vec<usize> = {
        let mut v: Vec<usize> = (0..nodes.len()).collect();
        v.sort_by_key(|&i| std::cmp::Reverse(nodes[i].depth));
        v
    };
    for &i in &order {
        let mut count = nodes[i].direct.len() as u32;
        let mut oldest = u32::MAX;
        for p in &nodes[i].direct {
            if let Some(m) = repo.files.get(p) {
                oldest = oldest.min(m.growth_index);
            }
        }
        let kids: Vec<usize> = nodes[i].children.clone();
        for k in kids {
            count += nodes[k].files_under;
            oldest = oldest.min(nodes[k].oldest);
        }
        nodes[i].files_under = count;
        nodes[i].oldest = oldest;
    }
    // top-level ancestor index, for colour
    let mut top_of: BTreeMap<LogicalPath, u32> = BTreeMap::new();
    let root_kids = nodes[0].children.clone();
    for (n, &k) in root_kids.iter().enumerate() {
        top_of.insert(nodes[k].path.clone(), n as u32);
    }
    for i in 0..nodes.len() {
        let first = nodes[i].path.components().next().map(str::to_owned);
        nodes[i].top = match first {
            Some(c) => LogicalPath::new(&c)
                .ok()
                .and_then(|p| top_of.get(&p).copied())
                .unwrap_or(0),
            None => u32::MAX,
        };
    }
    nodes
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Block {
    pub poly: Vec<Pt>,
    pub district: u32,
    /// Per-edge setback: the half-width of the road that runs along that edge,
    /// so a block on a boulevard gives up more ground than one on an alley and
    /// no building can sit under asphalt.
    pub edge_inset: Vec<f64>,
}

#[derive(Debug, Clone)]
pub struct Lot {
    pub poly: Vec<Pt>,
    pub block: u32,
    pub occupant: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct Building {
    pub poly: Vec<Pt>,
    pub path: LogicalPath,
    pub lot: u32,
    pub district: u32,
    pub height: f64,
    pub monument: bool,
}

#[derive(Debug, Clone)]
pub struct Street {
    pub poly: Vec<Pt>,
    pub weight: u32,
}

pub struct Layout {
    pub nodes: Vec<Pt>,
    pub segs: Vec<(NodeId, NodeId, Class)>,
    /// Parallel to `segs`: true when the segment separates two districts (or the
    /// city from the outside). These are the arterials.
    pub seg_is_border: Vec<bool>,
    pub blocks: Vec<Block>,
    /// Block courtyards: the ground the perimeter ring encloses.
    pub gardens: Vec<Vec<Pt>>,
    pub lots: Vec<Lot>,
    pub buildings: Vec<Building>,
    pub districts: Vec<DistrictInfo>,
    pub streets: Vec<Street>,
    pub files: Vec<LogicalPath>,
    /// Kept so an incremental add can fill a vacant lot without rebuilding.
    pub district_of_file: BTreeMap<LogicalPath, u32>,
    pub lots_of_district: BTreeMap<u32, Vec<u32>>,
    pub district_borders: BTreeSet<(u32, u32)>,
}

// ---------------------------------------------------------------------------
// Terrain + warp
// ---------------------------------------------------------------------------

struct Field {
    seed: u64,
    /// Coarse raster of "oldness" over the pre-warp plane.
    old_grid: Vec<f32>,
    gw: usize,
    lo: Pt,
    cell: f64,
    p: Params,
    /// The lot scale, `sqrt(area_per_file)`. Warp lengths are multiples of it.
    r: f64,
    /// The city radius, for the long-wavelength octave that bends the arterials.
    city_r: f64,
}

impl Field {
    fn terrain_gradient(&self, p: Pt, depth_bias: f64) -> Pt {
        let s = 520.0;
        let (_, dx, dy) = simplex2_with_gradient(self.seed ^ 0x51_ED_2701, p.x / s, p.y / s);
        let g = pt(dx, dy).norm();
        // deeper directories read as higher ground (PRD §7.2)
        let r = pt(p.x, p.y).norm();
        g.add(r.mul(depth_bias * 0.5)).norm()
    }

    fn oldness(&self, p: Pt) -> f64 {
        let fx = (p.x - self.lo.x) / self.cell;
        let fy = (p.y - self.lo.y) / self.cell;
        let ix = fx.floor();
        let iy = fy.floor();
        let tx = fx - ix;
        let ty = fy - iy;
        let sample = |gx: i64, gy: i64| -> f64 {
            if gx < 0 || gy < 0 || gx as usize >= self.gw || gy as usize >= self.gw {
                return 0.0;
            }
            f64::from(self.old_grid[gy as usize * self.gw + gx as usize])
        };
        let (ix, iy) = (ix as i64, iy as i64);
        let a = sample(ix, iy);
        let b = sample(ix + 1, iy);
        let c = sample(ix, iy + 1);
        let d = sample(ix + 1, iy + 1);
        let top = a + (b - a) * tx;
        let bot = c + (d - c) * tx;
        top + (bot - top) * ty
    }

    /// A smooth, low-gradient displacement. Because amplitude/wavelength stays
    /// under ~0.15 per octave the map is a diffeomorphism: it bends roads and
    /// cannot make them cross.
    fn warp(&self, p: Pt) -> Pt {
        let o = self.oldness(p);
        let amp = self.p.warp_amp * self.r * (0.45 + 1.25 * o);
        let l1 = self.p.warp_len * self.r;
        let dx1 = fbm2_f64(self.seed ^ 0xA1, p.x / l1, p.y / l1, 2);
        let dy1 = fbm2_f64(self.seed ^ 0xB2, p.x / l1, p.y / l1, 2);
        let l2 = l1 * 0.40;
        let dx2 = fbm2_f64(self.seed ^ 0xC3, p.x / l2, p.y / l2, 2);
        let dy2 = fbm2_f64(self.seed ^ 0xD4, p.x / l2, p.y / l2, 2);
        // A third, city-scale octave. Without it the top-level chords stay dead
        // straight all the way across the map, which is exactly the circuit-board
        // reading a treemap has to escape. Its wavelength is half the city, so
        // its gradient is tiny and it cannot make roads cross.
        let l3 = self.city_r * self.p.arterial_bend_len;
        let a3 = self.city_r * self.p.arterial_bend;
        let dx3 = fbm2_f64(self.seed ^ 0xE5, p.x / l3, p.y / l3, 1);
        let dy3 = fbm2_f64(self.seed ^ 0xF6, p.x / l3, p.y / l3, 1);
        pt(
            p.x + dx1 * amp + dx2 * amp * 0.30 + dx3 * a3,
            p.y + dy1 * amp + dy2 * amp * 0.30 + dy3 * a3,
        )
    }
}

// ---------------------------------------------------------------------------
// Build
// ---------------------------------------------------------------------------

fn rim_polygon(p: &Params, radius: f64) -> Vec<Pt> {
    let mut rng = SeededRng::for_seed(p.seed, "city-rim");
    let n = p.rim_verts;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let a = std::f64::consts::TAU * (i as f64) / (n as f64);
        let d = geom::rotate(pt(1.0, 0.0), a);
        let wob = fbm2_f64(p.seed ^ 0x5EED, d.x * 1.9, d.y * 1.9, 2);
        let r = radius * (1.0 + 0.26 * wob + 0.09 * (rng.next_f64() - 0.5));
        out.push(pt(d.x * r, d.y * r * 0.92));
    }
    // Convexify: every face in the subdivision is then convex, and a chord
    // between two points of a convex face cannot leave it. That is the planarity
    // guarantee the whole design rests on.
    geom::convex_hull(&out)
}

pub fn build(repo: &RepoTree, params: &Params) -> Layout {
    let tree = build_tree(repo);
    // The city is sized to its content, so blocks, lots and roads have the same
    // absolute size at 90 files and at 5000.
    let radius =
        (repo.files.len().max(8) as f64 * params.area_per_file / std::f64::consts::PI).sqrt();
    let max_growth = repo
        .files
        .values()
        .map(|f| f.growth_index)
        .filter(|&g| g != u32::MAX)
        .max()
        .unwrap_or(1)
        .max(1);

    let mut params = params.clone();
    if params.min_district_files == 0 {
        params.min_district_files =
            (((repo.files.len() as f64).sqrt() * 0.70).round() as u32).max(4);
    }
    let params = &params;

    let mut arr = Arr::from_ring(rim_polygon(params, radius));
    let mut districts: Vec<DistrictInfo> = Vec::new();

    // --- 1. Districts tile the plane; their shared borders are the arterials.
    place_district(
        &mut arr,
        0,
        &tree,
        0,
        max_growth,
        params,
        &mut districts,
        Class::Boulevard,
    );

    stage_check("districts", &arr);

    // Every file belongs to the deepest district that is a prefix of its path.
    let district_index: BTreeMap<LogicalPath, u32> = districts
        .iter()
        .enumerate()
        .map(|(i, d)| (d.path.clone(), i as u32))
        .collect();
    let mut district_of_file: BTreeMap<LogicalPath, u32> = BTreeMap::new();
    let mut counts = vec![0u32; districts.len()];
    for path in repo.files.keys() {
        let mut cur = path.parent();
        while let Some(p) = cur {
            if let Some(&i) = district_index.get(&p) {
                district_of_file.insert(path.clone(), i);
                counts[i as usize] += 1;
                break;
            }
            if p.is_root() {
                break;
            }
            cur = p.parent();
        }
    }
    for (i, d) in districts.iter_mut().enumerate() {
        d.direct_files = counts[i];
        d.lots = lots_for(counts[i]);
    }

    // --- 2. Oldness raster (pre-warp), used by the warp and by block sizing.
    let rim = arr
        .faces
        .iter()
        .filter(|f| f.alive)
        .flat_map(|f| f.ring.iter().map(|&n| arr.nodes[n as usize]))
        .collect::<Vec<_>>();
    let (lo, hi) = geom::bounds(&rim);
    let gw = 160usize;
    let cell = ((hi.x - lo.x).max(hi.y - lo.y) * 1.02) / gw as f64;
    let lo = pt(lo.x - cell, lo.y - cell);
    let mut old_grid = vec![0f32; gw * gw];
    for d in &districts {
        rasterize(&mut old_grid, gw, lo, cell, &d.poly, d.oldness as f32);
    }
    let field = Field {
        seed: params.seed,
        old_grid,
        gw,
        lo,
        cell,
        p: params.clone(),
        r: params.area_per_file.sqrt(),
        city_r: radius,
    };

    // --- 3. Blocks: subdivide inside each district until a block holds roughly
    // the target number of lots. Block count follows the file count, not the
    // frame, which is what keeps the fabric dense at both scales.
    let target_blocks = ((repo.files.len() as f64 / 8.0).round() as usize)
        .max(districts.len())
        .min(900);
    let per_base =
        (repo.files.len() as f64 / target_blocks as f64).clamp(4.0, params.lots_per_block_new);
    for di in 0..districts.len() {
        let d = districts[di].clone();
        let per = per_base * (1.22 - 0.44 * d.oldness);
        // Blocks follow the file count *and* the ground: a district with a big
        // face and few files still gets city-sized blocks (the spare ones become
        // open ground) rather than one enormous polygon.
        let by_files = (f64::from(d.direct_files.max(1)) / per).ceil() as i64;
        let by_area = (geom::area(&d.poly) / (params.area_per_file * per)).round() as i64;
        let want = by_files.max(by_area).clamp(1, 400) as usize;
        subdivide_blocks(&mut arr, di as u32, want, &d, params, &field);
    }

    stage_check("blocks", &arr);

    // --- 4. Dissolve some interior edges: irregular, non-convex blocks.
    dissolve(&mut arr, &districts, params);

    stage_check("dissolve", &arr);

    // --- 5. Curve the roads: subdivide, then warp every node once.
    arr.subdivide_edges(params.seg_len, 6);
    stage_check("subdivided", &arr);
    for i in 0..arr.nodes.len() {
        arr.nodes[i] = field.warp(arr.nodes[i]);
    }
    arr.compact();
    stage_check("warped", &arr);
    for d in districts.iter_mut() {
        d.poly = d.poly.iter().map(|&p| field.warp(p)).collect();
    }

    // --- 6. Extract roads and blocks. A district border is exactly an edge with
    // two different districts on its sides (or only one side at all) — the
    // arterial network is read straight off the tiling.
    let mut district_borders: BTreeSet<(u32, u32)> = BTreeSet::new();
    let mut border_key: BTreeSet<(NodeId, NodeId)> = BTreeSet::new();
    for (&k, info) in &arr.edges {
        let ds: Vec<u32> = info
            .faces
            .iter()
            .filter(|&&f| arr.faces[f as usize].alive)
            .map(|&f| arr.faces[f as usize].district)
            .collect();
        if ds.len() < 2 {
            border_key.insert(k);
        } else if ds[0] != ds[1] {
            border_key.insert(k);
            if ds[0] != u32::MAX && ds[1] != u32::MAX {
                district_borders.insert((ds[0].min(ds[1]), ds[0].max(ds[1])));
            }
        }
    }
    let mut segs: Vec<(NodeId, NodeId, Class)> = arr
        .edges
        .iter()
        .map(|(&(a, b), i)| (a, b, i.class))
        .collect();
    segs.sort_by_key(|&(a, b, c)| (a, b, c as u8));
    let seg_is_border: Vec<bool> = segs
        .iter()
        .map(|&(a, b, _)| border_key.contains(&key(a, b)))
        .collect();

    let mut blocks: Vec<Block> = Vec::new();
    for f in arr.live_faces() {
        let poly = arr.poly(f);
        if poly.len() < 3 || geom::area(&poly) < 1.0 {
            continue;
        }
        let d = arr.faces[f as usize].district;
        if d == u32::MAX {
            continue;
        }
        let ring = &arr.faces[f as usize].ring;
        let edge_inset: Vec<f64> = (0..ring.len())
            .map(|i| {
                let a = ring[i];
                let b = ring[(i + 1) % ring.len()];
                let cl = arr
                    .edges
                    .get(&key(a, b))
                    .map_or(Class::Street, |e| e.class);
                params.road_half_width[cl as usize] + params.setback
            })
            .collect();
        blocks.push(Block {
            poly,
            district: d,
            edge_inset,
        });
    }

    // --- 7. Lots, then buildings.
    let (lots, buildings, files, lots_of_district, gardens) =
        parcel(&blocks, &districts, repo, &district_of_file, params);

    Layout {
        nodes: arr.nodes.clone(),
        segs,
        seg_is_border,
        blocks,
        gardens,
        lots,
        buildings,
        districts,
        streets: Vec::new(),
        files,
        district_of_file,
        lots_of_district,
        district_borders,
    }
}

fn rasterize(grid: &mut [f32], gw: usize, lo: Pt, cell: f64, poly: &[Pt], v: f32) {
    if poly.len() < 3 {
        return;
    }
    let (plo, phi) = geom::bounds(poly);
    let y0 = (((plo.y - lo.y) / cell).floor() as i64).max(0);
    let y1 = (((phi.y - lo.y) / cell).ceil() as i64).min(gw as i64 - 1);
    for gy in y0..=y1 {
        let y = lo.y + (gy as f64 + 0.5) * cell;
        let mut xs: Vec<f64> = Vec::new();
        for i in 0..poly.len() {
            let a = poly[i];
            let b = poly[(i + 1) % poly.len()];
            if (a.y > y) != (b.y > y) {
                let t = (y - a.y) / (b.y - a.y);
                xs.push(a.x + t * (b.x - a.x));
            }
        }
        xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
        for pair in xs.chunks(2) {
            if pair.len() < 2 {
                break;
            }
            let x0 = (((pair[0] - lo.x) / cell).floor() as i64).max(0);
            let x1 = (((pair[1] - lo.x) / cell).ceil() as i64).min(gw as i64 - 1);
            for gx in x0..=x1 {
                grid[gy as usize * gw + gx as usize] = v;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn place_district(
    arr: &mut Arr,
    node: usize,
    tree: &[TreeNode],
    face: FaceId,
    max_growth: u32,
    params: &Params,
    out: &mut Vec<DistrictInfo>,
    class: Class,
) {
    let n = &tree[node];
    // Items competing for this face: each child subtree, plus this directory's
    // own files as a pseudo-child.
    struct Item {
        weight: u32,
        oldest: u32,
        child: Option<usize>,
    }
    let mut items: Vec<Item> = Vec::new();
    // Subtrees below the threshold are absorbed into this directory's own
    // district rather than becoming districts of their own.
    let mut absorbed = 0u32;
    let mut absorbed_oldest = u32::MAX;
    for &c in &n.children {
        if tree[c].files_under == 0 {
            continue;
        }
        if tree[c].files_under < params.min_district_files {
            absorbed += tree[c].files_under;
            absorbed_oldest = absorbed_oldest.min(tree[c].oldest);
            continue;
        }
        items.push(Item {
            weight: quant(tree[c].files_under),
            oldest: tree[c].oldest,
            child: Some(c),
        });
    }
    let direct = n.direct.len() as u32 + absorbed;
    if direct > 0 || items.is_empty() {
        let mut oldest = absorbed_oldest;
        for p in &n.direct {
            let _ = p;
            oldest = oldest.min(n.oldest);
        }
        if oldest == u32::MAX {
            oldest = n.oldest;
        }
        items.push(Item {
            weight: quant(direct.max(1)),
            oldest,
            child: None,
        });
    }
    // Oldest first: the older half is always given the origin-ward sub-face, so
    // the old town ends up at the civic square (PRD §7.1).
    items.sort_by_key(|i| {
        (
            i.oldest,
            i.child.map(|c| tree[c].path.as_str().to_owned()),
        )
    });

    place_items(
        arr, &items, tree, face, node, max_growth, params, out, class,
    );

    #[allow(clippy::too_many_arguments)]
    fn place_items(
        arr: &mut Arr,
        items: &[Item],
        tree: &[TreeNode],
        face: FaceId,
        node: usize,
        max_growth: u32,
        params: &Params,
        out: &mut Vec<DistrictInfo>,
        class: Class,
    ) {
        if items.is_empty() {
            return;
        }
        if items.len() == 1 {
            match items[0].child {
                Some(c) => {
                    let next = if tree[c].depth <= 1 {
                        Class::Boulevard
                    } else {
                        Class::Avenue
                    };
                    place_district(arr, c, tree, face, max_growth, params, out, next);
                }
                None => {
                    let n = &tree[node];
                    let poly = arr.poly(face);
                    let idx = out.len() as u32;
                    arr.faces[face as usize].district = idx;
                    let oldness = 1.0
                        - (f64::from(n.oldest.min(max_growth)) / f64::from(max_growth))
                            .clamp(0.0, 1.0);
                    out.push(DistrictInfo {
                        path: n.path.clone(),
                        files_under: n.files_under,
                        direct_files: n.direct.len() as u32,
                        oldness,
                        top: n.top,
                        depth: n.depth,
                        poly,
                        lots: lots_for(n.direct.len() as u32),
                    });
                }
            }
            return;
        }
        let total: u32 = items.iter().map(|i| i.weight).sum();
        // Balanced prefix split, keeping the (age, path) order intact.
        let mut acc = 0u32;
        let mut k = 1usize;
        let mut best = i64::MAX;
        for i in 0..items.len() - 1 {
            acc += items[i].weight;
            let imb = (2 * i64::from(acc) - i64::from(total)).abs();
            if imb < best {
                best = imb;
                k = i + 1;
            }
        }
        let left_w: u32 = items[..k].iter().map(|i| i.weight).sum();
        let ratio = (f64::from(left_w) / f64::from(total.max(1))).clamp(0.12, 0.88);

        let poly = arr.poly(face);
        if poly.len() < 3 {
            return;
        }
        let c = geom::centroid(&poly);
        let base = geom::longest_axis(&poly);
        // A road follows the contour, so its normal leans toward the gradient.
        let s = 520.0;
        let (_, gx, gy) = simplex2_with_gradient(params.seed ^ 0x51_ED_2701, c.x / s, c.y / s);
        let mut g = pt(gx, gy).norm();
        if g.dot(base) < 0.0 {
            g = g.mul(-1.0);
        }
        let oldness = 1.0
            - (f64::from(items[0].oldest.min(max_growth)) / f64::from(max_growth)).clamp(0.0, 1.0);
        let base = base.add(g.mul(0.30)).norm();
        let mut rng = SeededRng::for_path(&tree[node].path, "district-split");
        let jitter = 0.09 + 0.52 * oldness;
        let delta = (rng.next_f64() * 2.0 - 1.0) * jitter;
        let diam = geom::diameter(&poly);

        // Bounded retry ladder: rotate the cut and shrink the snap radius until
        // the chord lands cleanly. A face with three or more vertices and real
        // area always yields on one of these; the loop is what guarantees
        // termination, which the first version of this code did not have.
        let mut normal = pt(0.0, 0.0);
        let mut off = 0.0;
        let mut split = None;
        for attempt in 0..9u32 {
            let extra = f64::from(attempt) * 0.41;
            let mut n = geom::rotate(base, delta + extra).norm();
            if c.dot(n) < 0.0 {
                n = n.mul(-1.0);
            }
            let o = crate::arr::offset_for_area_ratio(&poly, n, ratio);
            let snap = diam * params.snap_frac * 0.5f64.powi(attempt as i32);
            if let Some(r) = arr.split_face(face, n, o, snap, class) {
                normal = n;
                off = o;
                split = Some(r);
                break;
            }
        }
        let Some((fa, fb)) = split else {
            // Should not happen; if it does, the face becomes one leaf district
            // rather than a stack overflow. Counted by `unsplittable`.
            UNSPLITTABLE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            place_items(
                arr,
                &items[..1],
                tree,
                face,
                node,
                max_growth,
                params,
                out,
                class,
            );
            return;
        };
        let ca = geom::centroid(&arr.poly(fa));
        let (near, far) = if ca.dot(normal) <= off { (fa, fb) } else { (fb, fa) };
        let deeper = match class {
            Class::Boulevard => Class::Boulevard,
            _ => Class::Avenue,
        };
        place_items(
            arr,
            &items[..k],
            tree,
            near,
            node,
            max_growth,
            params,
            out,
            deeper,
        );
        place_items(
            arr,
            &items[k..],
            tree,
            far,
            node,
            max_growth,
            params,
            out,
            deeper,
        );
    }
}

fn subdivide_blocks(
    arr: &mut Arr,
    district: u32,
    want: usize,
    d: &DistrictInfo,
    params: &Params,
    _field: &Field,
) {
    if want <= 1 {
        return;
    }
    let mut queue: Vec<FaceId> = arr
        .live_faces()
        .into_iter()
        .filter(|&f| arr.faces[f as usize].district == district)
        .collect();
    let mut made = queue.len();
    let mut step = 0u64;
    while made < want {
        // Always split the largest live face: gives an even block texture and a
        // deterministic order independent of queue insertion history.
        queue.retain(|&f| arr.faces[f as usize].alive);
        if queue.is_empty() {
            break;
        }
        let mut best = queue[0];
        let mut best_a = -1.0;
        for &f in &queue {
            let a = geom::area(&arr.poly(f));
            if a > best_a {
                best_a = a;
                best = f;
            }
        }
        if best_a < params.area_per_file * 0.85 {
            break;
        }
        let poly = arr.poly(best);
        let mut rng = SeededRng::for_path_indexed(&d.path, "block-split", step);
        step += 1;
        let mut normal = geom::longest_axis(&poly);
        let jitter = 0.07 + 0.60 * d.oldness;
        normal = geom::rotate(normal, (rng.next_f64() * 2.0 - 1.0) * jitter).norm();
        let spread = 0.06 + 0.20 * d.oldness;
        let ratio = 0.5 + (rng.next_f64() * 2.0 - 1.0) * spread;
        let off = crate::arr::offset_for_area_ratio(&poly, normal, ratio);
        let snap = geom::diameter(&poly) * (params.snap_frac * (0.7 + 0.9 * d.oldness));
        match arr.split_face(best, normal, off, snap, Class::Street) {
            Some((fa, fb)) => {
                queue.push(fa);
                queue.push(fb);
                made += 1;
            }
            None => {
                // try once more with a different angle, else give up on this face
                let normal2 = geom::rotate(normal, 0.7).norm();
                let off2 = crate::arr::offset_for_area_ratio(&poly, normal2, 0.5);
                match arr.split_face(best, normal2, off2, snap * 0.4, Class::Street) {
                    Some((fa, fb)) => {
                        queue.push(fa);
                        queue.push(fb);
                        made += 1;
                    }
                    None => {
                        queue.retain(|&f| f != best);
                    }
                }
            }
        }
        if step > (want as u64 + 64) * 4 {
            break;
        }
    }
}

fn dissolve(arr: &mut Arr, districts: &[DistrictInfo], params: &Params) {
    let candidates: Vec<(NodeId, NodeId)> = arr
        .edges
        .iter()
        .filter(|(_, i)| i.class == Class::Street && i.faces.len() == 2)
        .map(|(&k, _)| k)
        .collect();
    let deg = arr.degrees();
    let mut deg = deg;
    for (a, b) in candidates {
        let Some(info) = arr.edges.get(&(a, b)) else {
            continue;
        };
        if info.faces.len() != 2 {
            continue;
        }
        let (f1, f2) = (info.faces[0], info.faces[1]);
        if !arr.faces[f1 as usize].alive || !arr.faces[f2 as usize].alive {
            continue;
        }
        let d1 = arr.faces[f1 as usize].district;
        let d2 = arr.faces[f2 as usize].district;
        if d1 != d2 || d1 == u32::MAX {
            continue;
        }
        // Keep minimum degree 2: never create a dangling road stub.
        if deg[a as usize] < 4 || deg[b as usize] < 4 {
            continue;
        }
        let old = districts[d1 as usize].oldness;
        let p = params.merge_p_new + (params.merge_p_old - params.merge_p_new) * old;
        let mut rng = SeededRng::for_seed(
            polis_layout::determinism::combine_seeds(u64::from(a), u64::from(b)),
            "dissolve",
        );
        if rng.next_f64() >= p {
            continue;
        }
        if arr.remove_edge(a, b).is_some() {
            deg[a as usize] -= 1;
            deg[b as usize] -= 1;
        }
    }
}

// ---------------------------------------------------------------------------
// Lots + buildings
// ---------------------------------------------------------------------------

type Parcelled = (
    Vec<Lot>,
    Vec<Building>,
    Vec<LogicalPath>,
    BTreeMap<u32, Vec<u32>>,
    Vec<Vec<Pt>>,
);

fn parcel(
    blocks: &[Block],
    districts: &[DistrictInfo],
    repo: &RepoTree,
    district_of_file: &BTreeMap<LogicalPath, u32>,
    params: &Params,
) -> Parcelled {
    let mut by_district: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for (i, b) in blocks.iter().enumerate() {
        by_district.entry(b.district).or_default().push(i as u32);
    }
    let mut lots: Vec<Lot> = Vec::new();
    let mut gardens: Vec<Vec<Pt>> = Vec::new();
    let mut lots_of_district: BTreeMap<u32, Vec<u32>> = BTreeMap::new();

    for (&di, bids) in &by_district {
        let d = &districts[di as usize];
        let mut buildable: Vec<(u32, Vec<Pt>, f64)> = Vec::new();
        for &b in bids {
            let blk = &blocks[b as usize];
            let (sp, sd) = geom::simplify_with(&blk.poly, &blk.edge_inset, params.simplify_tol);
            if let Some(p) = best_inset(&sp, &sd) {
                let a = geom::area(&p);
                if a > params.area_per_file * 0.10 {
                    buildable.push((b, p, a));
                }
            }
        }
        if buildable.is_empty() {
            // Every block was swallowed by its setbacks: fall back to the
            // narrowest one so the district's files still get ground.
            for &b in bids {
                let blk = &blocks[b as usize];
                let (sp, sd) = geom::simplify_with(&blk.poly, &blk.edge_inset, params.simplify_tol);
                let thin: Vec<f64> = sd.iter().map(|d| d * 0.85).collect();
                if let Some(p) = best_inset(&sp, &thin) {
                    if geom::area(&p) > 4.0 {
                        let a = geom::area(&p);
                        buildable.push((b, p, a));
                    }
                }
            }
        }
        if buildable.is_empty() {
            // Still nothing: shrink the biggest block toward its centroid so the
            // district's files are never silently dropped.
            if let Some(&b) = bids.iter().max_by(|&&x, &&y| {
                geom::area(&blocks[x as usize].poly)
                    .partial_cmp(&geom::area(&blocks[y as usize].poly))
                    .unwrap_or(Ordering::Equal)
            }) {
                let poly = &blocks[b as usize].poly;
                let c = geom::centroid(poly);
                let p: Vec<Pt> = poly.iter().map(|q| c.add(q.sub(c).mul(0.72))).collect();
                if p.len() >= 3 && geom::area(&p) > 1.0 {
                    let a = geom::area(&p);
                    buildable.push((b, p, a));
                }
            }
        }
        if buildable.is_empty() {
            BLOCK_NO_GROUND.fetch_add(bids.len(), std::sync::atomic::Ordering::Relaxed);
            continue;
        }
        BLOCK_NO_GROUND.fetch_add(
            bids.len().saturating_sub(buildable.len()),
            std::sync::atomic::Ordering::Relaxed,
        );
        // Deterministic order: nearest the civic square first, so the oldest
        // files (assigned first) land closest to the centre.
        buildable.sort_by(|x, y| {
            let cx = geom::centroid(&x.1);
            let cy = geom::centroid(&y.1);
            cx.len()
                .partial_cmp(&cy.len())
                .unwrap_or(Ordering::Equal)
                .then(x.0.cmp(&y.0))
        });
        let want = d.lots.max(d.direct_files) as usize;
        let total_area: f64 = buildable.iter().map(|b| b.2).sum();
        // Largest-remainder apportionment.
        let mut alloc: Vec<usize> = Vec::with_capacity(buildable.len());
        let mut rem: Vec<(f64, usize)> = Vec::new();
        let mut used = 0usize;
        for (i, b) in buildable.iter().enumerate() {
            let exact = want as f64 * b.2 / total_area;
            let f = exact.floor();
            alloc.push(f as usize);
            used += f as usize;
            rem.push((exact - f, i));
        }
        rem.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(Ordering::Equal)
                .then(a.1.cmp(&b.1))
        });
        let mut ri = 0usize;
        while used < want && !rem.is_empty() {
            alloc[rem[ri % rem.len()].1] += 1;
            used += 1;
            ri += 1;
        }
        let mut ids: Vec<u32> = Vec::new();
        #[allow(unused_mut)]
        for (i, (bid, poly, _)) in buildable.iter().enumerate() {
            let k = alloc[i];
            if k == 0 {
                continue;
            }
            let mut rng = SeededRng::for_path_indexed(&d.path, "lots", u64::from(*bid));
            // Ring depth follows the block: a deep block gets a deep ring, so
            // the courtyard stays a yard instead of swallowing the block.
            let depth = params.lot_depth * 3.4 * (1.0 - 0.18 * d.oldness);
            // A block with only a handful of lots has no business having a
            // courtyard: ring it only when there are enough lots to make a
            // frontage, otherwise subdivide the whole block (PRD §7.2 step 4).
            let ring = if k >= 5 {
                perimeter_lots(poly, k, depth)
            } else {
                None
            };
            let parts = match ring {
                Some((p, court)) => {
                    RING_OK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if geom::area(&court) > 6.0 {
                        gardens.push(court);
                    }
                    p
                }
                None => {
                    RING_FALLBACK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    split_into(poly, k, &mut rng, d.oldness)
                }
            };
            for p in parts {
                if p.len() < 3 || geom::area(&p) < 4.0 {
                    continue;
                }
                ids.push(lots.len() as u32);
                lots.push(Lot {
                    poly: p,
                    block: *bid,
                    occupant: None,
                });
            }
        }
        // Guarantee one lot per file: split the largest lot until there are
        // enough. Without this a degenerate clip silently loses a building.
        let need = d.direct_files as usize;
        let mut guard = 0;
        while ids.len() < need && guard < need * 4 + 16 {
            guard += 1;
            let Some(&biggest) = ids
                .iter()
                .max_by(|&&a, &&b| {
                    geom::area(&lots[a as usize].poly)
                        .partial_cmp(&geom::area(&lots[b as usize].poly))
                        .unwrap_or(Ordering::Equal)
                })
            else {
                break;
            };
            let poly = lots[biggest as usize].poly.clone();
            let blk = lots[biggest as usize].block;
            let mut rng = SeededRng::for_path_indexed(&d.path, "lot-topup", guard as u64);
            let mut parts = split_into(&poly, 2, &mut rng, d.oldness);
            if parts.len() < 2 {
                // Guaranteed-progress fallback: bisect through the centroid
                // along the polygon's own longest axis.
                let n = geom::longest_axis(&poly);
                let off = geom::centroid(&poly).dot(n);
                let l = geom::clip_halfplane(&poly, n, off);
                let r = geom::clip_halfplane(&poly, n.mul(-1.0), -off);
                if l.len() < 3 || r.len() < 3 {
                    break;
                }
                parts = vec![l, r];
            }
            lots[biggest as usize].poly = parts[0].clone();
            ids.push(lots.len() as u32);
            lots.push(Lot {
                poly: parts[1].clone(),
                block: blk,
                occupant: None,
            });
        }
        lots_of_district.insert(di, ids);
    }

    // Files to lots: oldest first, so the age gradient is visible inside a
    // district as well as across the city.
    let mut files: Vec<LogicalPath> = Vec::new();
    let mut buildings: Vec<Building> = Vec::new();

    let mut per_district: BTreeMap<u32, Vec<&FileMeta>> = BTreeMap::new();
    for m in repo.files.values() {
        if let Some(&di) = district_of_file.get(&m.path) {
            per_district.entry(di).or_default().push(m);
        }
    }
    for (di, metas) in per_district.iter_mut() {
        metas.sort_by(|a, b| {
            a.growth_index
                .cmp(&b.growth_index)
                .then(a.path.cmp(&b.path))
        });
        let empty = Vec::new();
        let ids = lots_of_district.get(di).unwrap_or(&empty).clone();
        for (i, m) in metas.iter().enumerate() {
            if i >= ids.len() {
                continue;
            }
            let lid = ids[i];
            lots[lid as usize].occupant = Some(files.len());
            let poly = building_poly(&lots[lid as usize].poly, m, params);
            let height = footprint_height(m);
            let monument = polis_repo::tree::entry_point(&m.path).is_some();
            buildings.push(Building {
                poly,
                path: m.path.clone(),
                lot: lid,
                district: *di,
                height,
                monument,
            });
            files.push(m.path.clone());
        }
    }
    // Safety net: PRD §7.2 makes a building per file non-negotiable, so a file
    // whose own district ended up with no usable ground is rehoused in the
    // nearest vacant lot rather than silently dropped.
    let placed: BTreeSet<LogicalPath> = buildings.iter().map(|b| b.path.clone()).collect();
    let homeless: Vec<&FileMeta> = repo
        .files
        .values()
        .filter(|m| district_of_file.contains_key(&m.path) && !placed.contains(&m.path))
        .collect();
    for m in homeless {
        let di = district_of_file[&m.path];
        let want = geom::centroid(&districts[di as usize].poly);
        let mut best: Option<(f64, u32)> = None;
        for (i, l) in lots.iter().enumerate() {
            if l.occupant.is_some() || l.poly.len() < 3 {
                continue;
            }
            let d = geom::centroid(&l.poly).dist(want);
            if best.is_none_or(|(bd, _)| d < bd) {
                best = Some((d, i as u32));
            }
        }
        let Some((_, lid)) = best else { break };
        lots[lid as usize].occupant = Some(files.len());
        let poly = building_poly(&lots[lid as usize].poly, m, params);
        buildings.push(Building {
            poly,
            path: m.path.clone(),
            lot: lid,
            district: di,
            height: footprint_height(m),
            monument: polis_repo::tree::entry_point(&m.path).is_some(),
        });
        files.push(m.path.clone());
    }
    (lots, buildings, files, lots_of_district, gardens)
}

/// The largest valid inset of a block.
///
/// `inset_edges` is exact for convex blocks but degenerates to the convex
/// kernel on the non-convex ones the dissolve pass produces — which silently
/// threw away the biggest, most interesting blocks. The miter offset handles
/// those, so take whichever is larger and still inside.
fn best_inset(poly: &[Pt], d: &[f64]) -> Option<Vec<Pt>> {
    let a = geom::inset_edges(poly, d).filter(|q| q.len() >= 3);
    let dmax = d.iter().copied().fold(0.0f64, f64::max);
    // The miter candidate has to clear every edge by that edge's own setback,
    // not merely stay inside the block, or buildings end up under the asphalt.
    let clears = |q: &Vec<Pt>| -> bool {
        q.iter().all(|&v| geom::point_in_poly(poly, v))
            && q.iter().all(|&v| {
                (0..poly.len()).all(|i| {
                    let a = poly[i];
                    let b = poly[(i + 1) % poly.len()];
                    geom::seg_dist2(v, a, b) >= (d[i] * 0.92) * (d[i] * 0.92)
                })
            })
    };
    let b = geom::inset(poly, dmax).filter(|q| q.len() >= 3 && clears(q));
    match (a, b) {
        (Some(x), Some(y)) => {
            if geom::area(&y) > geom::area(&x) {
                Some(y)
            } else {
                Some(x)
            }
        }
        (Some(x), None) => Some(x),
        (None, y) => y,
    }
}

/// Lots that front the street: a perimeter ring of trapezoids around a
/// courtyard, which is what a real city block looks like from above and what
/// stops a triangular block from producing a fan of triangular buildings.
///
/// Returns `None` when the block is too small or too spiky to hold a ring, and
/// the caller falls back to recursive splitting.
fn perimeter_lots(poly: &[Pt], k: usize, depth: f64) -> Option<(Vec<Vec<Pt>>, Vec<Pt>)> {
    if k == 0 || poly.len() < 3 {
        return None;
    }
    let a = geom::area(poly);
    let per = geom::perimeter(poly).max(1e-6);
    let mean_hw = a / per;
    // Depth is chosen so a lot is about as deep as it is wide, capped so the
    // ring cannot swallow the block. This is what makes the fabric read as
    // built-up rather than as a hedge around a field.
    // Aim deep and let the ladder below find the deepest ring the block will
    // actually take; anything shallower leaves a courtyard the size of a field.
    let d0 = (per / k as f64 * 0.95)
        .min(mean_hw * 1.05)
        .min(depth)
        .max(1e-3);
    // Adaptive: a spiky block will not take a deep ring, but it will take a
    // shallow one.
    let mut inner = None;
    for f in [1.0, 0.92, 0.84, 0.76, 0.68, 0.60, 0.52, 0.44, 0.36, 0.28, 0.20] {
        if let Some(q) = geom::miter_inset_paired(poly, d0 * f) {
            if q.len() == poly.len() && geom::area(&q) > a * 0.02 {
                inner = Some(q);
                break;
            }
        }
    }
    let inner = inner?;
    let n = poly.len();
    let lens: Vec<f64> = (0..n).map(|i| poly[i].dist(poly[(i + 1) % n])).collect();
    let total: f64 = lens.iter().sum();
    if total <= 0.0 {
        return None;
    }
    // Largest-remainder apportionment of k lots over the edges by frontage.
    let mut alloc: Vec<usize> = Vec::with_capacity(n);
    let mut rem: Vec<(f64, usize)> = Vec::new();
    let mut used = 0usize;
    for (i, l) in lens.iter().enumerate() {
        let exact = k as f64 * l / total;
        let f = exact.floor();
        alloc.push(f as usize);
        used += f as usize;
        rem.push((exact - f, i));
    }
    rem.sort_by(|x, y| {
        y.0.partial_cmp(&x.0)
            .unwrap_or(Ordering::Equal)
            .then(x.1.cmp(&y.1))
    });
    let mut ri = 0usize;
    while used < k && !rem.is_empty() {
        alloc[rem[ri % rem.len()].1] += 1;
        used += 1;
        ri += 1;
    }
    let mut out = Vec::with_capacity(k);
    for i in 0..n {
        let m = alloc[i];
        if m == 0 {
            continue;
        }
        let a0 = poly[i];
        let b0 = poly[(i + 1) % n];
        let a1 = inner[i];
        let b1 = inner[(i + 1) % n];
        for j in 0..m {
            let t0 = j as f64 / m as f64;
            let t1 = (j + 1) as f64 / m as f64;
            let q = vec![
                a0.lerp(b0, t0),
                a0.lerp(b0, t1),
                a1.lerp(b1, t1),
                a1.lerp(b1, t0),
            ];
            if geom::area(&q) > 1e-6 {
                out.push(q);
            }
        }
    }
    if out.is_empty() {
        return None;
    }
    Some((out, inner))
}

/// Recursive area-ratio subdivision into exactly `k` parts.
fn split_into(poly: &[Pt], k: usize, rng: &mut SeededRng, oldness: f64) -> Vec<Vec<Pt>> {
    if k <= 1 || poly.len() < 3 {
        return vec![poly.to_vec()];
    }
    let kl = k / 2;
    let mut normal = geom::longest_axis(poly);
    let jitter = 0.03 + 0.22 * oldness;
    normal = geom::rotate(normal, (rng.next_f64() * 2.0 - 1.0) * jitter).norm();
    let base = kl as f64 / k as f64;
    let ratio = (base + (rng.next_f64() - 0.5) * 0.10 * base).clamp(0.15, 0.85);
    let off = crate::arr::offset_for_area_ratio(poly, normal, ratio);
    let left = geom::clip_halfplane(poly, normal, off);
    let right = geom::clip_halfplane(poly, normal.mul(-1.0), -off);
    if left.len() < 3 || right.len() < 3 {
        return vec![poly.to_vec()];
    }
    let mut out = split_into(&left, kl, rng, oldness);
    out.extend(split_into(&right, k - kl, rng, oldness));
    out
}

fn footprint_height(m: &FileMeta) -> f64 {
    let s = (m.size_bytes.max(1) as f64).sqrt();
    (s / 9.0).clamp(1.0, 46.0)
}

fn building_poly(lot: &[Pt], m: &FileMeta, params: &Params) -> Vec<Pt> {
    let Some(base) = geom::inset(lot, params.setback) else {
        return Vec::new();
    };
    if base.len() < 3 {
        return Vec::new();
    }
    let lot_area = geom::area(&base);
    // Footprint is proportional to sqrt(bytes) (PRD 7.3), clamped into the lot
    // so a dense district still reads as built-up rather than as scattered dots.
    let want = ((m.size_bytes.max(1) as f64).sqrt() * 0.80)
        .clamp(lot_area * 0.16, lot_area * 0.76);
    let mut k = (want / lot_area.max(1e-6)).sqrt().clamp(0.30, 1.0);
    let c = geom::centroid(&base);
    let mut rng = SeededRng::for_path(&m.path, "footprint");
    let rot = (rng.next_f64() * 2.0 - 1.0) * 0.0698; // +/- 4 degrees
    for _ in 0..5 {
        let cand: Vec<Pt> = base
            .iter()
            .map(|p| {
                let v = p.sub(c).mul(k);
                c.add(geom::rotate(v, rot))
            })
            .collect();
        if cand.iter().all(|&p| geom::point_in_poly(lot, p)) {
            return cand;
        }
        k *= 0.90;
    }
    // Last resort: the convex kernel of the lot is inside the lot by
    // construction, so a building placed in it can never leave it.
    if let Some(kern) = geom::kernel(lot, params.setback) {
        if kern.len() >= 3 && kern.iter().all(|&p| geom::point_in_poly(lot, p)) {
            return kern;
        }
    }
    let lc = geom::centroid(lot);
    if geom::point_in_poly(lot, lc) {
        for k in [0.55, 0.40, 0.28, 0.18] {
            let cand: Vec<Pt> = lot.iter().map(|p| lc.add(p.sub(lc).mul(k))).collect();
            if cand.iter().all(|&p| geom::point_in_poly(lot, p)) {
                return cand;
            }
        }
    }
    Vec::new()
}

// ---------------------------------------------------------------------------
// Streets: cross-district imports, routed along the road network
// ---------------------------------------------------------------------------

pub fn route_streets(layout: &mut Layout, pairs: &[(u32, u32, u32)]) {
    if layout.nodes.is_empty() {
        return;
    }
    let mut adj: Vec<Vec<(u32, f64)>> = vec![Vec::new(); layout.nodes.len()];
    for &(a, b, _) in &layout.segs {
        let w = layout.nodes[a as usize].dist(layout.nodes[b as usize]);
        adj[a as usize].push((b, w));
        adj[b as usize].push((a, w));
    }
    // Anchor each district at the road node nearest its centroid.
    let mut anchor: BTreeMap<u32, u32> = BTreeMap::new();
    let mut cents: Vec<Pt> = Vec::new();
    for d in &layout.districts {
        cents.push(geom::centroid(&d.poly));
    }
    // Only nodes on that district's boundary blocks are eligible; approximate by
    // nearest node overall, which is on the district's own fabric.
    for (di, c) in cents.iter().enumerate() {
        let mut best = 0u32;
        let mut bd = f64::MAX;
        for (i, p) in layout.nodes.iter().enumerate() {
            let d = p.dist(*c);
            if d < bd {
                bd = d;
                best = i as u32;
            }
        }
        anchor.insert(di as u32, best);
    }
    let mut streets = Vec::new();
    for &(a, b, w) in pairs {
        let (Some(&sa), Some(&sb)) = (anchor.get(&a), anchor.get(&b)) else {
            continue;
        };
        if sa == sb {
            continue;
        }
        if let Some(path) = dijkstra(&adj, sa, sb) {
            let poly: Vec<Pt> = path.iter().map(|&n| layout.nodes[n as usize]).collect();
            if poly.len() >= 2 {
                streets.push(Street { poly, weight: w });
            }
        }
    }
    layout.streets = streets;
}

fn dijkstra(adj: &[Vec<(u32, f64)>], src: u32, dst: u32) -> Option<Vec<u32>> {
    let n = adj.len();
    let mut dist = vec![f64::MAX; n];
    let mut prev = vec![u32::MAX; n];
    let mut heap: BinaryHeap<(std::cmp::Reverse<i64>, std::cmp::Reverse<u32>)> = BinaryHeap::new();
    dist[src as usize] = 0.0;
    heap.push((std::cmp::Reverse(0), std::cmp::Reverse(src)));
    let mut seen = vec![false; n];
    while let Some((std::cmp::Reverse(_), std::cmp::Reverse(u))) = heap.pop() {
        if seen[u as usize] {
            continue;
        }
        seen[u as usize] = true;
        if u == dst {
            break;
        }
        for &(v, w) in &adj[u as usize] {
            let nd = dist[u as usize] + w;
            if nd < dist[v as usize] {
                dist[v as usize] = nd;
                prev[v as usize] = u;
                heap.push((std::cmp::Reverse((nd * 64.0) as i64), std::cmp::Reverse(v)));
            }
        }
    }
    if dist[dst as usize] == f64::MAX {
        return None;
    }
    let mut path = vec![dst];
    let mut cur = dst;
    while cur != src {
        cur = prev[cur as usize];
        if cur == u32::MAX {
            return None;
        }
        path.push(cur);
    }
    path.reverse();
    Some(path)
}

// ---------------------------------------------------------------------------
// Incremental growth
// ---------------------------------------------------------------------------

pub enum AddOutcome {
    /// The quantised weight did not move: the file took a vacant lot and not one
    /// road or block changed.
    FilledVacantLot { lot: u32 },
    /// The bucket moved: this district's subtree must be re-subdivided. Its
    /// outer polygon is fixed by its parent, so nothing outside it moves.
    NeedsResubdivide { district: u32 },
}

pub fn add_file(layout: &mut Layout, meta: &FileMeta, params: &Params) -> AddOutcome {
    let dir = meta.path.parent().unwrap_or_else(LogicalPath::root);
    let di = layout
        .districts
        .iter()
        .position(|d| d.path == dir)
        .map(|i| i as u32);
    let Some(di) = di else {
        return AddOutcome::NeedsResubdivide { district: 0 };
    };
    let d = &layout.districts[di as usize];
    let after = lots_for(d.direct_files + 1);
    if after != d.lots {
        return AddOutcome::NeedsResubdivide { district: di };
    }
    let empty = Vec::new();
    let ids = layout.lots_of_district.get(&di).unwrap_or(&empty);
    let free = ids
        .iter()
        .copied()
        .find(|&l| layout.lots[l as usize].occupant.is_none());
    let Some(lot) = free else {
        return AddOutcome::NeedsResubdivide { district: di };
    };
    let poly = building_poly(&layout.lots[lot as usize].poly, meta, params);
    layout.lots[lot as usize].occupant = Some(layout.files.len());
    layout.buildings.push(Building {
        poly,
        path: meta.path.clone(),
        lot,
        district: di,
        height: footprint_height(meta),
        monument: false,
    });
    layout.files.push(meta.path.clone());
    layout.districts[di as usize].direct_files += 1;
    AddOutcome::FilledVacantLot { lot }
}

/// The worst-case incremental step: the district's quantised weight moved, so
/// its interior is re-subdivided and re-parcelled from scratch.
///
/// Its outer polygon is fixed by its parent in the subdivision, so nothing
/// outside it can move — this is the whole incremental argument, and it is what
/// this function measures.
pub fn resubdivide_district(layout: &Layout, di: u32, params: &Params) -> usize {
    let d = &layout.districts[di as usize];
    if d.poly.len() < 3 {
        return 0;
    }
    let mut a = Arr::from_ring(d.poly.clone());
    a.faces[0].district = 0;
    let want = layout
        .blocks
        .iter()
        .filter(|b| b.district == di)
        .count()
        .max(2);
    let field = Field {
        seed: params.seed,
        old_grid: vec![d.oldness as f32; 4],
        gw: 2,
        lo: pt(-1e6, -1e6),
        cell: 1e7,
        p: params.clone(),
        r: params.area_per_file.sqrt(),
        city_r: 1.0,
    };
    subdivide_blocks(&mut a, 0, want, d, params, &field);
    a.subdivide_edges(params.seg_len, 6);
    let mut blocks = Vec::new();
    for f in a.live_faces() {
        let poly = a.poly(f);
        if poly.len() >= 3 && geom::area(&poly) > 1.0 {
            let n = poly.len();
            blocks.push(Block {
                poly,
                district: 0,
                edge_inset: vec![params.road_half_width[3] + params.setback; n],
            });
        }
    }
    let one = vec![d.clone()];
    let mut sub = RepoTree::default();
    for (p, m) in &layout.district_of_file {
        if *m == di {
            sub.files.insert(
                p.clone(),
                FileMeta::untracked(p.clone(), 4096),
            );
        }
    }
    let mut dof = BTreeMap::new();
    for p in sub.files.keys() {
        dof.insert(p.clone(), 0u32);
    }
    let (lots, _, _, _, _) = parcel(&blocks, &one, &sub, &dof, params);
    lots.len()
}

/// A stable textual dump used for the byte-identity proof.
pub fn digest(layout: &Layout) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(1 << 20);
    let q = |v: f64| -> i64 { (v * 4096.0).round() as i64 };
    let _ = writeln!(
        s,
        "nodes {} segs {} blocks {} lots {} buildings {} districts {} streets {}",
        layout.nodes.len(),
        layout.segs.len(),
        layout.blocks.len(),
        layout.lots.len(),
        layout.buildings.len(),
        layout.districts.len(),
        layout.streets.len()
    );
    for n in &layout.nodes {
        let _ = writeln!(s, "N {} {}", q(n.x), q(n.y));
    }
    for (a, b, c) in &layout.segs {
        let _ = writeln!(s, "S {a} {b} {}", *c as u8);
    }
    for b in &layout.blocks {
        let _ = write!(s, "B {}", b.district);
        for p in &b.poly {
            let _ = write!(s, " {},{}", q(p.x), q(p.y));
        }
        let _ = writeln!(s);
    }
    for l in &layout.lots {
        let _ = write!(s, "L {} {:?}", l.block, l.occupant);
        for p in &l.poly {
            let _ = write!(s, " {},{}", q(p.x), q(p.y));
        }
        let _ = writeln!(s);
    }
    for b in &layout.buildings {
        let _ = write!(s, "U {} {} {}", b.path.as_str(), b.district, q(b.height));
        for p in &b.poly {
            let _ = write!(s, " {},{}", q(p.x), q(p.y));
        }
        let _ = writeln!(s);
    }
    for d in &layout.districts {
        let _ = write!(
            s,
            "D {} {} {} {}",
            d.path.as_str(),
            d.files_under,
            d.direct_files,
            q(d.oldness)
        );
        for p in &d.poly {
            let _ = write!(s, " {},{}", q(p.x), q(p.y));
        }
        let _ = writeln!(s);
    }
    for st in &layout.streets {
        let _ = write!(s, "T {}", st.weight);
        for p in &st.poly {
            let _ = write!(s, " {},{}", q(p.x), q(p.y));
        }
        let _ = writeln!(s);
    }
    s
}

/// Cross-district import pairs, aggregated (PRD §9). Deterministic order.
pub fn street_pairs(
    layout: &Layout,
    edges: &[(LogicalPath, LogicalPath)],
    cap: usize,
) -> Vec<(u32, u32, u32)> {
    let mut index: BTreeMap<LogicalPath, u32> = BTreeMap::new();
    for (i, d) in layout.districts.iter().enumerate() {
        index.insert(d.path.clone(), i as u32);
    }
    let mut counts: BTreeMap<(u32, u32), u32> = BTreeMap::new();
    for (from, to) in edges {
        let fd = from.parent().unwrap_or_else(LogicalPath::root);
        let td = to.parent().unwrap_or_else(LogicalPath::root);
        if fd == td {
            continue;
        }
        let (Some(&a), Some(&b)) = (index.get(&fd), index.get(&td)) else {
            continue;
        };
        if a == b {
            continue;
        }
        let k = if a < b { (a, b) } else { (b, a) };
        *counts.entry(k).or_insert(0) += 1;
    }
    let mut v: Vec<(u32, u32, u32)> = counts.into_iter().map(|((a, b), c)| (a, b, c)).collect();
    v.sort_by(|x, y| y.2.cmp(&x.2).then((x.0, x.1).cmp(&(y.0, y.1))));
    v.truncate(cap);
    v.sort_by_key(|&(a, b, _)| (a, b));
    v
}

