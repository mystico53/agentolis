//! Stage 0 — the growth simulation (PRD §7.1).
//!
//! > **`git log` is the growth order.** Replay it in commit order. Files added
//! > in the repo's first year form the old town — dense, tangled, irregular.
//!
//! The settlement accretes one **plot** at a time. A plot is a parcel of ground
//! with a capacity; a file moves into a plot of its own directory when one has
//! room, and only when every one is full does the district settle new ground on
//! its frontier. Nothing here places a road or a building: the output is a
//! labelled point set whose *dual* is the road network (see [`crate::voronoi`]),
//! so PRD §7.2's non-negotiable ordering — roads, then blocks, then lots, then
//! buildings — is preserved rather than worked around.
//!
//! # The two hard constraints
//!
//! Everything else is a weight; these two are absolute, and they are rejects
//! inside the candidate loop rather than penalties on the score, so no amount of
//! weight can outvote them.
//!
//! 1. **Never closer than `sep` to a settled plot.** This is what bounds cell
//!    size, and therefore block size.
//! 2. **Never further than [`Params::touch_max`] separations from the nearest
//!    settled plot.** A new quarter cannot be founded across a gap. This single
//!    rule is why `components = 1`, and it is also why the phantom ring in
//!    [`crate::voronoi`] has no interior pocket to grow a hole in.
//!
//! # What is *not* a constraint any more, and why
//!
//! Until this commit there was a third: a plot could settle only inside its own
//! district's polygon, cut out of a convex city limit by
//! [`crate::territory`]. It was there to make districts contiguous and it did —
//! at the cost of the two things three M1 gates rejected: a silhouette that was
//! the polygon (solidity 0.9975, convex is 1.00) and district borders that were
//! exactly straight lines from the middle of the city to its edge.
//!
//! Contiguity is now obtained where it belongs, on the **graph** rather than on
//! the plane: [`crate::regions`] partitions the plot adjacency graph down the
//! same directory tree, and every part of that partition is connected by
//! construction. The growth is free again, so the town's outline is the outline
//! of the ground people settled.
//!
//! The district *shape* is still steered here, by weights — hug the settlement,
//! stay near the district's centre of mass, do not interleave with a neighbour,
//! prefer contact with the district's own ground — because a partition that
//! starts from blobby input moves far fewer plots than one that starts from
//! confetti. Weights shape it; the graph partition guarantees it.
//!
//! # Where the through-streets come from
//!
//! A plain Voronoi diagram of scattered plots has no long road in it. Every
//! junction is three-way at about 120°, the stroke rule's 40° continuation limit
//! stops there, and the plan reads as soap foam — the accretion prototype's own
//! honest complaint about itself: *"no road runs more than a couple of blocks
//! straight."*
//!
//! A road here is the perpendicular bisector of two neighbouring plots, so a
//! **run of collinear roads needs a run of facing pairs**: plots in two parallel
//! rows. The previous attempt at that drew the rows on straight cuts out of a
//! partition of the plane, which is where the avenues — and the pie chart — came
//! from.
//!
//! This grows them instead, off the terrain. The local street frame is the
//! **contour direction and the fall line** ([`Settlement::grid_frame`]): a
//! candidate is rewarded for sitting one separation from the nearest settled
//! plot *along* one of those two axes rather than at some angle between them.
//! The terrain is low-frequency fbm, so the frame turns slowly — one quarter's
//! streets are square to each other, the next quarter's are square to each other
//! on a different bearing, and where a valley bends the grid bends with it.
//! That is what a hill town does, it has no centre and no radius in it anywhere,
//! and it is measured: at 5 000 files it takes the strokes past a quarter of the
//! city diameter from **7 to 20**, and the longest stroke from 37 % of the
//! diameter to 45 %. The wavelength the frame is read at is the whole of it —
//! see [`FRAME_SCALE`] for the table, and for the one setting that overshot.
//!
//! # The age gradient is structural
//!
//! `sep` and the plot capacity both ramp with growth progress, so the historic
//! core is settled at a finer separation with fewer files per plot than the
//! recent rim. The old town therefore has a finer street mesh and smaller blocks
//! than the periphery — PRD §7.1's age structure expressed as structure, not as
//! a colour ramp.

// The middle of the pipeline is numeric geometry, and five lint families fire on
// nearly every line of it without telling us anything:
//
// * the `cast_*` family — every cast here lands in a bucket index or a quantised
//   sort key that is clamped or wrapped on purpose;
// * `float_cmp` — exact float comparison is how a determinism tie is broken
//   (PRD §7.4), and an approximate comparison there would be the bug;
// * `many_single_char_names` and `similar_names` — `a`, `b`, `c`, `n`, `p` are
//   the names the geometry itself uses;
// * `too_many_lines` — a pipeline stage read as one ordered sequence is clearer
//   than the same code cut into fragments each called once;
// * `assigning_clones` — the buffers reassigned here are rebuilt from scratch,
//   so `clone_from` would save nothing.
#![allow(
    clippy::assigning_clones,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::float_cmp,
    clippy::many_single_char_names,
    clippy::similar_names,
    clippy::too_many_lines
)]

use polis_events::{LogicalPath, WallTime};

use crate::age::AgeRamp;
use crate::determinism::{combine_seeds, det_sin_cos, quantize_f64, SeededRng, TAU};
use crate::geom::{add, dist, mul, qp, sub, Pt};
use crate::terrain::TerrainField;
use crate::territory::Territory;

/// Area one plot occupies, in units of `sep²`.
///
/// A hexagonal lattice at spacing `sep` gives `√3/2 ≈ 0.866`; a jammed random
/// packing is looser. Measured against the settled area at both scales.
pub(crate) const PACKING: f64 = 1.18;

/// Frontier a district is given beyond the ground its plots occupy.
pub(crate) const BASE_SLACK: f64 = 1.10;

/// Coordinate scale the street frame's orientation field is sampled at.
///
/// The frame comes from the terrain gradient, and the terrain is sized for
/// *relief* — hills a few blocks across, which is what makes a slope worth
/// avoiding. Read at that scale the frame turns within one block, neighbouring
/// plots inherit different bearings, and no row of facing pairs ever forms.
/// Sampling the same field at `0.15` of the coordinate stretches its wavelength
/// by a factor of seven, so the bearing is near-constant across a quarter and
/// drifts across the city.
///
/// Measured at 5 000 files — strokes past a quarter of the diameter, and the
/// longest as a share of it, against the 8 and the 35-70 % the gate asks for:
///
/// | scale | `w_grid` | through-streets | longest |
/// |---|---|---|---|
/// | - | 0.0 | 7 | 37.0 % |
/// | 1.00 | 2.6 | 6 | 31.6 % |
/// | 1.00 | 6.0 | 12 | 48.7 % |
/// | 0.22 | 2.6 | 13 | 39.5 % |
/// | 0.15 | 2.0 | 14 | 41.4 % |
/// | **0.15** | **2.6** | **20** | **44.8 %** |
/// | 0.15 | 4.0 | 19 | 50.9 % |
/// | 0.15 | 1.5 | 24 | 74.7 % - *past the ceiling* |
/// | 0.10 | 2.6 | 14 | 48.9 % |
///
/// The wavelength is what matters and the weight is second order: every setting
/// at `0.15` clears the bar, and the one at `1.00` with the same weight does
/// not. `1.5` is excluded not for being weak but for making one stroke run three
/// quarters of the way across the city, which is a boulevard and not a street.
const FRAME_SCALE: f64 = 0.15;

/// How many settled plots one round of the search grows its candidates from.
const SEARCH_HOSTS: usize = 32;

/// How far the search may work outward before it gives up on this rung.
const SEARCH_ROUNDS: usize = 4;

/// How much further each round reaches than the one before it.
///
/// Three, so five rounds cover `3⁴ = 81` bands — past the far side of any city
/// this pipeline builds — in five queries rather than nine. A query at radius
/// `r` costs a scan of every plot inside it, so the round count is the cost.
const ROUND_GROWTH: f64 = 3.0;

/// Largest the normalised terrain height can be, in units of the relief.
///
/// The noise is a sum of octaves normalised to `[-1, 1]`, and the district
/// biases are applied after the growth, so one is the bound with a little
/// headroom. It is used to prune candidates that cannot win — see
/// [`Settlement::evaluate`] — so it must be an over-estimate and never an
/// under-estimate.
const HEIGHT_BOUND: f64 = 1.05;

/// Directions tried when asking whether a plot still has room beside it.
const OPEN_PROBES: u32 = 16;

/// How far out those probes sit, in units of the coarsest separation.
///
/// Inside the legal band: a plot that has room at `1.35 · sep_rim` has room for
/// a neighbour of any grain, and one that does not is enclosed.
const OPEN_RADIUS: f64 = 1.35;

/// Rings across the legal band round each host plot.
const BAND_RINGS: u32 = 3;

/// Positions per ring.
///
/// Sixteen is 22.5°, which at one separation is 0.39 of a separation between
/// neighbouring candidates — finer than the quantisation the packing rule can
/// tell apart once the four exact frame slots are in the list too.
const BAND_ANGLES: u32 = 10;

/// How many of a district's own outermost plots anchor its search.
const ANCHOR_FRONTIER: usize = 6;

/// How many of the town's outermost plots anchor the last rung of the ladder.
const TOWN_FRONTIER: usize = 24;

/// Radius of the neighbour query around a candidate, in units of `sep`.
///
/// Must exceed [`Params::touch_max`], or the connectivity rule would be decided
/// on a neighbour set that does not contain the neighbour that satisfies it.
const NEIGHBOUR_WINDOW: f64 = 1.9;

/// One file as the growth simulation sees it.
#[derive(Debug, Clone)]
pub(crate) struct FileRec {
    /// The layout key (PRD §7.6) and the seed of every draw about this file.
    pub(crate) path: LogicalPath,
    /// Size in bytes; footprint area is proportional to its square root.
    pub(crate) size_bytes: u64,
    /// Index in the git growth sequence; `u32::MAX` for an untracked file.
    pub(crate) growth_index: u32,
    /// Commit time of the commit that first added the file
    /// ([`polis_events::WallTime::UNIX_EPOCH`] for an untracked one). This, not
    /// `growth_index`, is what [`crate::age`] calibrates the ramp on.
    pub(crate) added_at: WallTime,
    /// `node_modules`, `vendor`, `target`: drawn as one dull mass (PRD §8).
    pub(crate) industrial: bool,
    /// An entry point or a top-decile import target (PRD §8).
    pub(crate) monument: bool,
}

/// A settled parcel of ground: the generator of one Voronoi cell.
#[derive(Debug, Clone)]
pub(crate) struct Plot {
    /// Where it sits, quantised.
    pub(crate) pos: Pt,
    /// Territory node index of the district that owns it.
    pub(crate) district: u32,
    /// Files living here, in growth order.
    pub(crate) files: Vec<u32>,
    /// How many files it holds before the district must settle another.
    pub(crate) cap: u32,
    /// Growth step at which it was settled — the plot's own age.
    pub(crate) birth: u32,
}

/// Running state of one district's ground.
#[derive(Debug, Clone, Default)]
pub(crate) struct DistrictGround {
    /// Plot ids, in settlement order.
    pub(crate) plots: Vec<u32>,
    /// Sum of plot positions, for the running centre of mass.
    sum: Pt,
    /// Max distance of a plot from that centre.
    radius: f64,
}

impl DistrictGround {
    /// Centre of mass of the district's plots.
    pub(crate) fn centroid(&self) -> Pt {
        if self.plots.is_empty() {
            [0.0, 0.0]
        } else {
            mul(self.sum, 1.0 / self.plots.len() as f64)
        }
    }
}

/// The tuning of the growth process.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Params {
    /// Minimum plot separation in the historic core.
    pub(crate) sep_core: f64,
    /// Minimum plot separation on the recent periphery.
    pub(crate) sep_rim: f64,
    /// Plot capacity in the core: a fine mesh and small blocks.
    pub(crate) cap_core: u32,
    /// Plot capacity on the rim: a coarse mesh and big planned blocks.
    pub(crate) cap_rim: u32,
    /// Seed of the terrain field the growth reads.
    pub(crate) terrain_seed: u64,
    /// Weight on staying near the district's own centre of mass.
    pub(crate) w_compact: f64,
    /// Weight on hugging the existing settlement.
    ///
    /// **Low, and that is the coastline.** A town that hugs itself hard fills
    /// its own convex hull; one that only prefers to stay in touch grows round
    /// what the ground puts in its way. Measured across four corpora, solidity
    /// (built area over convex hull; a coin is 1.00) at `w_slope = 6`,
    /// `w_noise = 2.5`:
    ///
    /// | `w_hug` | synthetic 5k | Django | Neovim | `CPython` |
    /// |---|---|---|---|---|
    /// | 2.20 | 0.786 | 0.837 | — | — |
    /// | **1.00** | **0.862** | **0.859** | **0.762** | **0.756** |
    ///
    /// The gate asks for under 0.90 and the previous convex city limit measured
    /// 0.9947–0.9994.
    pub(crate) w_hug: f64,
    /// Weight on avoiding steep ground.
    ///
    /// The other half of the coastline, and the physical one: a town does not
    /// build on a cliff, so a ridge is an edge of town and a valley floor is a
    /// gap in it. At `2.6` the synthetic measured 0.822 solidity and Django
    /// 0.903 — over the bar; at `6.0` they are 0.862 and 0.859.
    ///
    /// The term did nothing at all until this commit, and not because it was
    /// small: [`Settlement::new`] was handed `TerrainField::default()`, whose
    /// relief is zero, and the real field was attached to the settlement
    /// *after* the growth had finished. Every plot in every city this pipeline
    /// has ever built was placed on ground it could not see.
    pub(crate) w_slope: f64,
    /// Weight on not interleaving with a neighbouring district.
    pub(crate) w_foreign: f64,
    /// Weight on high ground.
    ///
    /// Positive: settle the shoulders and the ridges, leave the wet ground.
    /// With [`Params::w_slope`] this is what makes the outline follow the
    /// terrain rather than a circle, and it is why the same field that bends the
    /// streets ([`FRAME_SCALE`]) also shapes the coast.
    pub(crate) w_noise: f64,
    /// Weight on touching the district's own ground.
    pub(crate) w_adjacent: f64,
    /// Bonus when the nearest plot of all is a sibling.
    pub(crate) w_touch: f64,
    /// Weight on sitting square to the local street frame.
    ///
    /// The through-street term. Zero gives the accretion prototype's soap foam.
    /// It is second order to the wavelength the frame is read at — see
    /// [`FRAME_SCALE`] for the measured table of both together.
    pub(crate) w_grid: f64,
    /// Furthest a new plot may sit from the **nearest settled plot of any
    /// district**, in units of `sep`.
    ///
    /// The connectivity rule, and the one the prototype in
    /// `docs/design/accretion` used: a new quarter buds onto the town rather
    /// than being founded across a gap. It is what makes `components = 1` and
    /// what keeps the phantom ring ([`crate::voronoi`]) to the outside of the
    /// settlement, where a hole in the map is the edge of town rather than a
    /// void in the middle of it.
    ///
    /// `1.75` is the prototype's measured value. Below about `1.5` the frontier
    /// has too few legal positions and the growth crawls along a one-plot-wide
    /// tendril; above about `2.1` a plot can settle far enough out to leave a
    /// pocket the phantom lattice fills, which is a hole.
    pub(crate) touch_max: f64,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            sep_core: 0.55,
            sep_rim: 2.75,
            cap_core: 2,
            cap_rim: 9,
            terrain_seed: 0x504f_4c49_5300_0001,
            w_compact: 2.20,
            w_hug: 1.00,
            w_slope: 6.00,
            w_foreign: 3.40,
            w_noise: 2.50,
            w_adjacent: 2.00,
            w_touch: 1.60,
            w_grid: 3.40,
            touch_max: 1.75,
        }
    }
}

impl Params {
    /// The tuning for a repository of `files` files.
    ///
    /// A small repository needs finer ground to be a town at all; a large one
    /// needs coarser ground to stay inside PRD §13.1's cold-start budget.
    pub(crate) fn for_file_count(files: usize) -> Self {
        let mut p = Self::default();
        if files < 400 {
            p.cap_core = 1;
            p.cap_rim = 2;
        } else if files < 2_000 {
            p.cap_core = 2;
            p.cap_rim = 5;
        }
        p
    }

    /// Plot separation at growth progress `t ∈ [0, 1]`.
    pub(crate) fn sep_at(&self, t: f64) -> f64 {
        let t = t.clamp(0.0, 1.0).powf(0.62);
        self.sep_core + (self.sep_rim - self.sep_core) * t
    }

    /// Plot capacity at growth progress `t ∈ [0, 1]`.
    pub(crate) fn cap_at(&self, t: f64) -> u32 {
        let t = t.clamp(0.0, 1.0).powf(0.62);
        let c = f64::from(self.cap_core) + (f64::from(self.cap_rim) - f64::from(self.cap_core)) * t;
        c.round().max(1.0) as u32
    }

    /// Ground one plot occupies at age `t`.
    pub(crate) fn plot_area_at(&self, t: f64) -> f64 {
        let sep = self.sep_at(t);
        sep * sep * PACKING
    }
}

/// Uniform spatial hash over plot positions.
///
/// # Why this is not a `BTreeMap`
///
/// The rest of this crate uses `BTreeMap` because iteration order reaches the
/// output and PRD §7.4 forbids that. **Nothing iterates this one.** Both readers
/// look up a fixed rectangle of cells by key and reduce what they find to
/// minima and counts, which are order-independent to the last bit, and the one
/// reader that returns a list sorts it. So the ordering guarantee a tree buys is
/// worth nothing here, and its `O(log n)` lookup is worth a great deal: the
/// growth's candidate loop is the hot path of the whole pipeline — Django's
/// 2 413 plots evaluate **20.8 million candidates**, each scanning a few dozen
/// cells — and a tree walk per cell was two thirds of a twelve-second
/// generation.
///
/// It is a hand-written open-addressing table rather than a `HashMap` for two
/// reasons: the workspace pins no hash crate for `polis-layout`, and `std`'s
/// `RandomState` is seeded per process, which is exactly the hazard the
/// `BTreeMap` rule exists to prevent. The hash here is a fixed integer mix, so
/// two runs probe the table in the same order — and even if they did not, the
/// answer would be the same.
#[derive(Debug, Clone, Default)]
struct CellMap {
    /// Slot keys; `None` for an empty slot.
    keys: Vec<Option<(i32, i32)>>,
    /// Plot ids per slot.
    vals: Vec<Vec<u32>>,
    /// Occupied slots.
    len: usize,
}

impl CellMap {
    fn new() -> Self {
        Self {
            keys: vec![None; 64],
            vals: vec![Vec::new(); 64],
            len: 0,
        }
    }

    #[inline]
    fn hash(k: (i32, i32)) -> u64 {
        let x = (i64::from(k.0) as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let y = (i64::from(k.1) as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
        crate::determinism::mix64(x ^ y.rotate_left(31))
    }

    #[inline]
    fn slot(&self, k: (i32, i32)) -> usize {
        let mask = self.keys.len() - 1;
        let mut i = (Self::hash(k) as usize) & mask;
        loop {
            match self.keys[i] {
                None => return i,
                Some(other) if other == k => return i,
                Some(_) => i = (i + 1) & mask,
            }
        }
    }

    fn push(&mut self, k: (i32, i32), id: u32) {
        if (self.len + 1) * 2 > self.keys.len() {
            self.grow();
        }
        let i = self.slot(k);
        if self.keys[i].is_none() {
            self.keys[i] = Some(k);
            self.len += 1;
        }
        self.vals[i].push(id);
    }

    #[inline]
    fn get(&self, k: (i32, i32)) -> Option<&Vec<u32>> {
        let i = self.slot(k);
        if self.keys[i].is_some() {
            Some(&self.vals[i])
        } else {
            None
        }
    }

    fn grow(&mut self) {
        let bigger = self.keys.len() * 2;
        let old_keys = std::mem::replace(&mut self.keys, vec![None; bigger]);
        let old_vals = std::mem::replace(&mut self.vals, vec![Vec::new(); bigger]);
        self.len = 0;
        for (k, v) in old_keys.into_iter().zip(old_vals) {
            if let Some(k) = k {
                let i = self.slot(k);
                self.keys[i] = Some(k);
                self.vals[i] = v;
                self.len += 1;
            }
        }
    }
}

/// Uniform spatial hash over plot positions.
#[derive(Debug, Clone)]
struct Grid {
    cell: f64,
    buckets: CellMap,
}

impl Grid {
    fn new(cell: f64) -> Self {
        Self {
            cell: cell.max(1e-6),
            buckets: CellMap::new(),
        }
    }

    #[inline]
    fn key(&self, p: Pt) -> (i32, i32) {
        (
            (p[0] / self.cell).floor() as i32,
            (p[1] / self.cell).floor() as i32,
        )
    }

    fn insert(&mut self, p: Pt, id: u32) {
        let k = self.key(p);
        self.buckets.push(k, id);
    }

    /// Fold over every plot id within `r` of `p`, without allocating.
    ///
    /// **No order is imposed and none is needed.** The caller reduces to minima
    /// and counts, and both are order-independent to the last bit — a minimum of
    /// a set of `f64` is the same however the set is walked, and a count is a
    /// count.
    fn scan(&self, p: Pt, r: f64, mut f: impl FnMut(u32)) {
        self.scan_while(p, r, |i| {
            f(i);
            true
        });
    }

    /// [`Self::scan`], but the visitor may stop the walk by returning `false`.
    ///
    /// The growth's candidate loop rejects a position the instant it finds a
    /// settled plot inside the packing distance, and that verdict cannot be
    /// changed by anything else in the window — so finishing the scan is pure
    /// waste, and it is waste paid 2.7 million times on Django, where the dense
    /// core makes most candidates collide on one of the first plots visited.
    /// Stopping early cannot move the city: `collision` is a property of the
    /// set, not of the order it is visited in (PRD §7.4).
    fn scan_while(&self, p: Pt, r: f64, mut f: impl FnMut(u32) -> bool) {
        let n = (r / self.cell).ceil() as i32;
        let (kx, ky) = self.key(p);
        for dy in -n..=n {
            for dx in -n..=n {
                if let Some(b) = self.buckets.get((kx + dx, ky + dy)) {
                    for &i in b {
                        if !f(i) {
                            return;
                        }
                    }
                }
            }
        }
    }

    /// Every plot id within `r` of `p`, in ascending id order.
    fn query(&self, p: Pt, r: f64, out: &mut Vec<u32>) {
        out.clear();
        self.scan(p, r, |i| out.push(i));
        out.sort_unstable();
    }
}

/// How far a placement had to bend the rules.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Relaxations {
    /// Touching the district's own ground, and the town's. The good case.
    pub(crate) clean: usize,
    /// Touching the town but **not** its own district's ground.
    ///
    /// The growth's shaping weights could not find a legal position beside a
    /// sibling, so the plot budded onto a stranger. It is not a failure — the
    /// graph partition in [`crate::regions`] fixes district shape afterwards and
    /// does not care where a plot came from — but it is the number that says how
    /// hard the partition has to work, so it is counted.
    pub(crate) nonadjacent: usize,
    /// Unused since the territory constraint was removed. Kept at zero so the
    /// report's shape does not change.
    pub(crate) fringe: usize,
    /// Unused since the territory constraint was removed.
    pub(crate) ancestor: usize,
    /// Placed on the town's own frontier, because nothing near the district's
    /// own ground was legal. See [`Settlement::frontier_anchors`].
    pub(crate) anywhere: usize,
    /// Placed with no territory at all, because nothing else fitted.
    pub(crate) detached: usize,
}

/// The settled ground.
#[derive(Debug, Clone)]
pub(crate) struct Settlement {
    /// Plots, in settlement order.
    pub(crate) plots: Vec<Plot>,
    /// Ground held by each territory district, indexed by territory node.
    pub(crate) ground: Vec<DistrictGround>,
    /// Files, in growth order.
    pub(crate) files: Vec<FileRec>,
    /// `files[i]` lives in `plots[file_plot[i]]`.
    pub(crate) file_plot: Vec<u32>,
    /// The directory tree the growth reads and [`crate::regions`] partitions.
    pub(crate) territory: Territory,
    /// The height field the growth reads (PRD §7.2 step 1).
    pub(crate) terrain: TerrainField,
    /// The tuning.
    pub(crate) params: Params,
    /// How often each rule had to bend.
    pub(crate) relaxed: Relaxations,
    /// Growth index to age of the ground, calibrated on real commit time
    /// (PRD §7.1). Frozen at generation: see [`crate::age`].
    pub(crate) ramp: AgeRamp,
    grid: Grid,
    /// Running sum of every plot position, for the town's own centre of mass.
    town_sum: Pt,
    /// Furthest any plot is from it.
    town_radius: f64,
    /// Which plots still have room beside them at the coarsest grain.
    ///
    /// The **frontier**, maintained rather than searched for. A district
    /// founded last month is settled at five times the separation of the old
    /// town (PRD §7.1's ramp), so no position anywhere in the old town is legal
    /// for it and it has to reach the edge. Finding the edge by sorting the
    /// whole settlement by distance, once per such plot, is what made Django
    /// take eleven seconds; this is the same answer for the cost of re-testing
    /// a handful of plots whenever one is settled.
    open_rim: Vec<bool>,
    step: u32,
    scratch: Vec<u32>,
}

impl Settlement {
    /// An empty settlement over a fixed district tree.
    pub(crate) fn new(
        territory: Territory,
        params: Params,
        terrain: TerrainField,
        ramp: AgeRamp,
        total_files: u32,
    ) -> Self {
        let ground = vec![DistrictGround::default(); territory.nodes.len()];
        let _ = total_files;
        Self {
            plots: Vec::new(),
            ground,
            files: Vec::new(),
            file_plot: Vec::new(),
            territory,
            terrain,
            relaxed: Relaxations::default(),
            ramp,
            grid: Grid::new(params.sep_rim * 1.25),
            town_sum: [0.0, 0.0],
            town_radius: 0.0,
            open_rim: Vec::new(),
            step: 0,
            scratch: Vec::new(),
            params,
        }
    }

    /// The age of a district, in `[0, 1]`: 0 was founded in the repository's
    /// first year, 1 last month.
    ///
    /// **A district's grain follows its own age, not the clock.** An early
    /// directory that is still receiving files keeps the fine mesh it was
    /// founded with, which is both what an old quarter actually does and what
    /// makes the territory partition binding: a face is laid out for the grain
    /// its files will be settled at, so the ground fits. Using global progress
    /// instead means an old district's late file demands rim-sized spacing
    /// inside a core-sized polygon, it does not fit, and 83 % of plots end up
    /// settling in somebody else's territory.
    ///
    /// The number comes from [`crate::age::AgeRamp`] — **real commit time**,
    /// not the district's position in the file ordering. A repository whose
    /// first year produced a twentieth of its files still gets an old town, and
    /// one imported in a single commit gets no invented gradient at all.
    pub(crate) fn district_age(&self, did: u32) -> f64 {
        self.ramp.at(self.territory.nodes[did as usize].own_oldest)
    }

    /// Plot separation in a district.
    pub(crate) fn sep_of(&self, did: u32) -> f64 {
        self.params.sep_at(self.district_age(did))
    }

    /// The local street frame: `(along the contour, up the fall line)`.
    ///
    /// No angle is ever formed — the two axes come straight out of the terrain
    /// gradient, one normalised and one its perpendicular — so there is no
    /// `atan2` and no trigonometry in the growth's hot loop at all (PRD §7.4).
    ///
    /// On genuinely flat ground the gradient vanishes and the frame falls back
    /// to the world axes, which is the right answer: a plain has no reason to
    /// prefer one bearing, and a whole quarter agreeing on one is what a grid
    /// laid out on a plain looks like.
    fn grid_frame(gx: f64, gy: f64) -> (Pt, Pt) {
        let l = (gx * gx + gy * gy).sqrt();
        if l < 1e-9 {
            return ([1.0, 0.0], [0.0, 1.0]);
        }
        let up = [gx / l, gy / l];
        ([-up[1], up[0]], up)
    }

    /// How square a candidate sits to the local frame, in `[0, 1]`.
    ///
    /// One is exactly a whole number of separations from the anchor along both
    /// axes; zero is diagonal to both. The anchor is the **nearest settled
    /// plot**, so the frame is inherited from the neighbour rather than from an
    /// absolute lattice: separations ramp with age, and an absolute lattice
    /// would go out of phase the moment the grain changed.
    fn frame_fit(v: Pt, along: Pt, up: Pt, pitch: f64) -> f64 {
        if pitch <= 1e-9 {
            return 0.0;
        }
        let u = (v[0] * along[0] + v[1] * along[1]) / pitch;
        let w = (v[0] * up[0] + v[1] * up[1]) / pitch;
        let fu = (u - u.round()).abs();
        let fw = (w - w.round()).abs();
        (1.0 - 2.0 * fu).max(0.0) * (1.0 - 2.0 * fw).max(0.0)
    }

    /// The district a file belongs to: its parent directory, or the nearest
    /// ancestor the territory knows about.
    fn district_of(&self, path: &LogicalPath) -> u32 {
        let mut candidate = Some(path.parent().unwrap_or_else(LogicalPath::root));
        while let Some(p) = candidate {
            if let Some(id) = self.territory.get(&p) {
                return id;
            }
            candidate = p.parent();
        }
        0
    }

    /// Reserve PRD §8's civic square: an open plot at the historic centre.
    ///
    /// > **Civic square** — repo root / top-level config — a recognisable open
    /// > space at the historic centre.
    ///
    /// A plot with capacity zero. It is settled before any file, so it is the
    /// oldest ground in the city; nothing can move into it, so its cell has no
    /// buildings and reads as a square. Making it a *plot* rather than hunting
    /// for an empty block afterwards is what makes it reliably central and
    /// reliably there.
    fn reserve_civic(&mut self) {
        if !self.plots.is_empty() || self.territory.nodes.is_empty() {
            return;
        }
        let root = self.territory.root();
        // The origin, because the origin is where the first file would have
        // settled anyway: the growth starts from nothing and hugs itself, so the
        // oldest ground is the ground around the first plot. There is no city
        // limit to take a centroid of any more, and there does not need to be.
        self.settle_plot(root, [0.0, 0.0], 0);
        // Counted like any other placement, so the ladder's tallies add up to
        // the plot count exactly and a plot can never go unaccounted for.
        self.relaxed.clean += 1;
    }

    /// One growth step: place one file.
    ///
    /// This is the whole incremental story. There is no separate batch path to
    /// keep in sync with it: generating a city calls this once per file in
    /// growth order, and adding a file to a live city calls it once.
    pub(crate) fn add_file(&mut self, rec: FileRec) -> u32 {
        let fid = u32::try_from(self.files.len()).expect("file count fits in u32");
        self.reserve_civic();
        let did = self.district_of(&rec.path);

        // 1. An existing plot of this district with room, nearest the district's
        //    centre of mass. Ties broken by plot id, never by iteration order.
        let cent = self.ground[did as usize].centroid();
        let mut chosen: Option<(f64, u32)> = None;
        for &pi in &self.ground[did as usize].plots {
            let pl = &self.plots[pi as usize];
            if (pl.files.len() as u32) < pl.cap {
                let dd = quantize_f64(dist(pl.pos, cent));
                let better = chosen.is_none_or(|(bd, bi)| dd < bd || (dd == bd && pi < bi));
                if better {
                    chosen = Some((dd, pi));
                }
            }
        }

        let plot = if let Some((_, pi)) = chosen {
            pi
        } else {
            let seed = combine_seeds(rec.path.layout_seed(), u64::from(self.step));
            let cap = self.params.cap_at(self.district_age(did));
            let pos = self.settle_position(did, seed);
            self.settle_plot(did, pos, cap)
        };

        self.plots[plot as usize].files.push(fid);
        self.files.push(rec);
        self.file_plot.push(plot);
        self.step += 1;
        fid
    }

    /// Find ground for a new plot of `did`, relaxing in a fixed order.
    ///
    /// Two rungs, not six. The hard rules — the packing distance and
    /// [`Params::touch_max`] — hold on both; what is dropped between them is the
    /// *preference* for budding onto the district's own ground. Each rung is
    /// counted, so "the district shaping is binding" is a number in the report
    /// rather than a claim in a comment.
    fn settle_position(&mut self, did: u32, seed: u64) -> Pt {
        let anchors = self.anchors(did, seed);
        // A district's very first plot has no ground of its own to touch, so the
        // sibling rule does not apply to it — it is the root of the induction,
        // not an exception to it.
        let rooted = !self.ground[did as usize].plots.is_empty();
        if rooted {
            if let Some(p) = self.search(did, &anchors, seed, true) {
                self.relaxed.clean += 1;
                return p;
            }
        }
        if let Some(p) = self.search(did, &anchors, seed, false) {
            if rooted {
                self.relaxed.nonadjacent += 1;
            } else {
                self.relaxed.clean += 1;
            }
            return p;
        }
        // Nothing legal near its own ground: take the edge of town, which always
        // has room. See [`Settlement::frontier_anchors`].
        let edge = self.frontier_anchors();
        if let Some(p) = self.search(did, &edge, seed, false) {
            self.relaxed.anywhere += 1;
            return p;
        }
        // Nothing legal anywhere at all. Counted, and never silent — this is
        // asserted to be zero at every scale, on every corpus.
        self.relaxed.detached += 1;
        qp(anchors.first().copied().unwrap_or([0.0, 0.0]))
    }

    /// Anchors for the frontier search.
    ///
    /// Deliberately **not** biased toward the district's outermost parcels. An
    /// outward bias is a positive feedback loop — each new plot extends the tip
    /// the next one grows from — and it produces long spindly arms instead of
    /// quarters. The search sweeps outward from the centre of mass, with a
    /// second ring pre-advanced to the frontier so a full district does not
    /// re-scan its own interior, plus a few sampled parcels so a quarter can
    /// still bulge where there is room.
    fn anchors(&self, did: u32, seed: u64) -> Vec<Pt> {
        let ground = &self.ground[did as usize];
        if ground.plots.is_empty() {
            return self.seed_anchors(did);
        }
        let c = ground.centroid();
        let mut out = vec![qp(c)];
        // The district's own outermost plots — its frontier — plus a few
        // sampled from the interior so a quarter can still bulge where there is
        // room. Outermost rather than a ring of empty space: the candidate
        // generator works outward from *plots*, so an anchor that is not near
        // one buys nothing.
        let mut ranked: Vec<(f64, u32)> = ground
            .plots
            .iter()
            .map(|&pi| (quantize_f64(dist(self.plots[pi as usize].pos, c)), pi))
            .collect();
        ranked.sort_by(|x, y| y.0.total_cmp(&x.0).then_with(|| x.1.cmp(&y.1)));
        for (_, pi) in ranked.iter().take(ANCHOR_FRONTIER) {
            out.push(self.plots[*pi as usize].pos);
        }
        let n = ground.plots.len();
        let mut rng = SeededRng::for_seed(seed, "frontier.pick");
        for _ in 0..3.min(n) {
            let idx = (rng.next_f64() * n as f64) as usize % n;
            out.push(self.plots[ground.plots[idx] as usize].pos);
        }
        out.sort_by(|a, b| a[0].total_cmp(&b[0]).then_with(|| a[1].total_cmp(&b[1])));
        out.dedup();
        out
    }

    /// Centre of mass of the whole settlement.
    fn town_centre(&self) -> Pt {
        if self.plots.is_empty() {
            [0.0, 0.0]
        } else {
            qp(mul(self.town_sum, 1.0 / self.plots.len() as f64))
        }
    }

    /// Anchors on the **town's** own frontier, where there is always room.
    ///
    /// The last rung of the ladder, and the one that makes "every plot has a
    /// legal position" true rather than hoped for. A district deep inside a
    /// crowded quarter can have no legal position anywhere near its own ground:
    /// every ring around its centre of mass is either inside one separation of a
    /// neighbour or past [`Params::touch_max`] from any plot at all. Measured on
    /// Django — 7 014 files in 1 950 directories, the densest tree of the three
    /// real repositories — that was 125 of 2 413 plots placed on top of their
    /// own district's centre of mass, which is not a position, it is a
    /// collision.
    ///
    /// The edge of town always has room, because it has open country on one
    /// side. Sweeping it is the difference between a plot that had to found a
    /// new quarter and a plot with no place to be.
    fn frontier_anchors(&self) -> Vec<Pt> {
        if self.plots.is_empty() {
            return vec![[0.0, 0.0]];
        }
        let c = self.town_centre();
        let mut ranked: Vec<(f64, u32)> = self
            .plots
            .iter()
            .enumerate()
            .map(|(i, p)| {
                (
                    quantize_f64(dist(p.pos, c)),
                    u32::try_from(i).expect("plot count fits in u32"),
                )
            })
            .collect();
        ranked.sort_by(|x, y| y.0.total_cmp(&x.0).then_with(|| x.1.cmp(&y.1)));
        ranked.truncate(TOWN_FRONTIER);
        ranked
            .into_iter()
            .map(|(_, pi)| self.plots[pi as usize].pos)
            .collect()
    }

    /// Where a district with no ground yet should start looking.
    ///
    /// On its **parent's** frontier, so `polis-layout/src` buds onto
    /// `polis-layout` and siblings fan around the directory that made them
    /// (PRD §9). Failing that, on the frontier of the nearest ancestor that has
    /// ground; failing that, on the town's own frontier; and with no settlement
    /// at all, the origin — which is the civic square.
    fn seed_anchors(&self, did: u32) -> Vec<Pt> {
        if self.plots.is_empty() {
            return vec![[0.0, 0.0]];
        }
        let mut up = self.territory.nodes[did as usize].parent;
        while let Some(a) = up {
            let g = &self.ground[a as usize];
            if !g.plots.is_empty() {
                let c = g.centroid();
                let mut ranked: Vec<(f64, u32)> = g
                    .plots
                    .iter()
                    .map(|&pi| (quantize_f64(dist(self.plots[pi as usize].pos, c)), pi))
                    .collect();
                ranked.sort_by(|x, y| y.0.total_cmp(&x.0).then_with(|| x.1.cmp(&y.1)));
                ranked.truncate(ANCHOR_FRONTIER);
                return ranked
                    .into_iter()
                    .map(|(_, pi)| self.plots[pi as usize].pos)
                    .collect();
            }
            up = self.territory.nodes[a as usize].parent;
        }
        // No ancestor has ground: bud onto the town itself.
        self.frontier_anchors()
    }

    /// Score every legal position near the anchors and take the best.
    ///
    /// # Where the candidates come from, and why not from rings round the anchor
    ///
    /// A plot may sit no closer than `sep` to a settled plot and no further than
    /// [`Params::touch_max`] separations from the nearest one. That is a **band
    /// round every existing plot**, and it is the whole of the legal set — so
    /// the generator enumerates it directly: for each plot near an anchor, a few
    /// rings inside the band, plus the four slots square to the local street
    /// frame.
    ///
    /// The previous generator swept seventy-two rings outward from the anchor
    /// itself and tested each. It found the same positions and it cost, at
    /// Django's 2 413 plots, **12.9 seconds of a 13.1-second generation** — four
    /// times PRD §13.1's whole cold-start budget — because all but a handful of
    /// those rings lie outside the band and were generated and rejected one
    /// candidate at a time.
    ///
    /// `touch` asks that the nearest settled plot be one of the district's own.
    /// The packing distance and the reach are hard rejects inside the candidate
    /// loop, so no weight can outvote them and no build profile can skip them.
    fn search(&mut self, did: u32, anchors: &[Pt], seed: u64, touch: bool) -> Option<Pt> {
        let sep = self.sep_of(did);
        let dcent = self.ground[did as usize].centroid();
        let has_ground = !self.ground[did as usize].plots.is_empty();
        let mut ctx = Candidate {
            did,
            sep,
            touch,
            has_ground,
            dcent,
            home: anchors.first().copied().unwrap_or([0.0, 0.0]),
            along: [1.0, 0.0],
            up: [0.0, 1.0],
        };
        let mut rng = SeededRng::for_seed(seed, "plot.position");
        let base = rng.next_f64() * TAU;
        let mut best: Option<(f64, Pt)> = None;
        if self.plots.is_empty() {
            for a in anchors {
                if let Some(score) = self.evaluate(*a, &ctx, f64::NEG_INFINITY) {
                    accept(&mut best, score, *a);
                }
            }
            return best.map(|(_, p)| p);
        }
        // Two passes, and the split is what keeps the cost down.
        //
        // **The near pass** grows candidates from the plots within one band of
        // an anchor. Almost every plot settles here: it is the ground the
        // district is already on, and it costs one small spatial query.
        //
        // **The far pass** is for the plot that has no legal position near its
        // own ground at all — which is not rare and is not a failure. PRD §7.1's
        // ramp means a directory founded last month is settled at five times the
        // separation of the old town, and *no* position in the old town is five
        // separations from every plot in it. Such a district has to reach the
        // edge, and the far pass sorts the whole neighbourhood by distance
        // **once** and then strides outward through it in widening prefixes.
        //
        // Sorting once rather than once per round is the difference between
        // Django generating in 2 seconds and in 8: a query at a large radius
        // costs a scan of everything inside it, and the previous shape of this
        // loop paid that for every round.
        let band = sep * (self.params.touch_max + 1.0);
        let mut near: Vec<u32> = Vec::new();
        for a in anchors {
            self.grid.query(*a, band, &mut self.scratch);
            near.extend_from_slice(&self.scratch);
        }
        near.sort_unstable();
        near.dedup();
        if near.len() > SEARCH_HOSTS {
            // Nearest the district's own centre of mass. A cap, not a choice:
            // the near pass runs for every plot in the city and a big district's
            // anchors can reach a hundred plots, most of which are the same
            // ground seen from two anchors.
            let home = ctx.home;
            let mut ranked: Vec<(f64, u32)> = near
                .iter()
                .map(|&pi| (quantize_f64(dist(self.plots[pi as usize].pos, home)), pi))
                .collect();
            ranked.sort_by(|x, y| x.0.total_cmp(&y.0).then_with(|| x.1.cmp(&y.1)));
            ranked.truncate(SEARCH_HOSTS);
            near = ranked.into_iter().map(|(_, pi)| pi).collect();
            near.sort_unstable();
        }
        self.try_hosts(&near, base, &mut ctx, &mut best);
        if best.is_some() {
            return best.map(|(_, p)| p);
        }
        let mut done: Vec<u32> = near;

        // The far pass: the frontier plots nearest the district's own ground,
        // widening until one of them has room. `open_rim` is maintained by the
        // growth, so this is a scan of a flag array and a sort of a few hundred
        // entries rather than a sort of the whole settlement.
        let mut ranked: Vec<(f64, u32)> = self
            .open_rim
            .iter()
            .enumerate()
            .filter(|(_, o)| **o)
            .map(|(i, _)| {
                (
                    quantize_f64(dist(self.plots[i].pos, ctx.home)),
                    u32::try_from(i).expect("plot count fits in u32"),
                )
            })
            .filter(|(_, pi)| done.binary_search(pi).is_err())
            .collect();
        ranked.sort_by(|x, y| x.0.total_cmp(&y.0).then_with(|| x.1.cmp(&y.1)));
        let mut radius = band;
        for round in 0..SEARCH_ROUNDS {
            radius *= ROUND_GROWTH;
            let inside = ranked.partition_point(|(d, _)| *d <= radius);
            let inside = if round + 1 == SEARCH_ROUNDS {
                ranked.len()
            } else {
                inside
            };
            if inside == 0 {
                continue;
            }
            // A **stride** through the prefix, not its nearest fortieth: the
            // nearest open plots are all on one side of the district, and a
            // district that cannot fit there should try the other side before it
            // tries forty more positions on this one.
            let stride = inside.div_ceil(SEARCH_HOSTS).max(1);
            let batch: Vec<u32> = ranked[..inside]
                .iter()
                .step_by(stride)
                .map(|(_, pi)| *pi)
                .collect();
            self.try_hosts(&batch, base, &mut ctx, &mut best);
            if best.is_some() {
                break;
            }
            done.extend_from_slice(&batch);
            done.sort_unstable();
        }
        best.map(|(_, p)| p)
    }

    /// Offer every legal position round each host plot and keep the best.
    fn try_hosts(
        &mut self,
        hosts: &[u32],
        base: f64,
        ctx: &mut Candidate,
        best: &mut Option<(f64, Pt)>,
    ) {
        let sep = ctx.sep;
        for (hi, &pi) in hosts.iter().enumerate() {
            let q = self.plots[pi as usize].pos;
            // Square to the local street frame first: the four positions that
            // make a straight bisector, offered exactly rather than approached.
            let (_, gx, gy) = self.terrain.sample(q[0] * FRAME_SCALE, q[1] * FRAME_SCALE);
            let (along, up) = Self::grid_frame(gx, gy);
            ctx.along = along;
            ctx.up = up;
            for axis in [along, up] {
                for sign in [1.0f64, -1.0] {
                    let c = qp([q[0] + axis[0] * sep * sign, q[1] + axis[1] * sep * sign]);
                    if let Some(score) =
                        self.evaluate(c, ctx, best.map_or(f64::NEG_INFINITY, |(b, _)| b))
                    {
                        accept(best, score, c);
                    }
                }
            }
            // …then the rest of the band.
            for step in 0..BAND_RINGS {
                let t = f64::from(step) / f64::from(BAND_RINGS - 1);
                let r = sep * (1.0 + (self.params.touch_max - 1.0) * t);
                for k in 0..BAND_ANGLES {
                    let ang = base
                        + TAU * f64::from(k) / f64::from(BAND_ANGLES)
                        + (hi as f64) * 0.271_828;
                    let (sn, cs) = det_sin_cos(ang);
                    let c = qp([q[0] + cs * r, q[1] + sn * r]);
                    if let Some(score) =
                        self.evaluate(c, ctx, best.map_or(f64::NEG_INFINITY, |(b, _)| b))
                    {
                        accept(best, score, c);
                    }
                }
            }
        }
    }

    /// Score one candidate position, or reject it.
    ///
    /// `None` means the candidate broke a hard rule — the packing distance,
    /// [`Params::touch_max`], or the sibling-contact preference when it is
    /// binding — and those are tested before any weight is consulted, so no
    /// score can outvote them.
    fn evaluate(&mut self, c: Pt, ctx: &Candidate, floor: f64) -> Option<f64> {
        let sep = ctx.sep;
        // Wide enough that the connectivity rule is decided inside it: the
        // window is 2.6 separations and `touch_max` is 1.75, so a plot that
        // would satisfy the rule can never be outside the query.
        let window = sep * NEIGHBOUR_WINDOW;
        let mut nearest = f64::INFINITY;
        let mut nearest_same = f64::INFINITY;
        let mut foreign = 0.0f64;
        let mut samey = 0.0f64;
        let mut anchor: Option<Pt> = None;
        let mut collision = false;
        let plots = &self.plots;
        let did = ctx.did;
        self.grid.scan_while(c, window, |pi| {
            let pl = &plots[pi as usize];
            let dd = dist(c, pl.pos);
            if dd < sep - 1e-6 {
                collision = true;
                return false;
            }
            if dd < window {
                if pl.district == did {
                    samey += 1.0;
                } else {
                    foreign += 1.0;
                }
            }
            if pl.district == did {
                nearest_same = nearest_same.min(dd);
            }
            if dd < nearest {
                nearest = dd;
                anchor = Some(pl.pos);
            }
            true
        });
        if collision {
            return None;
        }
        if !self.plots.is_empty() {
            // The connectivity rule. Measured against the plot's own separation,
            // so the coarse rim is allowed the same *relative* reach as the fine
            // core rather than being held to the core's absolute distance.
            if !nearest.is_finite() || nearest > sep * self.params.touch_max {
                return None;
            }
        }
        // The sibling preference. `nearest` and `nearest_same` were measured
        // over the same window, so a sibling that wins inside it wins outright:
        // nothing outside the window can be nearer than something inside it.
        if ctx.touch && !crate::districts::touches_own(nearest, nearest_same) {
            return None;
        }
        if !nearest.is_finite() {
            nearest = 0.0; // the first plot in the world
        }
        let p = self.params;
        let mut score = -p.w_hug * (nearest / sep);
        if let Some(a) = anchor {
            score += p.w_grid * Self::frame_fit(sub(c, a), ctx.along, ctx.up, sep);
        }
        if ctx.has_ground {
            score -= p.w_compact * (dist(c, ctx.dcent) / sep) * 0.55;
            let tot = foreign + samey;
            if tot > 0.0 {
                score -= p.w_foreign * (foreign / tot);
            }
            if nearest_same.is_finite() {
                score -= p.w_adjacent * (nearest_same / sep);
                if nearest_same <= nearest + 1e-9 {
                    score += p.w_touch;
                }
            } else {
                score -= p.w_adjacent * 2.5;
            }
        } else {
            score -= 0.30 * (dist(c, ctx.home) / sep);
        }
        // The terrain is the expensive half of the score and the last thing
        // computed, because a candidate that cannot win even with the best
        // ground in the world does not need it.
        //
        // The bound is **exact**, not a heuristic: the slope term is a penalty
        // and can only lower the score, and the height term is at most
        // `w_noise · HEIGHT_BOUND` because the noise is normalised. A candidate
        // pruned here would have scored below the incumbent whatever the ground
        // under it, so the city is bit-for-bit the one the unpruned loop builds
        // — and at Django's scale it is the difference between five seconds and
        // two. The comparison is strict, so a tie still goes to `accept`'s
        // lexicographic rule (PRD §7.4).
        if score + p.w_noise * HEIGHT_BOUND < floor {
            return None;
        }
        let (h, gx, gy) = self.terrain.sample(c[0], c[1]);
        let relief = f64::from(self.terrain.relief()).max(1e-9);
        let slope = (gx * gx + gy * gy).sqrt();
        Some(score - p.w_slope * slope + p.w_noise * (h / relief))
    }

    /// Record a new plot.
    fn settle_plot(&mut self, did: u32, pos: Pt, cap: u32) -> u32 {
        let id = u32::try_from(self.plots.len()).expect("plot count fits in u32");
        self.plots.push(Plot {
            pos,
            district: did,
            files: Vec::new(),
            cap,
            // The plot's age is its district's, so the road mesh and the block
            // sizes around it follow the same ramp its spacing did. Stored raw,
            // `u32::MAX` included: the ramp, not a clamp, decides what an index
            // it never saw reads as.
            birth: self.territory.nodes[did as usize].own_oldest,
        });
        self.grid.insert(pos, id);
        // Optimistic: the refresh below tests it properly.
        self.open_rim.push(true);
        self.town_sum = add(self.town_sum, pos);
        // Updated with the new plot only, never rescanned: the centre drifts by
        // less than a separation per plot, so a running maximum is within one
        // plot of the true frontier and this stays a constant-time step
        // (PRD §13.1).
        self.town_radius = self.town_radius.max(dist(pos, self.town_centre()));
        let g = &mut self.ground[did as usize];
        g.plots.push(id);
        g.sum = add(g.sum, pos);
        let c = mul(g.sum, 1.0 / g.plots.len() as f64);
        let mut r = 0.0f64;
        for &pi in &self.ground[did as usize].plots {
            r = r.max(dist(self.plots[pi as usize].pos, c));
        }
        self.ground[did as usize].radius = r;
        self.refresh_open(pos);
        id
    }

    /// Re-test which plots near `at` still have room beside them.
    ///
    /// **Only the ones still marked open.** Settling a plot can take a free
    /// position away and can never give one back, so openness is monotone and a
    /// plot that has been enclosed stays enclosed. In a dense old quarter that
    /// is nearly every plot in range, which is what makes this a handful of
    /// tests per growth step rather than a few hundred. See
    /// [`Settlement::open_rim`].
    fn refresh_open(&mut self, at: Pt) {
        let sep = self.params.sep_rim;
        let band = sep * (self.params.touch_max + 1.0);
        let mut touched: Vec<u32> = Vec::new();
        self.grid.query(at, band + sep, &mut touched);
        for &pi in &touched {
            if !self.open_rim[pi as usize] {
                continue;
            }
            let q = self.plots[pi as usize].pos;
            let mut open = false;
            for k in 0..OPEN_PROBES {
                let ang = TAU * f64::from(k) / f64::from(OPEN_PROBES);
                let (sn, cs) = det_sin_cos(ang);
                let r = sep * OPEN_RADIUS;
                let c = qp([q[0] + cs * r, q[1] + sn * r]);
                let mut blocked = false;
                let plots = &self.plots;
                self.grid.scan(c, sep, |oi| {
                    if dist(c, plots[oi as usize].pos) < sep - 1e-6 {
                        blocked = true;
                    }
                });
                if !blocked {
                    open = true;
                    break;
                }
            }
            self.open_rim[pi as usize] = open;
        }
    }

    /// Growth progress of the plot nearest a point, in `[0, 1]`.
    ///
    /// **The age of the ground.** The prune ramp reads it, so blocks merge more
    /// on the recent periphery than in the historic core — PRD §7.1's age
    /// structure expressed as block size rather than as a colour ramp.
    pub(crate) fn age_at(&self, p: Pt) -> f64 {
        let mut scratch: Vec<u32> = Vec::new();
        let mut best: Option<(f64, u32)> = None;
        let mut r = self.params.sep_rim * 2.0;
        for _ in 0..6 {
            self.grid.query(p, r, &mut scratch);
            for &pi in &scratch {
                let pl = &self.plots[pi as usize];
                let d = dist(p, pl.pos);
                if best.is_none_or(|(bd, bb)| d < bd || (d == bd && pl.birth < bb)) {
                    best = Some((d, pl.birth));
                }
            }
            if best.is_some() {
                break;
            }
            r *= 2.0;
        }
        best.map_or(0.0, |(_, b)| self.ramp.at(b))
    }

    /// The district a file belongs to, by its path.
    pub(crate) fn district_of_file(&self, fi: u32) -> u32 {
        self.district_of(&self.files[fi as usize].path)
    }

    /// This settlement with [`crate::regions`]' partition applied.
    ///
    /// A copy, deliberately. The raw growth is what the next incremental step
    /// continues from, so the partition must not be able to feed back into it:
    /// a city grown one file at a time and one generated from scratch would
    /// then be different cities, and PRD §7.7 forbids that.
    pub(crate) fn reseated(&self, seating: &crate::regions::Seating) -> Self {
        let mut out = self.clone();
        for (i, plot) in out.plots.iter_mut().enumerate() {
            if let Some(&d) = seating.plot_district.get(i) {
                plot.district = d;
            }
            if let Some(files) = seating.plot_files.get(i) {
                plot.files = files.clone();
            }
        }
        out.file_plot = seating.file_plot.clone();
        out.ground = vec![DistrictGround::default(); out.territory.nodes.len()];
        for (i, plot) in out.plots.iter().enumerate() {
            let id = u32::try_from(i).expect("plot count fits in u32");
            let g = &mut out.ground[plot.district as usize];
            g.plots.push(id);
            g.sum = add(g.sum, plot.pos);
        }
        for d in 0..out.ground.len() {
            let c = out.ground[d].centroid();
            let mut r = 0.0f64;
            for &pi in &out.ground[d].plots {
                r = r.max(dist(out.plots[pi as usize].pos, c));
            }
            out.ground[d].radius = r;
        }
        out
    }

    /// Every plot position, in plot order.
    pub(crate) fn positions(&self) -> Vec<Pt> {
        self.plots.iter().map(|p| p.pos).collect()
    }
}

impl Default for Settlement {
    fn default() -> Self {
        Self::new(
            Territory::default(),
            Params::default(),
            TerrainField::default(),
            AgeRamp::default(),
            1,
        )
    }
}

/// Keep the better of the running best and a new candidate; a tie goes to the
/// lexicographically smaller point, never to the arrival order (PRD §7.4).
fn accept(best: &mut Option<(f64, Pt)>, score: f64, c: Pt) {
    let better =
        best.is_none_or(|(bs, bp)| score > bs || (score == bs && (c[0], c[1]) < (bp[0], bp[1])));
    if better {
        *best = Some((score, c));
    }
}

/// Everything a candidate is judged against that does not change between
/// candidates.
///
/// Bundled so [`Settlement::evaluate`] can be one function shared by the radial
/// sweep and the lattice, rather than two copies of the rule set that a later
/// change could let drift apart.
#[derive(Clone, Copy)]
struct Candidate {
    /// The district asking for ground.
    did: u32,
    /// Its plot separation.
    sep: f64,
    /// Ask that the nearest plot of all be a sibling.
    touch: bool,
    /// Whether the district has ground already.
    has_ground: bool,
    /// The district's centre of mass.
    dcent: Pt,
    /// The first anchor, used only before the district has ground.
    home: Pt,
    /// The local street frame, along the contour and up the fall line.
    ///
    /// Carried rather than sampled per candidate. It is read off a field
    /// stretched to seven times the terrain's wavelength ([`FRAME_SCALE`]), so
    /// it is constant to three decimal places across one plot's neighbourhood —
    /// and sampling it per candidate doubled the cost of the growth's hot loop
    /// for an answer that did not change.
    along: Pt,
    up: Pt,
}

/// Replay a file list in growth order.
///
/// The list is sorted on `(growth_index, path)` **here**, so the order the
/// caller happened to collect the files in cannot reach the city (PRD §7.4).
pub(crate) fn grow(
    mut files: Vec<FileRec>,
    territory: Territory,
    params: Params,
    ramp: AgeRamp,
    terrain: TerrainField,
) -> Settlement {
    files.sort_by(|a, b| (a.growth_index, a.path.as_str()).cmp(&(b.growth_index, b.path.as_str())));
    let total = u32::try_from(files.len()).unwrap_or(u32::MAX);
    let mut s = Settlement::new(territory, params, terrain, ramp, total);
    for f in files {
        s.add_file(f);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::territory;

    fn lp(s: &str) -> LogicalPath {
        LogicalPath::new(s).expect("a valid test path")
    }

    fn corpus(paths: &[&str]) -> Vec<FileRec> {
        paths
            .iter()
            .enumerate()
            .map(|(i, p)| FileRec {
                path: lp(p),
                size_bytes: 1_000 + (i as u64) * 37,
                growth_index: u32::try_from(i).expect("small"),
                added_at: crate::age::tests::even_history(i, paths.len()),
                industrial: p.starts_with("vendor/"),
                monument: false,
            })
            .collect()
    }

    /// The ramp the production pipeline would calibrate for a corpus.
    fn ramp_of(files: &[FileRec]) -> AgeRamp {
        AgeRamp::calibrate(
            &files
                .iter()
                .map(|f| (f.growth_index, f.added_at))
                .collect::<Vec<_>>(),
        )
    }

    fn settle(paths: &[&str]) -> Settlement {
        settle_records(corpus(paths))
    }

    fn settle_records(files: Vec<FileRec>) -> Settlement {
        let params = Params::for_file_count(files.len());
        let terrain = crate::terrain::TerrainField::generate(0x51, 100.0);
        // The same demand model the generator uses, not a copy of it: a test
        // that sizes the territory differently from production is testing a
        // city nobody ships.
        let ramp = ramp_of(&files);
        let t = territory::build(&crate::districts::demands(&files), &|p| {
            p.as_str().starts_with("vendor")
        });
        grow(files, t, params, ramp, terrain)
    }

    fn sample_paths() -> Vec<&'static str> {
        vec![
            "README.md",
            "Cargo.toml",
            "src/lib.rs",
            "src/main.rs",
            "src/auth/session.rs",
            "src/auth/token.rs",
            "src/auth/oauth.rs",
            "src/net/server.rs",
            "src/net/client.rs",
            "docs/guide.md",
            "docs/api.md",
            "tests/it.rs",
            "vendor/blob.js",
            "vendor/other.js",
        ]
    }

    #[test]
    fn every_file_gets_ground() {
        let s = settle(&sample_paths());
        assert_eq!(s.files.len(), sample_paths().len());
        assert_eq!(s.file_plot.len(), s.files.len());
        assert!(!s.plots.is_empty());
        for (i, &p) in s.file_plot.iter().enumerate() {
            assert!(
                s.plots[p as usize].files.contains(&(i as u32)),
                "file {i} is not in the plot it claims"
            );
        }
    }

    #[test]
    fn plots_keep_their_separation() {
        let s = settle(&sample_paths());
        let sep = s.params.sep_core;
        for (i, a) in s.plots.iter().enumerate() {
            for b in &s.plots[i + 1..] {
                assert!(
                    dist(a.pos, b.pos) >= sep - 1e-3,
                    "two plots are {} apart, below sep {sep}",
                    dist(a.pos, b.pos)
                );
            }
        }
    }

    #[test]
    fn every_placement_is_counted() {
        let s = settle(&sample_paths());
        assert_eq!(
            s.relaxed.detached, 0,
            "a plot was placed with no legal position at all"
        );
        assert_eq!(
            s.relaxed.clean + s.relaxed.nonadjacent + s.relaxed.detached,
            s.plots.len(),
            "a plot was placed without being counted"
        );
    }

    /// The rule that makes `components = 1` structural: no plot is founded
    /// across a gap.
    #[test]
    fn every_plot_after_the_first_touches_the_town() {
        let s = settle(&sample_paths());
        let reach = s.params.sep_rim * s.params.touch_max;
        for (i, p) in s.plots.iter().enumerate().skip(1) {
            let nearest = s.plots[..i]
                .iter()
                .map(|q| dist(p.pos, q.pos))
                .fold(f64::INFINITY, f64::min);
            assert!(
                nearest <= reach + 1e-6,
                "plot {i} settled {nearest} from the nearest ground, past {reach}"
            );
        }
    }

    #[test]
    fn input_order_cannot_move_the_settlement() {
        let paths = sample_paths();
        let forward = settle_records(corpus(&paths));
        // Permute the *input list*, not the history: a file's growth index is a
        // property of the repository, and reversing the paths would have
        // reversed the commit order too, which is a different city by design.
        let mut shuffled = corpus(&paths);
        shuffled.reverse();
        shuffled.rotate_left(3);
        let backward = settle_records(shuffled);
        // `grow` sorts, so the two runs must place the same plots. Compare the
        // whole plot list, positions included.
        assert_eq!(forward.plots.len(), backward.plots.len());
        for (a, b) in forward.plots.iter().zip(backward.plots.iter()) {
            assert_eq!(a.pos, b.pos);
            assert_eq!(a.district, b.district);
            assert_eq!(a.files, b.files);
        }
    }

    #[test]
    fn the_core_is_finer_than_the_rim() {
        let p = Params::default();
        assert!(p.sep_at(0.0) < p.sep_at(1.0));
        assert!(p.cap_at(0.0) <= p.cap_at(1.0));
        assert!(p.plot_area_at(1.0) > p.plot_area_at(0.0));
    }

    /// The street frame is a pair of perpendicular unit axes, and a candidate
    /// square to it scores 1 while a diagonal one scores nothing.
    #[test]
    fn the_street_frame_rewards_a_square_neighbour() {
        let (along, up) = Settlement::grid_frame(0.0, 2.0);
        assert!((along[0].abs() - 1.0).abs() < 1e-9 || (along[1].abs() - 1.0).abs() < 1e-9);
        assert!((along[0] * up[0] + along[1] * up[1]).abs() < 1e-9);
        let square = Settlement::frame_fit([0.0, 1.0], along, up, 1.0);
        let diagonal = Settlement::frame_fit([0.707, 0.707], along, up, 1.0);
        assert!(square > 0.99, "a square neighbour scored {square}");
        assert!(diagonal < 0.2, "a diagonal neighbour scored {diagonal}");
    }

    #[test]
    fn a_full_plot_pushes_the_district_outward() {
        // One directory, more files than one plot can hold: the district must
        // settle more ground rather than overfilling a plot.
        let paths: Vec<String> = (0..12).map(|i| format!("pkg/f{i}.rs")).collect();
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let s = settle(&refs);
        assert!(s.plots.len() > 1, "one plot held twelve files");
        for p in &s.plots {
            assert!(
                p.files.len() as u32 <= p.cap,
                "a plot holds {} files with capacity {}",
                p.files.len(),
                p.cap
            );
        }
    }
}
