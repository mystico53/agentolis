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
//! # The three hard constraints
//!
//! Everything else is a weight; these three are absolute, and they are rejects
//! inside the candidate loop rather than penalties on the score, so no amount of
//! weight can outvote them.
//!
//! 1. **Never closer than `sep` to a settled plot.** This is what bounds cell
//!    size, and therefore block size.
//! 2. **Never outside its own district's territory** ([`crate::territory`],
//!    [`crate::districts`] rule T). This is the graft from `treemap-arterials`.
//! 3. **Never out of contact with its own district's ground**
//!    ([`crate::districts`] rule A): after a district's first plot, the nearest
//!    plot in the whole city must be one of its own. The nearest-neighbour graph
//!    is a subgraph of the Delaunay graph, so this makes the new cell share an
//!    edge with a sibling's — and a district's blocks one connected region.
//!
//! Rules 2 and 3 together are what turn "districts are contiguous" from a
//! measurement into a property of the construction; [`crate::districts`] carries
//! the argument in full.
//!
//! # Why contact is with a *sibling*, and why the city is still connected
//!
//! The design this port is based on had a contact rule too, but a different one:
//! a new plot had to be within `1.75 × sep` of an existing plot **of any
//! district**. That rule is what gave it one connected component — and it is
//! also, on measurement, what stopped the territory constraint from ever
//! binding. A district whose polygon had not yet been reached by the growth
//! front failed the contact test inside its own ground and settled in an
//! ancestor's instead: **83 % of plots at 5 000 files**, which left districts as
//! fragmented as they were without a partition at all.
//!
//! Rule 3 asks the opposite question. Contact with a *foreign* plot is what a
//! district must be free to do without, so that it can start its own quarter on
//! its own ground; contact with its *own* is always available, because the
//! district's previous plot is right there. The rule that used to fight the
//! partition now serves it.
//!
//! Global connectivity does not depend on either version. The faces **tile** the
//! city limit, every face belongs to a district that has files, and each face is
//! laid out at [`crate::territory::AREA_SLACK`] times the ground its plots need — so the
//! finished settlement covers the whole limit at a density whose nearest-
//! neighbour spacing is a small multiple of `sep`, and no gap wide enough to
//! separate two Voronoi cells can open. `components = 1` is now a property of
//! the partition rather than of a radius, and it is asserted rather than hoped
//! for. Hugging the settlement survives as a *weight*, which is what makes the
//! ground fill inward from a district's border with its neighbour rather than
//! starting in the middle of its own polygon.
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

use std::collections::BTreeMap;

use polis_events::{LogicalPath, WallTime};

use crate::age::AgeRamp;
use crate::determinism::{combine_seeds, det_sin_cos, quantize_f64, SeededRng, TAU};
use crate::geom::{
    add, contains, dist, dist_to_boundary, dist_to_seg, dot, mul, norm, qp, sub, Pt,
};
use crate::terrain::TerrainField;
use crate::territory::Territory;

/// Area one plot occupies, in units of `sep²`.
///
/// A hexagonal lattice at spacing `sep` gives `√3/2 ≈ 0.866`; a jammed random
/// packing is looser. Measured against the settled area at both scales.
pub(crate) const PACKING: f64 = 1.18;

/// Frontier a district is given beyond the ground its plots occupy.
pub(crate) const BASE_SLACK: f64 = 1.10;

/// Width of the border band a district loses to its neighbours, in units of
/// `sep`, per unit of perimeter. See [`Params::district_demand`].
pub(crate) const BORDER_BAND: f64 = 1.30;

/// Where a plot aligned on the **city limit** wants to sit, in units of `sep`.
const AVENUE_OFFSET: f64 = 0.5;

/// Beyond this distance, in units of `sep`, the city limit exerts no pull.
const AVENUE_REACH: f64 = 1.55;

/// Beyond this distance, in units of the lattice pitch, a desire line exerts no
/// pull.
///
/// The brief's "~1.5× the local separation": two rows either side of the line at
/// ±pitch/2, and nothing beyond them.
const DESIRE_REACH: f64 = 1.5;

/// Rungs either side of an anchor's own that the lattice offers as candidates.
const DESIRE_RUNGS: i64 = 3;

/// Lattice pitch, as a multiple of the coarsest separation on the line.
///
/// **Not 1.** A rung's two slots can only be filled by plots that are there to
/// fill them, and the count is `pitch² / (1.18 · sep²)` plots per slot: at
/// `pitch = sep` that is 0.85, so most slots go empty, runs of two are the best
/// the line can do, and the avenue never forms. Measured at 5 000 files with
/// `1.0`: 223 plots exactly on a lattice, 44 facing pairs, **longest run 2**.
///
/// The cost is that the parcels fronting an avenue are `LATTICE_PITCH²` times
/// the ground of an ordinary parcel of the same age — which is what a main road
/// looks like anyway, and is a good part of where the block size hierarchy comes
/// from.
const LATTICE_PITCH: f64 = 1.80;

/// Extra weight on a lattice point whose mirror across the line is settled.
///
/// Completing a facing pair is what puts a segment of the desire line itself on
/// the map, so it is worth more than merely being on the lattice.
const FACING_BONUS: f64 = 1.90;

/// Extra weight on a lattice point next to a settled one on the same side.
const ALONG_BONUS: f64 = 1.30;

/// How far a plot must keep off an avenue, in units of `sep`.
///
/// An avenue is a quarter boundary, and a quarter's cells are clipped to it
/// exactly ([`crate::voronoi`]). A plot that settles right against one therefore
/// gets a cell squeezed between its own bisectors and the line — a sliver, whose
/// long thin edge runs a hair off the avenue and crosses it. Measured at 3 000
/// files: seven slivers and seven crossings, all of them one plot pressed
/// against a quarter boundary.
///
/// Half a separation, which is where the lattice would have put the plot anyway.
const AVENUE_KEEPOUT: f64 = 0.30;

/// Desire lines a single anchor may draw candidates from.
///
/// Two, not one: cuts cross, and the plot at a crossing should be offered both
/// lattices rather than only whichever happens to be marginally nearer.
const DESIRE_LINES_PER_ANCHOR: usize = 2;

/// How many rings the placement search sweeps outward.
const RADIAL_STEPS: usize = 72;

/// A plot closer than this to a district border, in units of `sep`, is rejected
/// on the first attempt: its cell would straddle the border.
const BORDER_KEEPOUT: f64 = 0.12;

/// How far outside its own polygon a plot may settle, in units of `sep`, before
/// the search gives up and hands it to an ancestor.
///
/// A district's polygon is sized for the ground its plots need, but the plots of
/// the district next door press up against the shared border, and a plot may
/// come no closer than `sep` to one of them. For a small district that exclusion
/// band is most of its polygon, and the search fails inside ground that is
/// genuinely its own. Letting the *centre* sit half a separation over the line
/// costs nothing — the block still takes its district from the plot, not from
/// the geometry — and it is the difference between the territory constraint
/// binding on a third of the plots and on nearly all of them.
const FRINGE: f64 = 0.55;

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
    pub(crate) w_hug: f64,
    /// Weight on avoiding steep ground.
    pub(crate) w_slope: f64,
    /// Weight on not interleaving with a neighbouring district.
    pub(crate) w_foreign: f64,
    /// Weight on the path-seeded irregularity.
    pub(crate) w_noise: f64,
    /// Weight on touching the district's own ground.
    pub(crate) w_adjacent: f64,
    /// Bonus when the nearest plot of all is a sibling.
    pub(crate) w_touch: f64,
    /// Weight on lining up along the city limit.
    pub(crate) w_avenue: f64,
    /// Weight on sitting exactly on a desire line's lattice (Graft 2).
    pub(crate) w_lattice: f64,
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
            w_hug: 2.20,
            w_slope: 2.60,
            w_foreign: 3.40,
            w_noise: 0.85,
            w_adjacent: 2.00,
            w_touch: 1.60,
            w_avenue: 3.60,
            w_lattice: 4.40,
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

    /// Ground a district of `files` files founded at age `t` needs.
    ///
    /// Three terms, and leaving out either of the last two is what makes the
    /// territory constraint unsatisfiable for small districts:
    ///
    /// * the plots themselves — **`ceil(files / cap)`**, not `files / cap`; a
    ///   district of three files with a capacity of five still needs one whole
    ///   plot, and rounding that down under-sizes every small district in the
    ///   repository;
    /// * a slack factor, so there is frontier to grow into;
    /// * a **border band**, because a plot may come no closer than one
    ///   separation to a plot on the other side of the border, so a district
    ///   effectively loses half a separation around its whole perimeter. The
    ///   band scales with the perimeter, which is why it is a square root: it
    ///   costs a two-plot district most of its polygon and a hundred-plot
    ///   district almost nothing.
    pub(crate) fn district_demand(&self, files: usize, t: f64) -> f64 {
        let plots = files.div_ceil(self.cap_at(t).max(1) as usize).max(1) as f64;
        let occupied = plots * self.plot_area_at(t);
        occupied * BASE_SLACK + BORDER_BAND * occupied.sqrt() * self.sep_at(t)
    }
}

/// Uniform spatial hash over plot positions.
///
/// Iterated only in a fixed cell order, and every query sorts its result, so no
/// iteration order can reach the output (PRD §7.4).
#[derive(Debug, Clone)]
struct Grid {
    cell: f64,
    buckets: BTreeMap<(i32, i32), Vec<u32>>,
}

impl Grid {
    fn new(cell: f64) -> Self {
        Self {
            cell: cell.max(1e-6),
            buckets: BTreeMap::new(),
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
        self.buckets.entry(k).or_default().push(id);
    }

    /// Every plot id within `r` of `p`, in ascending id order.
    fn query(&self, p: Pt, r: f64, out: &mut Vec<u32>) {
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

/// How far a placement had to bend the rules.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Relaxations {
    /// Placed inside the district's own face, keeping every rule. The good case.
    pub(crate) clean: usize,
    /// Placed inside the district's own face, but **not** touching its own
    /// ground — [`crate::districts`]'s rule A had to be dropped.
    ///
    /// This is the only placement left that can split a district in two, so it
    /// is counted separately from every other kind: it is the number that says
    /// how far "districts are contiguous by construction" is from literally
    /// true. It is 0 on the 5 000-file corpus and 0 on this repository.
    pub(crate) nonadjacent: usize,
    /// Placed within [`FRINGE`] of its own face, on the shared border.
    pub(crate) fringe: usize,
    /// Placed inside an ancestor's face: the district's own was full.
    pub(crate) ancestor: usize,
    /// Placed anywhere inside the city limit: no ancestor face had room.
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
    /// The territory map every plot is constrained by.
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
    /// Segments plots line up along: the city limit.
    alignments: Vec<(Pt, Pt)>,
    /// The district skeleton, promoted to desire lines (Graft 2).
    desire: Vec<DesireLine>,
    grid: Grid,
    step: u32,
    scratch: Vec<u32>,
    probe: Vec<u32>,
}

/// A trimmed partition cut, with the lattice plots settle onto.
///
/// # Why a lattice and not a pull
///
/// The bake-off's second defect was that the longest natural road stroke was
/// 28 % of the city diameter and every edge bent within a cell or two — the
/// soap-foam tell. Graft 2 is the accretion author's own answer to it: promote
/// the district territory boundaries to desire lines before accretion begins,
/// and let a plot near one settle **on a lattice aligned to that line**.
///
/// The straightness is emergent, which is the whole point. Nothing here draws a
/// road. A road in this city is the perpendicular bisector of two neighbouring
/// plots, and the bisector of `(t, −pitch/2)` and `(t, +pitch/2)` *is* the line,
/// exactly, for any `t`. A run of facing pairs on consecutive rungs therefore
/// shares one straight boundary — an avenue — and the same-side neighbours at
/// `t` and `t + pitch` give the cross streets that meet it. That is a street
/// grid falling out of where the plots are, not a chord drawn across the map,
/// which is why it does not reintroduce `treemap-arterials`' artefact.
///
/// Two properties are load-bearing:
///
/// * **Both sides share one lattice.** The pitch is a property of the *line*,
///   fixed before any plot settles, not of the plot's own district — a cut
///   separates an older half from a younger half by construction, so the two
///   sides have different separations, and a lattice keyed on the plot would put
///   the two rows out of phase and tilt every bisector.
/// * **The pitch is the coarser side's separation** ([`Settlement::new`]). The
///   packing rule rejects any plot within `sep` of another, so a lattice finer
///   than the coarser neighbour's `sep` would simply be rejected on that side
///   and the avenue would stop at the district border.
#[derive(Debug, Clone, Copy)]
struct DesireLine {
    /// The phase origin: one end of the trimmed cut.
    o: Pt,
    /// Unit vector along the line.
    u: Pt,
    /// Unit normal, `u` turned left.
    n: Pt,
    /// Length of the trimmed cut.
    len: f64,
    /// Row separation across the line and rung spacing along it.
    pitch: f64,
}

impl DesireLine {
    /// A point in the line's frame: distance along, signed distance across.
    fn frame(&self, p: Pt) -> (f64, f64) {
        let v = sub(p, self.o);
        (dot(v, self.u), dot(v, self.n))
    }

    /// How near this line's lattice a point is, in `[0, 1]`; 1 is exactly on it.
    ///
    /// Zero outside the line's own span, so a desire line pulls along the cut it
    /// came from and nowhere else.
    fn lattice_fit(&self, p: Pt) -> f64 {
        let (t, d) = self.frame(p);
        if t < -self.pitch || t > self.len + self.pitch {
            return 0.0;
        }
        let across = ((d.abs() - self.pitch * 0.5).abs() / (self.pitch * 0.5)).min(1.0);
        let rung = t / self.pitch;
        let along = ((rung - rung.round()).abs() / 0.25).min(1.0);
        (1.0 - across) * (1.0 - along)
    }

    /// The lattice point on rung `j`, `side` being −1 or +1.
    fn point(&self, j: i64, side: f64) -> Pt {
        let t = (j as f64) * self.pitch;
        qp(add(
            add(self.o, mul(self.u, t)),
            mul(self.n, side * self.pitch * 0.5),
        ))
    }
}

impl Settlement {
    /// An empty settlement over a fixed territory map.
    pub(crate) fn new(
        territory: Territory,
        params: Params,
        total_hint: u32,
        ramp: AgeRamp,
    ) -> Self {
        // The city limit aligns the outer ring of plots, which is what makes the
        // edge of town a drawn boundary rather than a ragged fringe. It is a
        // *pull* and not a lattice: the rim is a closed convex ring rather than
        // a cut between two quarters, so there is no "other side" to line up
        // with.
        let mut alignments = Vec::with_capacity(territory.rim.len());
        let rim = &territory.rim;
        for i in 0..rim.len() {
            alignments.push((rim[i], rim[(i + 1) % rim.len()]));
        }
        let total = total_hint.max(1);
        let desire = desire_lines(&territory, &params, total);
        let ground = vec![DistrictGround::default(); territory.nodes.len()];
        Self {
            plots: Vec::new(),
            ground,
            files: Vec::new(),
            file_plot: Vec::new(),
            territory,
            terrain: TerrainField::default(),
            relaxed: Relaxations::default(),
            ramp,
            alignments,
            desire,
            grid: Grid::new(params.sep_rim * 1.25),
            step: 0,
            scratch: Vec::new(),
            probe: Vec::new(),
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

    /// Height at a point, normalised by the field's relief.
    fn height(&self, p: Pt) -> f64 {
        let relief = f64::from(self.terrain.relief()).max(1e-9);
        self.terrain.height_f64(p[0], p[1]) / relief
    }

    /// Slope at a point, in units of `sep` of rise per `sep` of run.
    fn slope(&self, p: Pt) -> f64 {
        self.terrain.slope_f64(p[0], p[1])
    }

    /// The desire line whose lattice `p` fits best, and how well, in `[0, 1]`.
    ///
    /// A reward, never a rule: a plot that cannot sit on a lattice settles where
    /// it would have settled anyway.
    fn lattice_best(&self, p: Pt, sep: f64) -> Option<(usize, f64)> {
        let mut best: Option<(usize, f64)> = None;
        for (i, line) in self.desire.iter().enumerate() {
            let reach = line.pitch.max(sep) * DESIRE_REACH;
            let (a, b) = (line.o, add(line.o, mul(line.u, line.len)));
            if p[0] < a[0].min(b[0]) - reach
                || p[0] > a[0].max(b[0]) + reach
                || p[1] < a[1].min(b[1]) - reach
                || p[1] > a[1].max(b[1]) + reach
            {
                continue;
            }
            let fit = line.lattice_fit(p);
            if fit > 0.0 && best.is_none_or(|(_, bf)| fit > bf) {
                best = Some((i, fit));
            }
        }
        best
    }

    /// The lattice term in the placement score.
    ///
    /// # Why sitting on the lattice is not enough
    ///
    /// Rewarding the lattice alone puts a fifth of the plots on one — and buys
    /// almost no street. Measured at 5 000 files: 235 plots exactly on a
    /// lattice, **40 facing pairs, and the longest run of consecutive occupied
    /// rungs was 2**. A road needs a *run*: the boundary between rung `j`'s two
    /// plots is a segment of the desire line, and the avenue is what you get
    /// when rungs `j`, `j+1`, `j+2` … all have both sides filled. Scattered
    /// lattice plots give scattered one-cell segments, which is the foam again.
    ///
    /// The two bonuses are what turn occupancy into a street, and each is a
    /// property of the *street*, not of the plot:
    ///
    /// * **facing** — the mirror position across the line is already settled, so
    ///   taking this one puts a segment of the line itself on the map. A cut is
    ///   a district border, so the two sides are always different districts and
    ///   neither can place both halves: the pair can only ever be completed by
    ///   the second district arriving later and choosing the rung the first one
    ///   used. Nothing but this bonus would make it choose that rung.
    /// * **along** — a neighbouring rung on this side is already settled, so
    ///   taking this one extends an existing row rather than starting a new one.
    ///   This is ribbon development, and it is what makes runs longer than two.
    fn lattice_score(&mut self, c: Pt, sep: f64) -> f64 {
        let Some((i, fit)) = self.lattice_best(c, sep) else {
            return 0.0;
        };
        if fit <= 0.0 {
            return 0.0;
        }
        let line = self.desire[i];
        let (t, d) = line.frame(c);
        let j = (t / line.pitch).round() as i64;
        let side = if d < 0.0 { -1.0 } else { 1.0 };
        let tol = line.pitch * 0.35;
        let mut w = 1.0;
        if self.occupied(line.point(j, -side), tol) {
            w += FACING_BONUS;
        }
        if self.occupied(line.point(j - 1, side), tol)
            || self.occupied(line.point(j + 1, side), tol)
        {
            w += ALONG_BONUS;
        }
        fit * w
    }

    /// Whether the segment `a`–`b` crosses desire line `i`.
    fn crosses_desire(&self, i: usize, a: Pt, b: Pt) -> bool {
        let l = &self.desire[i];
        crate::geom::segments_properly_cross(a, b, l.o, add(l.o, mul(l.u, l.len)))
    }

    /// Whether `c` is too close to an avenue to have a cell of its own.
    fn too_near_avenue(&self, c: Pt, sep: f64) -> bool {
        let keep = sep * AVENUE_KEEPOUT;
        self.desire.iter().any(|line| {
            let (a, b) = (line.o, add(line.o, mul(line.u, line.len)));
            dist_to_seg(c, a, b) < keep
        })
    }

    /// Whether a plot already sits within `tol` of `p`.
    fn occupied(&mut self, p: Pt, tol: f64) -> bool {
        self.grid.query(p, tol, &mut self.probe);
        self.probe
            .iter()
            .any(|&i| dist(self.plots[i as usize].pos, p) <= tol)
    }

    /// Lattice positions worth trying, for a plot searching around `anchors`.
    ///
    /// Each anchor contributes the rungs nearest it on the [two][
    /// `DESIRE_LINES_PER_ANCHOR`] desire lines it is nearest, both sides. The
    /// result is sorted and deduplicated so the candidate order is a property of
    /// the geometry rather than of the anchor list (PRD §7.4), and every point
    /// still has to pass [`Settlement::evaluate`] — the lattice proposes, the
    /// rules dispose.
    fn lattice_candidates(&self, anchors: &[(Pt, f64)], sep: f64) -> Vec<Pt> {
        if self.desire.is_empty() {
            return Vec::new();
        }
        let mut out: Vec<Pt> = Vec::new();
        for (anchor, _) in anchors {
            let mut near: Vec<(f64, usize)> = Vec::new();
            for (i, line) in self.desire.iter().enumerate() {
                let (t, d) = line.frame(*anchor);
                let reach = line.pitch.max(sep) * (DESIRE_REACH + f64::from(DESIRE_RUNGS as i32));
                if t < -reach || t > line.len + reach || d.abs() > reach {
                    continue;
                }
                near.push((quantize_f64(d.abs()), i));
            }
            near.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
            near.truncate(DESIRE_LINES_PER_ANCHOR);
            for (_, i) in near {
                let line = &self.desire[i];
                let (t, _) = line.frame(*anchor);
                let home = (t / line.pitch).round() as i64;
                let last = (line.len / line.pitch).floor() as i64;
                for j in (home - DESIRE_RUNGS)..=(home + DESIRE_RUNGS) {
                    if j < 0 || j > last {
                        continue;
                    }
                    out.push(line.point(j, -1.0));
                    out.push(line.point(j, 1.0));
                }
            }
        }
        out.sort_by(|a, b| a[0].total_cmp(&b[0]).then_with(|| a[1].total_cmp(&b[1])));
        out.dedup();
        out
    }

    /// Distance from `p` to the nearest alignment segment, or infinity.
    fn alignment_distance(&self, p: Pt, reach: f64) -> f64 {
        let mut best = f64::INFINITY;
        for (a, b) in &self.alignments {
            // Cheap reject on the bounding box before the exact test.
            if p[0] < a[0].min(b[0]) - reach
                || p[0] > a[0].max(b[0]) + reach
                || p[1] < a[1].min(b[1]) - reach
                || p[1] > a[1].max(b[1]) + reach
            {
                continue;
            }
            best = best.min(dist_to_seg(p, *a, *b));
        }
        best
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
        let root = self.territory.get(&LogicalPath::root()).unwrap_or(0);
        let face = &self.territory.nodes[root as usize].face;
        let at = if face.len() >= 3 {
            qp(crate::geom::centroid(face))
        } else {
            [0.0, 0.0]
        };
        self.settle_plot(root, at, 0);
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
    /// The ladder drops [`crate::districts`]'s two rules one at a time, hardest
    /// last: first adjacency (rule A), then territory (rule T). Each rung is
    /// counted, so "the constraint is binding" is a number in the report rather
    /// than a claim in a comment.
    fn settle_position(&mut self, did: u32, seed: u64) -> Pt {
        let anchors = self.anchors(did, seed);
        // A district's very first plot has no ground of its own to touch, so
        // rule A does not apply to it — it is the root of the induction, not an
        // exception to it.
        let rooted = !self.ground[did as usize].plots.is_empty();

        // 1. Both rules: inside the district's own face, clear of its border,
        //    and touching the district's own ground.
        let own = self.territory.nodes[did as usize].face.clone();
        if own.len() >= 3 {
            // Rungs 1 and 2 are run twice: once with the street plan binding —
            // the lattice slots along the desire lines held open for the
            // district on the other side of the cut — and once without. A
            // reservation must never be the reason a plot leaves its own
            // territory, so it is dropped before rule T is.
            for plan in [true, false] {
                if let Some(p) = self.search(did, &anchors, seed, &own, true, 0.0, rooted, plan) {
                    self.relaxed.clean += 1;
                    return p;
                }
                // 2. Both rules, allowed right up to the border.
                if rooted {
                    if let Some(p) = self.search(did, &anchors, seed, &own, false, 0.0, true, plan)
                    {
                        self.relaxed.clean += 1;
                        return p;
                    }
                }
            }
            // 3. Rule A kept, rule T bent: half a separation over its own
            //    border, still in contact with its own ground.
            //
            //    This rung comes *before* dropping rule A on purpose. A plot
            //    whose centre sits 0.55 `sep` outside its polygon is still in
            //    its own quarter — the block takes its district from the plot,
            //    not from the geometry — whereas a plot out of contact with its
            //    own ground is a second piece of the district. Ordered the other
            //    way round, the 200-file corpus loses rule A once; ordered this
            //    way it never does, at any scale measured.
            let fringe = self.sep_of(did) * FRINGE;
            if rooted {
                if let Some(p) = self.search(did, &anchors, seed, &own, false, fringe, true, false)
                {
                    self.relaxed.fringe += 1;
                    return p;
                }
            }
            // 4. Rule T only: inside its own ground, but out of contact with the
            //    rest of it. This is the one placement left that can split a
            //    district, so it is counted apart from every other.
            if let Some(p) = self.search(did, &anchors, seed, &own, true, 0.0, false, false) {
                self.relaxed.nonadjacent += 1;
                return p;
            }
            if let Some(p) = self.search(did, &anchors, seed, &own, false, 0.0, false, false) {
                self.relaxed.nonadjacent += 1;
                return p;
            }
            // 5. Neither: on the fringe and out of contact.
            if let Some(p) = self.search(did, &anchors, seed, &own, false, fringe, false, false) {
                self.relaxed.fringe += 1;
                return p;
            }
        }
        // 6. The nearest ancestor whose whole subtree has room. Its subtree
        //    face, not its own: a district that has run out of ground belongs
        //    with its siblings inside the same quarter, and measurement says so
        //    — steering these plots onto the parent's own ground instead put the
        //    fragmented-district count up by half.
        let mut up = self.territory.nodes[did as usize].parent;
        while let Some(a) = up {
            let face = self.territory.nodes[a as usize].subtree_face.clone();
            if face.len() >= 3 {
                if let Some(p) = self.search(did, &anchors, seed, &face, false, 0.0, false, false) {
                    self.relaxed.ancestor += 1;
                    return p;
                }
            }
            up = self.territory.nodes[a as usize].parent;
        }
        // 7. Anywhere inside the city limit.
        let rim = self.territory.rim.clone();
        if rim.len() >= 3 {
            if let Some(p) = self.search(did, &anchors, seed, &rim, false, 0.0, false, false) {
                self.relaxed.anywhere += 1;
                return p;
            }
        }
        self.relaxed.detached += 1;
        qp(anchors.first().map_or([0.0, 0.0], |a| a.0))
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
    fn anchors(&self, did: u32, seed: u64) -> Vec<(Pt, f64)> {
        let sep = self.sep_of(did);
        let ground = &self.ground[did as usize];
        if ground.plots.is_empty() {
            return self.seed_anchors(did);
        }
        let c = ground.centroid();
        let mut out = vec![(c, 0.0)];
        if ground.radius > sep * 1.4 {
            out.push((c, ground.radius - sep * 1.1));
        }
        let n = ground.plots.len();
        let mut rng = SeededRng::for_seed(seed, "frontier.pick");
        for _ in 0..3.min(n) {
            let idx = (rng.next_f64() * n as f64) as usize % n;
            out.push((self.plots[ground.plots[idx] as usize].pos, 0.0));
        }
        out
    }

    /// Where a district with no ground yet should start looking.
    ///
    /// Inside its own face, as close as possible to ground that already exists —
    /// so a quarter buds onto the town that made it rather than being founded in
    /// the wilderness. With no settlement at all, the face's centroid: that is
    /// the repository root, and it is the civic square (PRD §8).
    fn seed_anchors(&self, did: u32) -> Vec<(Pt, f64)> {
        let face = &self.territory.nodes[did as usize].face;
        let probe = if face.len() >= 3 {
            face_probes(face)
        } else {
            vec![[0.0, 0.0]]
        };
        if self.plots.is_empty() {
            let mut out: Vec<(Pt, f64)> = probe.iter().map(|p| (*p, 0.0)).collect();
            out.truncate(4);
            return out;
        }
        let mut ranked: Vec<(f64, Pt)> = probe
            .into_iter()
            .map(|p| {
                let mut nearest = f64::INFINITY;
                for pl in &self.plots {
                    nearest = nearest.min(dist(p, pl.pos));
                }
                (quantize_f64(nearest), p)
            })
            .collect();
        ranked.sort_by(|a, b| {
            a.0.total_cmp(&b.0)
                .then_with(|| qp(a.1)[0].total_cmp(&qp(b.1)[0]))
                .then_with(|| qp(a.1)[1].total_cmp(&qp(b.1)[1]))
        });
        ranked.truncate(4);
        ranked.into_iter().map(|(_, p)| (p, 0.0)).collect()
    }

    /// Score candidate positions on rings around the anchors and take the best.
    ///
    /// `allowed`, `keep_out` and `fringe` are [`crate::districts`]'s rule T;
    /// `touch` is its rule A. Both are hard rejects inside the candidate loop —
    /// a candidate that fails either is never scored, so no weight can outvote
    /// them and no build profile can skip them.
    #[allow(clippy::too_many_arguments)] // one argument per rule, plus the search's own four
    fn search(
        &mut self,
        did: u32,
        anchors: &[(Pt, f64)],
        seed: u64,
        allowed: &[Pt],
        keep_out: bool,
        fringe: f64,
        touch: bool,
        plan: bool,
    ) -> Option<Pt> {
        let sep = self.sep_of(did);
        let mut rng = SeededRng::for_seed(seed, "plot.position");
        let base = rng.next_f64() * TAU;
        let dcent = self.ground[did as usize].centroid();
        let has_ground = !self.ground[did as usize].plots.is_empty();
        let keep_out_d = if keep_out { sep * BORDER_KEEPOUT } else { 0.0 };

        let ctx = Candidate {
            did,
            sep,
            keep_out_d,
            fringe,
            touch,
            has_ground,
            dcent,
            home: anchors.first().map_or([0.0, 0.0], |a| a.0),
            plan,
        };

        let mut best: Option<(f64, Pt)> = None;
        let mut found = 0u32;
        // Graft 2, step one: the lattice points. A plot whose search window
        // straddles a desire line is offered the positions *on that line's
        // lattice* before anything else, and they are scored by exactly the same
        // function as every other candidate — so a lattice point wins on merit
        // or not at all, and can never override rule T, rule A or the packing
        // distance. See [`Settlement::lattice_candidates`].
        let lattice = self.lattice_candidates(anchors, sep);
        for c in lattice {
            if let Some(score) = self.evaluate(c, &ctx, allowed) {
                found += 1;
                accept(&mut best, score, c);
            }
        }

        // The radial schedule has to sample **just outside one separation**
        // densely. A coarse one — the prototype stepped 0, 0.42, 0.95, 1.58 —
        // has no ring between 0.95 and 1.58 sep, so a candidate at 1.05 sep from
        // its neighbour is never generated at all. In a small district polygon
        // that is every legal position there is, the search fails, and the plot
        // relaxes into somebody else's territory. Measured: it was 70 % of them.
        for step in 0..RADIAL_STEPS {
            let rf = 0.18 * (step as f64).powf(1.12);
            for (ai, (anchor, abias)) in anchors.iter().enumerate() {
                let r = rf * sep + abias;
                let count = if r < 1e-9 {
                    1
                } else {
                    (TAU * r / (sep * 0.40)).ceil().clamp(12.0, 72.0) as u32
                };
                for k in 0..count {
                    let ang =
                        base + TAU * f64::from(k) / f64::from(count) + (ai as f64) * 0.271_828;
                    let (sn, cs) = det_sin_cos(ang);
                    let c = qp([anchor[0] + cs * r, anchor[1] + sn * r]);
                    if let Some(score) = self.evaluate(c, &ctx, allowed) {
                        found += 1;
                        accept(&mut best, score, c);
                    }
                }
            }
            if found >= 16 && step >= 8 {
                break;
            }
        }
        best.map(|(_, p)| p)
    }

    /// Score one candidate position, or reject it.
    ///
    /// `None` means the candidate broke a rule — rule T (`allowed`, `fringe`,
    /// `keep_out_d`), rule A (`touch`) or the packing distance — and those are
    /// tested before any weight is consulted, so no score can outvote them.
    fn evaluate(&mut self, c: Pt, ctx: &Candidate, allowed: &[Pt]) -> Option<f64> {
        let sep = ctx.sep;
        let inside = contains(allowed, c);
        if !inside && (ctx.fringe <= 0.0 || dist_to_boundary(allowed, c) > ctx.fringe) {
            return None;
        }
        if ctx.keep_out_d > 0.0 && inside && dist_to_boundary(allowed, c) < ctx.keep_out_d {
            return None;
        }
        // The street plan: keep off the avenues on the first pass. See
        // [`AVENUE_KEEPOUT`].
        if ctx.plan && self.too_near_avenue(c, sep) {
            return None;
        }
        // Which avenues run through the search window. Usually none, and then
        // the visibility test below costs nothing.
        let window = sep * 2.6;
        let near_lines: Vec<usize> = self
            .desire
            .iter()
            .enumerate()
            .filter(|(_, l)| dist_to_seg(c, l.o, add(l.o, mul(l.u, l.len))) <= window)
            .map(|(i, _)| i)
            .collect();

        self.grid.query(c, window, &mut self.scratch);
        let mut nearest = f64::INFINITY;
        let mut nearest_same = f64::INFINITY;
        let mut nearest_seen = f64::INFINITY;
        let mut foreign = 0.0f64;
        let mut samey = 0.0f64;
        for &pi in &self.scratch {
            let pl = &self.plots[pi as usize];
            let dd = dist(c, pl.pos);
            if dd < sep - 1e-6 {
                return None;
            }
            if dd < window {
                if pl.district == ctx.did {
                    samey += 1.0;
                    nearest_same = nearest_same.min(dd);
                } else {
                    foreign += 1.0;
                }
            }
            nearest = nearest.min(dd);
            // Rule A asks whether the new cell will share an edge with a
            // sibling's, and the nearest-neighbour-is-a-Delaunay-edge argument
            // it rests on only holds **inside a quarter**: a plot on the far
            // side of an avenue is not a Voronoi neighbour at all, because the
            // diagram is computed per quarter and the avenue is where the cells
            // stop. Counting one as "the nearest plot" makes rule A fail for a
            // plot whose cell is perfectly well attached to its own district.
            if near_lines
                .iter()
                .all(|&i| !self.crosses_desire(i, c, pl.pos))
            {
                nearest_seen = nearest_seen.min(dd);
            }
        }
        let nearest_visible = nearest_seen;
        // Rule A. `nearest` and `nearest_same` were measured over the same
        // window, so a sibling that wins inside it wins outright: nothing
        // outside the window can be nearer than something inside it.
        if ctx.touch && !crate::districts::touches_own(nearest_visible, nearest_same) {
            return None;
        }
        if !nearest.is_finite() {
            nearest = 0.0; // the first plot in the world
        }
        let p = self.params;
        let mut score =
            -p.w_hug * (nearest / sep) - p.w_slope * self.slope(c) + p.w_noise * self.height(c);
        // Line up along the city limit.
        let ad = self.alignment_distance(c, sep * AVENUE_REACH);
        if ad.is_finite() && ad < sep * AVENUE_REACH {
            score -= p.w_avenue * (ad / sep - AVENUE_OFFSET).abs();
        }
        // …and, on a desire line, along the line as well as across it. The
        // across-the-line term above is what the prototype had, and on its own
        // it buys nothing: two plots facing each other at ±sep/2 but at
        // different distances *along* the line have a perpendicular bisector
        // that is tilted, so the boundary they share is a wiggle. Rewarding the
        // along-line lattice is what makes a run of facing pairs share one
        // straight bisector.
        score += p.w_lattice * self.lattice_score(c, sep);
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
        Some(score)
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
        let g = &mut self.ground[did as usize];
        g.plots.push(id);
        g.sum = add(g.sum, pos);
        let c = mul(g.sum, 1.0 / g.plots.len() as f64);
        let mut r = 0.0f64;
        for &pi in &self.ground[did as usize].plots {
            r = r.max(dist(self.plots[pi as usize].pos, c));
        }
        self.ground[did as usize].radius = r;
        id
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
            1,
            AgeRamp::default(),
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
struct Candidate {
    /// The district asking for ground.
    did: u32,
    /// Its plot separation.
    sep: f64,
    /// Rule T's border keep-out, or zero.
    keep_out_d: f64,
    /// How far outside `allowed` a candidate may sit, or zero.
    fringe: f64,
    /// Rule A: the nearest plot of all must be a sibling.
    touch: bool,
    /// Whether the district has ground already.
    has_ground: bool,
    /// The district's centre of mass.
    dcent: Pt,
    /// The first anchor, used only before the district has ground.
    home: Pt,
    /// Whether the held-open lattice slots are binding on this rung of the
    /// ladder. Dropped once the search starts relaxing, so a reservation can
    /// never be the reason a plot ends up outside its own district.
    plan: bool,
}

/// Promote the partition's trimmed cuts to desire lines (Graft 2).
///
/// The one judgement call is the pitch, and it is made here rather than in
/// `territory` because it needs [`Params`]: see [`DesireLine`] for why it is the
/// **coarsest** separation of any district whose ground reaches the line.
///
/// A district reaches the line when one of its face's corners is inside the
/// band. The faces are convex and the cuts are their own edges, so a district
/// that runs along a cut always has two corners on it; the test is cheap and
/// errs toward including a district, which errs toward a legal lattice.
fn desire_lines(territory: &Territory, params: &Params, total: u32) -> Vec<DesireLine> {
    let mut out = Vec::with_capacity(territory.avenues.len());
    for &(a, b) in &territory.avenues {
        let len = dist(a, b);
        if len <= 1e-6 {
            continue;
        }
        let u = norm(sub(b, a));
        let n = [-u[1], u[0]];
        let mut pitch = params.sep_core;
        for node in &territory.nodes {
            if node.own_units == 0 || node.face.len() < 3 {
                continue;
            }
            let age = if node.own_oldest == u32::MAX {
                1.0
            } else {
                (f64::from(node.own_oldest) / f64::from(total.max(1))).clamp(0.0, 1.0)
            };
            let sep = params.sep_at(age);
            if sep <= pitch {
                continue;
            }
            let band = sep * DESIRE_REACH;
            if node.face.iter().any(|&v| dist_to_seg(v, a, b) <= band) {
                pitch = sep;
            }
        }
        // Quantised: the pitch sets a lattice phase, and a lattice phase is the
        // last place an unrounded `f64` should reach (PRD §7.4).
        let pitch = quantize_f64(pitch * LATTICE_PITCH);
        if pitch <= 1e-6 || len < pitch * 2.0 {
            continue; // too short to carry even one straight run
        }
        out.push(DesireLine {
            o: qp(a),
            u: qp(u),
            n: qp(n),
            len: quantize_f64(len),
            pitch,
        });
    }
    out
}

/// A spread of points inside a convex face: the centroid, the vertices pulled
/// inward, and the edge midpoints pulled inward.
fn face_probes(face: &[Pt]) -> Vec<Pt> {
    let c = crate::geom::centroid(face);
    let mut out = vec![qp(c)];
    let n = face.len();
    for i in 0..n {
        let a = face[i];
        let b = face[(i + 1) % n];
        out.push(qp(crate::geom::lerp(a, c, 0.30)));
        out.push(qp(crate::geom::lerp(mul(add(a, b), 0.5), c, 0.25)));
    }
    out
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
) -> Settlement {
    files.sort_by(|a, b| (a.growth_index, a.path.as_str()).cmp(&(b.growth_index, b.path.as_str())));
    let total = u32::try_from(files.len()).unwrap_or(u32::MAX);
    let mut s = Settlement::new(territory, params, total, ramp);
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
        let t = territory::build(
            &crate::districts::demands(&files, &params, &ramp),
            &|p| p.as_str().starts_with("vendor"),
            &terrain,
            params.plot_area_at(0.0),
            9,
        );
        grow(files, t, params, ramp)
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
    fn the_territory_constraint_actually_binds() {
        let s = settle(&sample_paths());
        assert_eq!(
            s.relaxed.detached, 0,
            "a plot was placed with no territory at all"
        );
        // Every plot is accounted for by exactly one rung of the ladder.
        assert_eq!(
            s.relaxed.clean
                + s.relaxed.nonadjacent
                + s.relaxed.fringe
                + s.relaxed.ancestor
                + s.relaxed.anywhere
                + s.relaxed.detached,
            s.plots.len(),
            "a plot was placed without being counted"
        );
        assert_eq!(
            s.relaxed.anywhere, 0,
            "a plot was placed with no relation to its own directory at all"
        );
    }

    #[test]
    fn plots_settle_inside_their_own_district() {
        let s = settle(&sample_paths());
        let mut outside = 0;
        for p in &s.plots {
            let face = &s.territory.nodes[p.district as usize].face;
            if face.len() >= 3 && !contains(face, p.pos) {
                outside += 1;
            }
        }
        assert!(
            outside <= s.relaxed.fringe + s.relaxed.ancestor + s.relaxed.anywhere,
            "{outside} plots left their district but only {} relaxations were recorded",
            s.relaxed.fringe + s.relaxed.ancestor + s.relaxed.anywhere
        );
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
        assert!(p.district_demand(10, 1.0) > p.district_demand(10, 0.0));
        // One file still needs one whole plot, plus its border band.
        assert!(p.district_demand(1, 0.0) > p.plot_area_at(0.0));
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
