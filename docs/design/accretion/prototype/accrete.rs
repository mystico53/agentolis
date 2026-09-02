//! Stage 0 — the growth simulation (PRD §7.1).
//!
//! The settlement accretes one **plot** at a time, in git commit order. A plot
//! is a parcel of ground with a capacity; files land in the plot of their own
//! directory nearest that directory's centre of mass, and a new plot is settled
//! on the district's frontier only when every existing one is full.
//!
//! Nothing here places a road or a building. The output is a labelled point set
//! whose *dual* is the road network (see `voronoi`), which is why the pipeline
//! order in PRD §7.2 is preserved: roads still come before blocks, blocks before
//! lots, lots before buildings.

use std::collections::BTreeMap;

use polis_events::LogicalPath;
use polis_layout::determinism::{
    combine_seeds, det_sin_cos, fbm2_f64, fbm2_with_gradient, quantize_f64, SeededRng, TAU,
};

use crate::geom::*;

/// One file as the growth simulation sees it.
#[derive(Debug, Clone)]
pub struct FileRec {
    pub path: LogicalPath,
    pub size_bytes: u64,
    /// Index in the git growth sequence, `u32::MAX` for untracked.
    pub growth_index: u32,
    pub added_at: i64,
    pub last_touched: i64,
    pub industrial: bool,
    pub monument: bool,
}

/// A settled parcel of ground: the generator of one Voronoi cell.
#[derive(Debug, Clone)]
pub struct Plot {
    pub pos: P,
    pub district: u32,
    /// Files living here, in growth order.
    pub files: Vec<u32>,
    /// How many files this plot can hold before the district must settle
    /// another. Larger on the planned periphery, smaller in the old town.
    pub cap: u32,
    /// Growth step at which this plot was settled — the plot's own age.
    pub birth: u32,
}

#[derive(Debug, Clone)]
pub struct DistrictState {
    pub path: LogicalPath,
    pub plots: Vec<u32>,
    pub sum: P,
    pub parent: Option<u32>,
    pub industrial: bool,
    /// Max distance of a plot from the running centroid.
    pub radius: f64,
}

impl DistrictState {
    pub fn centroid(&self) -> P {
        if self.plots.is_empty() {
            [0.0, 0.0]
        } else {
            mul(self.sum, 1.0 / self.plots.len() as f64)
        }
    }
}

pub struct Params {
    /// Minimum plot separation in the historic core.
    pub sep_core: f64,
    /// Minimum plot separation on the recent periphery.
    pub sep_rim: f64,
    /// Plot capacity in the core (fine mesh, small blocks).
    pub cap_core: u32,
    /// Plot capacity on the rim (coarse mesh, big planned blocks).
    pub cap_rim: u32,
    pub terrain_seed: u64,
    pub terrain_freq: f64,
    /// Weight on staying near the district's own centre of mass.
    pub w_compact: f64,
    /// Weight on hugging the existing settlement — this is what keeps the city
    /// one connected thing instead of an archipelago.
    pub w_hug: f64,
    /// Weight on avoiding steep ground.
    pub w_slope: f64,
    /// Weight on not interleaving with a neighbouring district.
    pub w_foreign: f64,
    /// Weight on the path-seeded irregularity.
    pub w_noise: f64,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            sep_core: 1.00,
            sep_rim: 1.85,
            cap_core: 3,
            cap_rim: 7,
            terrain_seed: 0x504f_4c49_5300_0001,
            terrain_freq: 0.055,
            w_compact: crate::env_f("W_COMPACT", 2.20),
            w_hug: crate::env_f("W_HUG", 2.20),
            w_slope: crate::env_f("W_SLOPE", 2.60),
            w_foreign: crate::env_f("W_FOREIGN", 3.40),
            w_noise: crate::env_f("W_NOISE", 0.85),
        }
    }
}

/// Uniform spatial hash over plot positions. Iterated only in a fixed cell
/// order and every result is sorted, so no iteration order reaches the output.
struct Grid {
    cell: f64,
    buckets: BTreeMap<(i32, i32), Vec<u32>>,
}

impl Grid {
    fn new(cell: f64) -> Self {
        Self {
            cell,
            buckets: BTreeMap::new(),
        }
    }
    #[inline]
    fn key(&self, p: P) -> (i32, i32) {
        (
            (p[0] / self.cell).floor() as i32,
            (p[1] / self.cell).floor() as i32,
        )
    }
    fn insert(&mut self, p: P, id: u32) {
        self.buckets.entry(self.key(p)).or_default().push(id);
    }
    /// Every plot id within `r` of `p`, in ascending id order.
    fn query(&self, p: P, r: f64, out: &mut Vec<u32>) {
        out.clear();
        let n = (r / self.cell).ceil() as i32;
        let (kx, ky) = self.key(p);
        for dy in -n..=n {
            for dx in -n..=n {
                if let Some(b) = self.buckets.get(&(kx + dx, ky + dy)) {
                    out.extend_from_slice(b);
                }
            }
        }
        out.sort_unstable();
    }
}

pub struct Settlement {
    pub plots: Vec<Plot>,
    pub districts: Vec<DistrictState>,
    pub district_index: BTreeMap<LogicalPath, u32>,
    pub files: Vec<FileRec>,
    /// `files[i]` lives in `plots[file_plot[i]]`.
    pub file_plot: Vec<u32>,
    pub params: Params,
    grid: Grid,
    step: u32,
    total: u32,
    scratch: Vec<u32>,
}

impl Settlement {
    pub fn new(params: Params, total_hint: u32) -> Self {
        let cell = params.sep_rim * 1.25;
        Self {
            plots: Vec::new(),
            districts: Vec::new(),
            district_index: BTreeMap::new(),
            files: Vec::new(),
            file_plot: Vec::new(),
            grid: Grid::new(cell),
            step: 0,
            total: total_hint.max(1),
            scratch: Vec::new(),
            params,
        }
    }

    /// Growth progress in `[0, 1]`: 0 is the first commit, 1 is HEAD.
    fn progress(&self) -> f64 {
        (f64::from(self.step) / f64::from(self.total)).clamp(0.0, 1.0)
    }

    fn sep_now(&self) -> f64 {
        let t = self.progress().powf(0.62);
        self.params.sep_core + (self.params.sep_rim - self.params.sep_core) * t
    }

    fn cap_now(&self) -> u32 {
        let t = self.progress().powf(0.62);
        let c = f64::from(self.params.cap_core)
            + (f64::from(self.params.cap_rim) - f64::from(self.params.cap_core)) * t;
        c.round().max(1.0) as u32
    }

    fn terrain(&self, p: P) -> f64 {
        fbm2_f64(
            self.params.terrain_seed,
            p[0] * self.params.terrain_freq,
            p[1] * self.params.terrain_freq,
            3,
        )
    }

    fn slope(&self, p: P) -> f64 {
        let (_, gx, gy) = fbm2_with_gradient(
            self.params.terrain_seed,
            p[0] * self.params.terrain_freq,
            p[1] * self.params.terrain_freq,
            3,
        );
        (gx * gx + gy * gy).sqrt() * self.params.terrain_freq * 40.0
    }

    /// Ensure a district exists, creating it (and its ancestors) on first use.
    fn ensure_district(&mut self, path: &LogicalPath, industrial: bool) -> u32 {
        if let Some(&id) = self.district_index.get(path) {
            return id;
        }
        let parent = path.parent().map(|p| self.ensure_district(&p, false));
        let id = u32::try_from(self.districts.len()).expect("district count fits u32");
        self.districts.push(DistrictState {
            path: path.clone(),
            plots: Vec::new(),
            sum: [0.0, 0.0],
            parent,
            industrial,
            radius: 0.0,
        });
        self.district_index.insert(path.clone(), id);
        id
    }

    /// Where a brand-new district should look for its first plot: on the
    /// frontier of its parent's territory, in the direction least crowded by
    /// districts that already exist. A quarter buds off the town that made it.
    fn seed_ideal(&mut self, did: u32) -> P {
        let d = &self.districts[did as usize];
        let sep = self.sep_now();
        let Some(pid) = d.parent else {
            return [0.0, 0.0];
        };
        let parent = &self.districts[pid as usize];
        let (anchor, r0) = if parent.plots.is_empty() {
            // The parent has no ground of its own yet either — fall back to the
            // nearest ancestor that does, else the civic square.
            let mut a = [0.0, 0.0];
            let mut rr = sep;
            let mut cur = parent.parent;
            while let Some(c) = cur {
                let dc = &self.districts[c as usize];
                if !dc.plots.is_empty() {
                    a = dc.centroid();
                    rr = dc.radius + sep * 1.6;
                    break;
                }
                cur = dc.parent;
            }
            (a, rr)
        } else {
            (parent.centroid(), parent.radius + sep * 1.15)
        };
        let mut rng = SeededRng::for_path(&d.path, "district.seed.angle");
        let base = rng.next_f64() * TAU;
        // Centroids of every district that already has ground — the thing the
        // new quarter must not sit on top of.
        let occupied: Vec<P> = self
            .districts
            .iter()
            .filter(|x| !x.plots.is_empty())
            .map(DistrictState::centroid)
            .collect();
        let mut best = anchor;
        let mut best_score = f64::NEG_INFINITY;
        for ri in 0..3 {
            let r = r0 + sep * 0.8 * f64::from(ri);
            let n = 48;
            for k in 0..n {
                let ang = base + TAU * f64::from(k) / f64::from(n);
                let (sn, cs) = det_sin_cos(ang);
                let c = [anchor[0] + cs * r, anchor[1] + sn * r];
                let mut clear = f64::INFINITY;
                for o in &occupied {
                    clear = clear.min(dist(c, *o));
                }
                if !clear.is_finite() {
                    clear = 0.0;
                }
                let score = clear.min(sep * 6.0) * 1.0 - dist(c, anchor) * 0.55
                    - self.slope(c) * 1.4
                    + self.terrain(c) * 0.5;
                if score > best_score
                    || (score == best_score && (quantize_f64(c[0]), quantize_f64(c[1])) < (quantize_f64(best[0]), quantize_f64(best[1])))
                {
                    best_score = score;
                    best = c;
                }
            }
        }
        best
    }

    /// Search for a free position for a new plot of `did`, around a set of
    /// anchors.
    ///
    /// A candidate must **touch the existing settlement** — no closer than
    /// `sep` to a settled plot, and no farther than `contact` from the nearest
    /// one. That single constraint is what makes the city one connected thing
    /// rather than an archipelago: no quarter can ever be founded across a gap.
    /// Only if no touching position exists anywhere is the constraint relaxed.
    fn find_free(&mut self, did: u32, anchors: &[(P, f64)], seed: u64) -> Option<P> {
        for relax in 0..2 {
            let contact = if relax == 0 {
                self.sep_now() * 1.75
            } else {
                f64::INFINITY
            };
            if let Some(p) = self.search(did, anchors, seed, contact) {
                return Some(p);
            }
        }
        None
    }

    fn search(&mut self, did: u32, anchors: &[(P, f64)], seed: u64, contact: f64) -> Option<P> {
        let sep = self.sep_now();
        let mut rng = SeededRng::for_seed(seed, "plot.position");
        let base = rng.next_f64() * TAU;
        let dcent = self.districts[did as usize].centroid();
        let has_ground = !self.districts[did as usize].plots.is_empty();
        let empty_world = self.plots.is_empty();

        let mut best: Option<(f64, P)> = None;
        let mut found = 0u32;
        for step in 0..48usize {
            let rf = if step == 0 {
                0.0
            } else {
                0.42 * f64::from(step as u32).powf(1.18)
            };
            for (ai, (anchor, abias)) in anchors.iter().enumerate() {
                let r = rf * sep + abias;
                let count = if r < 1e-9 {
                    1
                } else {
                    (TAU * r / (sep * 0.40)).ceil().clamp(12.0, 72.0) as u32
                };
                for k in 0..count {
                    let ang = base
                        + TAU * f64::from(k) / f64::from(count)
                        + f64::from(ai as u32) * 0.271_828;
                    let (sn, cs) = det_sin_cos(ang);
                    let c = qp([anchor[0] + cs * r, anchor[1] + sn * r]);
                    self.grid.query(c, sep * 2.6, &mut self.scratch);
                    let mut nearest = f64::INFINITY;
                    let mut nearest_same = f64::INFINITY;
                    let mut foreign = 0.0f64;
                    let mut samey = 0.0f64;
                    let mut reject = false;
                    for &pi in &self.scratch {
                        let pl = &self.plots[pi as usize];
                        let dd = dist(c, pl.pos);
                        if dd < sep - 1e-6 {
                            reject = true;
                            break;
                        }
                        if dd < sep * 2.6 {
                            if pl.district == did {
                                samey += 1.0;
                                nearest_same = nearest_same.min(dd);
                            } else {
                                foreign += 1.0;
                            }
                        }
                        nearest = nearest.min(dd);
                    }
                    if reject {
                        continue;
                    }
                    if !empty_world && nearest > contact {
                        continue; // would found an island
                    }
                    if !nearest.is_finite() {
                        nearest = 0.0; // first plot in the world
                    }
                    let p = &self.params;
                    let mut score = -p.w_hug * (nearest / sep)
                        - p.w_slope * self.slope(c)
                        + p.w_noise * self.terrain(c);
                    if has_ground {
                        score -= p.w_compact * (dist(c, dcent) / sep) * 0.55;
                        let tot = foreign + samey;
                        if tot > 0.0 {
                            score -= p.w_foreign * (foreign / tot);
                        }
                        // Adjacency to the quarter's own ground: a district whose
                        // parcels touch each other is one contiguous district on
                        // the map, with a boundary you can trace.
                        if nearest_same.is_finite() {
                            score -= crate::env_f("W_ADJ", 2.00) * (nearest_same / sep);
                            if nearest_same <= nearest + 1e-9 {
                                score += crate::env_f("W_TOUCH", 1.60);
                            }
                        } else {
                            score -= crate::env_f("W_ADJ", 2.00) * 2.5;
                        }
                    } else {
                        // A brand-new quarter wants to sit next to the ground
                        // its parent already holds, but not inside it.
                        score -= 0.30 * (dist(c, anchors[0].0) / sep);
                    }
                    found += 1;
                    let better = match best {
                        None => true,
                        Some((bs, bp)) => {
                            score > bs
                                || (score == bs
                                    && (quantize_f64(c[0]), quantize_f64(c[1]))
                                        < (quantize_f64(bp[0]), quantize_f64(bp[1])))
                        }
                    };
                    if better {
                        best = Some((score, c));
                    }
                }
            }
            // Enough room found near the frontier — stop widening the search.
            if found >= 16 && step >= 2 {
                break;
            }
        }
        best.map(|(_, p)| p)
    }

    fn settle_plot(&mut self, did: u32, pos: P, cap: u32) -> u32 {
        let id = u32::try_from(self.plots.len()).expect("plot count fits u32");
        self.plots.push(Plot {
            pos,
            district: did,
            files: Vec::new(),
            cap,
            birth: self.step,
        });
        self.grid.insert(pos, id);
        let d = &mut self.districts[did as usize];
        d.plots.push(id);
        d.sum = add(d.sum, pos);
        let c = mul(d.sum, 1.0 / d.plots.len() as f64);
        // Recompute the radius against the moved centroid.
        let mut r: f64 = 0.0;
        for &pi in &d.plots {
            r = r.max(dist(self.plots[pi as usize].pos, c));
        }
        self.districts[did as usize].radius = r;
        id
    }

    /// Anchors for the frontier search.
    ///
    /// Deliberately **not** biased toward the district's outermost parcels. An
    /// outward bias is a positive feedback loop — each new plot extends the tip
    /// that the next one then grows from — and it produces long spindly arms
    /// instead of quarters. The search sweeps outward from the centre of mass
    /// (with a second ring pre-advanced to the frontier so a full district does
    /// not re-scan its own interior every step), plus a few uniformly sampled
    /// parcels so the quarter can still bulge where there is room.
    fn frontier_anchors(&self, did: u32, seed: u64) -> Vec<(P, f64)> {
        let d = &self.districts[did as usize];
        let c = d.centroid();
        let sep = self.sep_now();
        let mut out = vec![(c, 0.0)];
        if d.plots.is_empty() {
            return out;
        }
        if d.radius > sep * 1.4 {
            out.push((c, d.radius - sep * 1.1));
        }
        let n = d.plots.len();
        let mut rng = SeededRng::for_seed(seed, "frontier.pick");
        for _ in 0..3.min(n) {
            let idx = (rng.next_f64() * n as f64) as usize % n;
            out.push((self.plots[d.plots[idx] as usize].pos, 0.0));
        }
        out
    }

    /// One growth step: place one file. This is the whole incremental story —
    /// `add_file` is what runs when a file appears in a new commit.
    pub fn add_file(&mut self, rec: FileRec) -> u32 {
        let fid = u32::try_from(self.files.len()).expect("file count fits u32");
        let dpath = rec.path.parent().unwrap_or_else(LogicalPath::root);
        let did = self.ensure_district(&dpath, rec.industrial);

        // 1. An existing plot of this district with room, nearest the
        //    district's centre of mass. Ties broken by plot id.
        let cent = self.districts[did as usize].centroid();
        let mut chosen: Option<(f64, u32)> = None;
        for &pi in &self.districts[did as usize].plots {
            let pl = &self.plots[pi as usize];
            if (pl.files.len() as u32) < pl.cap {
                let dd = quantize_f64(dist(pl.pos, cent));
                let better = match chosen {
                    None => true,
                    Some((bd, bi)) => dd < bd || (dd == bd && pi < bi),
                };
                if better {
                    chosen = Some((dd, pi));
                }
            }
        }

        let plot = match chosen {
            Some((_, pi)) => pi,
            None => {
                // 2. Settle new ground on this district's frontier.
                let seed = combine_seeds(rec.path.layout_seed(), u64::from(self.step));
                let anchors = if self.districts[did as usize].plots.is_empty() {
                    let ideal = self.seed_ideal(did);
                    vec![(ideal, 0.0)]
                } else {
                    self.frontier_anchors(did, seed)
                };
                let cap = self.cap_now();
                let pos = self
                    .find_free(did, &anchors, seed)
                    .unwrap_or_else(|| anchors[0].0);
                self.settle_plot(did, pos, cap)
            }
        };

        self.plots[plot as usize].files.push(fid);
        self.files.push(rec);
        self.file_plot.push(plot);
        self.step += 1;
        fid
    }

    pub fn plot_positions(&self) -> Vec<P> {
        self.plots.iter().map(|p| p.pos).collect()
    }

    pub fn extent(&self) -> (P, P) {
        let pts: Vec<P> = self.plot_positions();
        if pts.is_empty() {
            return ([-1.0, -1.0], [1.0, 1.0]);
        }
        bounds(&pts)
    }
}
