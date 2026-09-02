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

use std::cmp::Reverse;

use polis_events::{LogicalPath, WallTime};

use crate::age::AgeRamp;
use crate::determinism::{combine_seeds, det_sin_cos, quantize_f64, SeededRng, TAU};
use crate::geom::{add, dist, len, mul, qp, sub, Pt};
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

// ---------------------------------------------------------------------------
// The founding quarters — why a town has several old cores and not one
// ---------------------------------------------------------------------------
//
// A settlement grown from **one** seed under [`Params::touch_max`] is a disc
// that expands outward in commit order. `sep` and `cap` ramp with age, so grain
// is a monotone function of *when*, and *when* is a monotone function of *how
// far out*: the two compose into a polar density field. It was measured on the
// junction set of three corpora — Pearson correlation between distance from the
// densest point and nearest-neighbour spacing +0.574 / +0.582 / +0.609, with
// median spacing doubling from the clot to the rim. That is the pie chart's
// axle, left standing after its crust was removed.
//
// The cure is not to flatten the ramp: PRD §7.1 *requires* the age structure,
// and a dense old core is the spec. The cure is to stop there being one centre
// for it to be graded about. Real towns are several villages that grew into each
// other along the roads between them, and the roads went where the ground let
// them.
//
// So the founding is explicit. The biggest packages are given a **site each**,
// chosen off the terrain rather than off a radius, and joined to what is already
// settled by a **causeway** — a chain of plots each inside the connectivity
// reach of the one before, routed along the contour. Every quarter then grows in
// place at its own package's grain. The old town is wherever the oldest package
// is, the civic square is with it, and the map's geometric centre is somewhere
// else entirely — which is what "the historic centre" means in a real town.
//
// Nothing here partitions the plane. It places a dozen points and a few dozen
// plots; every boundary on the finished map is still a Voronoi bisector and
// every district is still a connected part of the plot adjacency graph
// ([`crate::regions`]).
//
// # What it is worth, measured
//
// Correlation between nearest-neighbour junction spacing and distance from the
// densest point, and the ratio of median spacing in the outermost radial octile
// to the innermost — the two numbers the review reported — on the 5 000-file
// synthetic corpus, Django (7 014 files) and Neovim (3 890):
//
// | growth | synthetic | Django | Neovim |
// |---|---|---|---|
// | one seed at the origin | +0.582 / 2.05× | +0.609 / 1.68× | +0.574 / 2.05× |
// | quarters, no grain field | +0.459 / 1.39× | +0.402 / 1.32× | +0.072 / 1.27× |
// | **both, as shipped** | **+0.338 / 1.41×** | **+0.339 / 1.28×** | **+0.153 / 0.97×** |
//
// The quarters do most of it and the grain field ([`Settlement::sep_of`]) does
// the rest — on two corpora of three; on Neovim it costs 0.08, which is inside
// the run-to-run spread of this statistic and is reported rather than tuned
// away. Over the same three corpora the civic square moved from 5-14 % of the
// radius off the centre of the built ground to 40-44 %, and PRD §7.1's age
// gradient — median block area, newest third of the ground over oldest — stayed
// at 2.23× / 2.51× / 2.84× against 2.76× / 2.35× / 3.42×. The gradient is the
// spec; only its ordering by radius was the artefact.

/// Largest number of quarters a repository is founded with.
///
/// Every top-level package that is worth one gets one, and the cap is a bound on
/// the causeway network rather than a design choice: each quarter past the first
/// costs a road out to it, settled before there are files for it.
///
/// It is high on purpose. **A handful of quarters does not break the axle** —
/// measured at five on the synthetic corpus, the correlation moved from +0.582
/// to +0.570, because the eight packages that did *not* get a site all still
/// seeded off the civic square and rebuilt the clot around it. The number that
/// matters is not how many quarters there are but how many packages are left
/// growing out of one point.
const MAX_QUARTERS: usize = 14;

/// Smallest share of the repository a package must carry to found a quarter.
///
/// Below this the "quarter" is a hamlet whose causeway is longer than the ground
/// it reaches, which is the one shape that leaves a corridor of empty blocks
/// standing at the end of the growth.
const QUARTER_SHARE: f64 = 0.012;

/// Smallest absolute size, in files, for the same reason.
const QUARTER_FILES: u32 = 12;

/// Largest share of the repository one quarter may carry before it is split
/// into its own subdirectories.
///
/// The top level is not the right unit on a real repository: `django/` is 95 %
/// of Django and `src/` plus `runtime/` are all of Neovim, so a founding that
/// stops at the root's children founds nothing there. Anything over this share
/// is replaced by its own children, which is what makes `django/db`,
/// `django/contrib` and `django/forms` quarters of a town rather than three
/// rings of one.
const QUARTER_CEILING: f64 = 0.26;

/// How much bigger the town is than a jammed packing of its plots.
///
/// [`Settlement::expected_area`] adds up one plot's ground per plot, which is
/// what the town would occupy with no voids in it. A real settlement has parks,
/// bad ground and inlets — solidity measures 0.75-0.79 — so the radius the sites
/// are spread over is larger than the area sum says. Measured, the unslacked
/// estimate against the finished city's own radius: 35.1 against 55.7 on the
/// synthetic corpus, 59.9 against 83.3 on Django, 30.1 against 45.7 on Neovim —
/// ratios of 1.59, 1.39 and 1.52, and this is set at the bottom of that range.
/// Under-reaching costs a merged pair of quarters; over-reaching costs a
/// causeway that never fills in and stays standing as an empty corridor.
const TOWN_SLACK: f64 = 1.40;

/// Pitch of the candidate lattice the sites are chosen from, in units of the
/// town's expected radius.
///
/// A lattice and not a ring: a ring of candidates round the origin is a radial
/// construction and would put the quarters on a rosette, which is the artefact
/// with one more step in it. The lattice is jittered per site from the layout
/// seed, so the candidate set is a Poisson-ish scatter and the terrain decides
/// which of it wins.
const SITE_STEP: f64 = 0.19;

/// How far a lattice site jitters, as a fraction of the pitch.
const SITE_JITTER: f64 = 0.55;

/// How far the candidate lattice reaches from its own centre, in radii.
///
/// With [`SITE_OFFSET`] this bounds how far a quarter can be founded from the
/// civic square: `SITE_OFFSET + SITE_REACH` radii, so 1.12 here. **Keep the sum
/// close to one.** Well above it the quarters are founded outside the ground the
/// town will ever cover, the causeways between them never fill in, and the plan
/// comes apart into the shards the first M1 attempt was failed for — measured at
/// the sum 1.77: solidity 0.48, longest stroke 25 % of a diameter that had
/// itself grown by three quarters, and no through-street at all. Well below it
/// the quarters crush together and the axle comes back: at the sum 0.98 the
/// correlation on the synthetic corpus was +0.365 against +0.338 here.
const SITE_REACH: f64 = 0.72;

/// Where the candidate region sits, in radii along the growth bearing from the
/// civic square.
///
/// **This is what takes ROOT off the geometric centre.** The civic square is the
/// origin (`polis_layout::WORLD_ORIGIN`) and the oldest package grows round it;
/// every other quarter is sited in a region offset down the bearing the terrain
/// favours, so the built ground's centre of mass ends up half a radius away from
/// the old town. Measured, the root district moved from 5-14 % of the radius off
/// centre to 40-44 % — 41.5 % on the synthetic corpus, 39.8 % on Django, 44.1 %
/// on Neovim, against 12.2 %, 13.7 % and 5.2 % before.
const SITE_OFFSET: f64 = 0.40;

/// How far apart two quarters' centres must be, as a share of the sum of their
/// expected radii.
///
/// Under about 0.7 the cores merge into one clot and the axle comes back; over
/// about 1.1 the quarters never meet and the causeways stay standing as empty
/// corridors.
const QUARTER_GAP: f64 = 0.75;

/// Weight on a site being clear of the quarters already placed.
///
/// The term **saturates** at [`SITE_ELBOW`] quarter radii. A term that keeps
/// rewarding distance is a term that puts every site on the boundary of the
/// candidate region, which is a ring, which is the rosette again.
const SITE_SPREAD: f64 = 1.10;

/// Where the spread reward stops, in quarter radii.
const SITE_ELBOW: f64 = 3.0;

/// Weight on a quarter sitting near the other quarters of its own package.
///
/// See [`Settlement::found`]: a package split into several quarters has to come
/// back together as one connected region in [`crate::regions`], and it does that
/// far more easily when its quarters are neighbours. It is also the truth —
/// `django/db` and `django/forms` are two ends of one part of town.
const SITE_KIN: f64 = 4.50;

/// Weight on staying inside the town rather than out on its edge.
///
/// The counterweight to [`SITE_SPREAD`]: quarters that met are a town, quarters
/// that did not are an archipelago.
const SITE_PULL: f64 = 1.40;

/// Length of one causeway step, as a share of the connectivity reach.
///
/// Inside the reach with margin, so the invariant every plot after the first is
/// within [`Params::touch_max`] separations of an earlier one holds by
/// construction and not by rounding. Also inside `voronoi::PHANTOM_NEAR`'s
/// `1.02 · sep_rim` doubled, so no phantom can grow between two consecutive
/// causeway plots and open a hole in the map.
const CAUSEWAY_STRIDE: f64 = 0.88;

/// Angular offsets a causeway step is allowed to take, so the road bends round
/// the ground instead of ruling a line across it.
const CAUSEWAY_BEND: [f64; 5] = [-0.42, -0.21, 0.0, 0.21, 0.42];

/// Bearings sampled when asking which way the ground invites the town to grow.
const BEARING_SAMPLES: u32 = 24;

/// Coordinate scale the grain field is read at.
///
/// A wavelength of several quarters, so a whole neighbourhood agrees on its
/// grain and the change from one to the next is a district boundary rather than
/// a texture. Read much finer and neighbouring districts disagree, which is
/// noise; much coarser and the field is a single gradient across the town, which
/// is the artefact with a different centre.
const GRAIN_SCALE: f64 = 0.045;

/// How far the grain field may move a district's separation, either way.
///
/// The age ramp spans five to one ([`Params::sep_core`] to
/// [`Params::sep_rim`]); this is under half of one step of that, which is enough
/// to break the ordering by radius without touching the ordering by age.
///
/// Measured against the same growth with the field switched off — the middle row
/// of the table at the top of this module — it takes the correlation a further
/// 0.121 down on the synthetic corpus and 0.063 on Django, and puts 0.081 back
/// on Neovim. It is second order to the quarters and it is kept because two
/// corpora of three improve; the third is inside the spread. Above about 0.55 it
/// starts costing the thing it must not cost: at 0.55 the synthetic corpus's age
/// gradient fell to 1.79× and one Neovim package came apart.
const GRAIN_SWING: f64 = 0.45;

/// A founding quarter: where it is, how big it will get, and which package it
/// belongs to.
#[derive(Debug, Clone, Copy)]
struct Quarter {
    /// The site chosen for it.
    site: Pt,
    /// Radius of the ground its files will eventually cover.
    radius: f64,
    /// The top-level package, so quarters of one package stay together.
    package: u32,
}

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
    /// How many files the growth is about to be handed.
    ///
    /// The founding needs the town's *eventual* size before it has settled
    /// anything — the quarters have to be spread over the ground the town will
    /// occupy, not over the ground it occupies at step zero. It is an estimate
    /// and it is allowed to be: it sets a spacing, and the growth fills whatever
    /// it leaves.
    total_files: u32,
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
        Self {
            total_files,
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

    /// Plot separation in a district: its age, and the ground it stands on.
    ///
    /// # The second half is the other half of the axle
    ///
    /// Age alone makes grain a function of *when*, and a settlement grown under
    /// [`Params::touch_max`] makes *when* a function of *how far out* — the two
    /// compose into the polar density field this module's founding comment
    /// describes. Founding several quarters breaks most of that composition;
    /// what is left of it is that every quarter's newest ground is on the
    /// outside of the town, because the outside is where the room is.
    ///
    /// So the grain also follows the **ground**. A low-frequency field — read at
    /// [`GRAIN_SCALE`], a wavelength of a few quarters, and seeded apart from
    /// the relief so it is not the same signal that shapes the coast — moves a
    /// district's separation by up to [`GRAIN_SWING`] either way. Real density
    /// follows soil and water, not radius, and this is that in one line:
    /// somewhere on the rim is close-grained because the ground there is good,
    /// and somewhere in the middle is loose because it is not.
    ///
    /// It is read at the district's **own ground**, so it is one value per
    /// district at a time rather than a per-candidate field: the packing rule is
    /// a comparison between a candidate and its neighbours, and a separation
    /// that varied inside one district's search would make that comparison
    /// asymmetric.
    ///
    /// Clamped into the ramp's own range — `[sep_core, sep_rim]` — so the
    /// packing floor and the connectivity reach are exactly the two numbers they
    /// were before, and every invariant stated against them still holds.
    pub(crate) fn sep_of(&self, did: u32) -> f64 {
        let base = self.params.sep_at(self.district_age(did));
        let here = self.district_seat(did);
        (base * self.grain_at(here)).clamp(self.params.sep_core, self.params.sep_rim)
    }

    /// Where a district's grain is read: its own ground, or its nearest
    /// ancestor's, or the civic square.
    fn district_seat(&self, did: u32) -> Pt {
        let mut up = Some(did);
        while let Some(d) = up {
            let g = &self.ground[d as usize];
            if !g.plots.is_empty() {
                return g.centroid();
            }
            up = self.territory.nodes[d as usize].parent;
        }
        [0.0, 0.0]
    }

    /// The grain of the ground at a point, in `[1 - GRAIN_SWING, 1 + …]`.
    fn grain_at(&self, p: Pt) -> f64 {
        let n = crate::determinism::fbm2_f64(
            self.params.terrain_seed ^ 0x6772_6169_6E5F_0001,
            p[0] * GRAIN_SCALE,
            p[1] * GRAIN_SCALE,
            2,
        );
        1.0 + GRAIN_SWING * n.clamp(-1.0, 1.0)
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
    /// A plot with capacity zero. It is settled before any file, so it is the
    /// oldest ground in the city; nothing can move into it, so its cell has no
    /// buildings and reads as a square. Making it a *plot* rather than hunting
    /// for an empty block afterwards is what makes it reliably there.
    ///
    /// It is at the **origin**, and the origin is no longer the middle of the
    /// map. [`Settlement::found`] sites every other quarter down one bearing
    /// from here, so the civic square ends up where the oldest package is and
    /// the built ground's centre of mass ends up half a radius away — the
    /// historic centre rather than the geometric one, which is what PRD §8 asks
    /// for and what a town that grew in one direction actually looks like.
    fn reserve_civic(&mut self) {
        if !self.plots.is_empty() || self.territory.nodes.is_empty() {
            return;
        }
        let root = self.territory.root();
        self.settle_plot(root, [0.0, 0.0], 0);
        // Counted like any other placement, so the ladder's tallies add up to
        // the plot count exactly and a plot can never go unaccounted for.
        self.relaxed.clean += 1;
    }

    /// Ground one district's files will eventually need, in world units².
    fn expected_area(&self, files: u32, age: f64) -> f64 {
        if files == 0 {
            return 0.0;
        }
        let cap = f64::from(self.params.cap_at(age)).max(1.0);
        let plots = (f64::from(files) / cap).ceil().max(1.0);
        plots * self.params.plot_area_at(age)
    }

    /// Radius of the ground that much area covers, with the slack a real
    /// settlement leaves in it ([`TOWN_SLACK`]).
    fn expected_radius(&self, files: u32, age: f64) -> f64 {
        (self.expected_area(files, age) / std::f64::consts::PI).sqrt() * TOWN_SLACK
    }

    /// The town's expected radius.
    ///
    /// Summed **per district at its own grain**, not taken at the mean age: the
    /// separation ramps five to one across the ramp and its square ramps
    /// twenty-five to one, so a single-age estimate of a repository whose files
    /// are mostly recent is out by a factor of three. Measured against the
    /// finished cities, this lands within 15 % on all three corpora, which is
    /// all it has to do — it sets a spacing, and the growth fills whatever it
    /// leaves.
    fn town_reach(&self) -> f64 {
        let mut area = 0.0;
        for d in 0..self.territory.nodes.len() {
            let node = &self.territory.nodes[d];
            if node.own_files == 0 {
                continue;
            }
            let id = u32::try_from(d).expect("district count fits in u32");
            area += self.expected_area(node.own_files, self.district_age(id));
        }
        if area <= 0.0 {
            area = self.expected_area(self.total_files, 0.5);
        }
        (area / std::f64::consts::PI).sqrt() * TOWN_SLACK
    }

    /// The packages that get a quarter of their own, oldest first.
    ///
    /// A **top-level** package, because that is the unit PRD §9 colours and the
    /// unit `regions` keeps in one piece; and the founding district is the
    /// directory the package's oldest file is actually in, so the founding plot
    /// carries the package's real age rather than `u32::MAX` for a directory
    /// that holds only subdirectories.
    fn founding_packages(&self) -> Vec<(u32, u32)> {
        let root = self.territory.root();
        let Some(node) = self.territory.nodes.get(root as usize) else {
            return Vec::new();
        };
        let total = f64::from(self.total_files.max(1));
        let floor = QUARTER_FILES.max((total * QUARTER_SHARE).round() as u32);
        let ceiling = (total * QUARTER_CEILING).round() as u32;

        // Start at the top level, then **split whatever is too big to be one
        // quarter**. Top-level packages are the right unit for a repository like
        // the synthetic corpus, where a dozen of them share the tree; they are
        // the wrong unit for a real one, where `django/` is 95 % of the files
        // and `src/` plus `runtime/` are all of Neovim. Measured with no descent
        // at all: two sites on Django and two on Neovim, and the correlation
        // stayed at +0.56 and +0.60 — the founding did nothing, because nothing
        // was founded.
        //
        // A node that is split keeps its place in the list when it has files of
        // its own, so those files still have ground to seed from.
        let mut frontier: Vec<u32> = node
            .children
            .iter()
            .copied()
            .filter(|&c| self.territory.nodes[c as usize].subtree_files > 0)
            .collect();
        // A node that is split but kept — because it has files of its own —
        // stays in the list and must never be split twice, or the loop grows
        // nothing and never ends.
        let mut expanded: Vec<u32> = Vec::new();
        loop {
            if frontier.len() >= MAX_QUARTERS {
                break;
            }
            // The heaviest node over the ceiling that has somewhere to go.
            // Integer keys and tree indices only (PRD §7.4).
            let pick = frontier
                .iter()
                .enumerate()
                .filter(|&(_, &d)| {
                    let n = &self.territory.nodes[d as usize];
                    !expanded.contains(&d)
                        && n.subtree_files > ceiling
                        && n.children
                            .iter()
                            .filter(|&&c| self.territory.nodes[c as usize].subtree_files > 0)
                            .count()
                            >= 2
                })
                .max_by_key(|&(i, &d)| {
                    (
                        self.territory.nodes[d as usize].subtree_files,
                        Reverse(d),
                        Reverse(i),
                    )
                })
                .map(|(i, _)| i);
            let Some(i) = pick else {
                break;
            };
            let d = frontier[i];
            let kids: Vec<u32> = self.territory.nodes[d as usize]
                .children
                .iter()
                .copied()
                .filter(|&c| self.territory.nodes[c as usize].subtree_files > 0)
                .collect();
            expanded.push(d);
            if self.territory.nodes[d as usize].own_files > 0 {
                frontier.extend(kids);
            } else {
                frontier.remove(i);
                frontier.extend(kids);
            }
            frontier.sort_unstable();
            frontier.dedup();
        }

        let mut big: Vec<(u32, u32, u32)> = frontier
            .into_iter()
            .filter_map(|c| {
                let kid = &self.territory.nodes[c as usize];
                (kid.subtree_files >= floor).then_some((kid.subtree_files, kid.oldest, c))
            })
            .collect();
        // Biggest first, ties by age then by index — every key is an integer or
        // a tree position, so no float comparison decides which package founds a
        // quarter (PRD §7.4).
        big.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.cmp(&b.1))
                .then_with(|| a.2.cmp(&b.2))
        });
        big.truncate(MAX_QUARTERS);
        // …then oldest first, so the oldest quarter is the one that keeps the
        // civic square and the rest are sited around it.
        big.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.2.cmp(&b.2)));
        let mut seats: Vec<u32> = Vec::new();
        let mut out: Vec<(u32, u32)> = Vec::new();
        for (_, _, c) in big {
            // A node that was split and a child of it can name the same oldest
            // leaf; one site each would put two quarters on one piece of ground.
            let seat = if self.territory.nodes[c as usize].own_files > 0 {
                c
            } else {
                self.founding_district(c)
            };
            if seats.contains(&seat) {
                continue;
            }
            seats.push(seat);
            out.push((c, seat));
        }
        out
    }

    /// The directory inside `pkg` that holds its oldest file.
    ///
    /// Used for the **age** of the quarter's founding ground, not for its
    /// ownership: the plot itself belongs to `pkg`, because a nucleus planted on
    /// a leaf is ground only that leaf's own files can seed from — its four
    /// sibling directories walk up to a parent with no ground, fall through to
    /// the root, and found themselves back at the civic square. Measured on a
    /// fixture with two old subsystems, that put four fifths of each quarter
    /// back at the origin and left the correlation exactly where it started.
    ///
    /// Depth-first over `children`, which the territory orders by
    /// `(oldest, path)`, so this is a deterministic walk and never a search.
    fn founding_district(&self, pkg: u32) -> u32 {
        let mut best = pkg;
        let mut best_key = (self.territory.nodes[pkg as usize].own_oldest, pkg);
        let mut stack = vec![pkg];
        while let Some(d) = stack.pop() {
            let node = &self.territory.nodes[d as usize];
            if node.own_files > 0 && (node.own_oldest, d) < best_key {
                best_key = (node.own_oldest, d);
                best = d;
            }
            stack.extend(node.children.iter().copied());
        }
        best
    }

    /// How good a piece of ground is to found on: PRD §7.2's terrain, read with
    /// the same two weights the growth scores every other candidate with.
    fn ground_score(&self, c: Pt) -> f64 {
        let (h, gx, gy) = self.terrain.sample(c[0], c[1]);
        let relief = f64::from(self.terrain.relief()).max(1e-9);
        let slope = (gx * gx + gy * gy).sqrt();
        self.params.w_noise * (h / relief) - self.params.w_slope * slope
    }

    /// Which way the ground invites the town to grow, as a unit vector.
    ///
    /// One bearing, sampled once, from the terrain alone. It is the only radial
    /// quantity in the founding and it is a *direction*, not a gradient: the
    /// quarters are then chosen off a lattice laid down that way, so nothing on
    /// the map is graded by distance from anywhere.
    fn growth_bearing(&self, reach: f64) -> Pt {
        let mut best: Option<(f64, Pt)> = None;
        for k in 0..BEARING_SAMPLES {
            let ang = TAU * f64::from(k) / f64::from(BEARING_SAMPLES);
            let (sn, cs) = det_sin_cos(ang);
            let u = [cs, sn];
            // Averaged over the run out to the reach, so one lucky hilltop does
            // not decide where a town of five thousand files goes.
            let mut score = 0.0;
            for step in 1..=4 {
                let r = reach * f64::from(step) / 4.0;
                score += self.ground_score([u[0] * r, u[1] * r]);
            }
            let key = quantize_f64(score);
            if best.is_none_or(|(b, bu)| key > b || (key == b && (u[0], u[1]) < (bu[0], bu[1]))) {
                best = Some((key, u));
            }
        }
        best.map_or([1.0, 0.0], |(_, u)| u)
    }

    /// Found the town: the civic square, then one quarter per big package, each
    /// sited off the terrain and joined to the settlement by a causeway.
    ///
    /// Called once, before the first file. A repository with fewer than two
    /// qualifying packages is founded exactly as it was before — one seed at the
    /// origin — because a town with one quarter is a town with one quarter, and
    /// inventing a second one for a 90-file repository would be a lie about its
    /// history.
    fn found(&mut self) {
        self.reserve_civic();
        let packages = self.founding_packages();
        if packages.len() < 2 {
            return;
        }
        let reach = self.town_reach();
        if !(reach.is_finite() && reach > self.params.sep_rim * 2.0) {
            return;
        }
        let bearing = self.growth_bearing(reach);
        let centre = qp([
            bearing[0] * reach * SITE_OFFSET,
            bearing[1] * reach * SITE_OFFSET,
        ]);

        // A quarter's **kin**: the top-level package it belongs to. Quarters of
        // one package are drawn toward each other, so `django/db`,
        // `django/forms` and `django/contrib` are neighbouring quarters of one
        // district rather than three quarters scattered among strangers. It is
        // truer — a package really is one part of town — and it is also what
        // keeps `regions` able to make the package one connected region, which
        // is asserted absolutely and was seen to fail once when the sites were
        // scattered without it.
        let kin = crate::districts::package_of(&self.territory);
        let kin_of = |d: u32| kin.get(&d).copied().unwrap_or(0);

        // The oldest quarter keeps the civic square's ground: it is the old
        // town, and its own nucleus goes beside the square rather than out on
        // the lattice.
        let (first, first_seat) = packages[0];
        let first_age = self.district_age(first_seat);
        let first_born = self.territory.nodes[first_seat as usize].own_oldest;
        self.plant_quarter([0.0, 0.0], first, self.params.cap_at(first_age), first_born);
        let mut placed: Vec<Quarter> = vec![Quarter {
            site: [0.0, 0.0],
            radius: self.expected_radius(
                self.territory.nodes[first as usize].subtree_files,
                first_age,
            ),
            package: kin_of(first),
        }];

        for &(pkg, seat) in &packages[1..] {
            let age = self.district_age(seat);
            let born = self.territory.nodes[seat as usize].own_oldest;
            let radius =
                self.expected_radius(self.territory.nodes[pkg as usize].subtree_files, age);
            let package = kin_of(pkg);
            let Some(site) = self.quarter_site(centre, reach, radius, package, &placed) else {
                continue;
            };
            // Claimed whether or not it is reached, so the next package looks
            // somewhere else rather than at the same piece of ground again.
            placed.push(Quarter {
                site,
                radius,
                package,
            });
            self.lay_causeway(site, pkg, self.params.cap_at(age), born);
        }
    }

    /// The best unclaimed piece of ground for a quarter of this size.
    ///
    /// The candidate set is a jittered lattice over the region the town will
    /// occupy — not a ring, and not a sweep outward from anything. A site is
    /// rejected outright when it would sit inside a quarter already placed, and
    /// scored on the ground under it plus how far it is from its neighbours.
    fn quarter_site(
        &self,
        centre: Pt,
        reach: f64,
        radius: f64,
        package: u32,
        placed: &[Quarter],
    ) -> Option<Pt> {
        let step = (reach * SITE_STEP).max(self.params.sep_rim);
        let n = (SITE_REACH / SITE_STEP).ceil() as i32;
        let span = reach * SITE_REACH;
        let mut best: Option<(f64, Pt)> = None;
        for iy in -n..=n {
            for ix in -n..=n {
                let mut rng = SeededRng::for_seed(
                    combine_seeds(
                        self.params.terrain_seed ^ 0x5175_4152_5445_5200,
                        ((i64::from(ix) << 32) ^ i64::from(iy)) as u64,
                    ),
                    "quarter.site",
                );
                let jx = (rng.next_f64() - 0.5) * 2.0 * SITE_JITTER;
                let jy = (rng.next_f64() - 0.5) * 2.0 * SITE_JITTER;
                let c = qp([
                    centre[0] + (f64::from(ix) + jx) * step,
                    centre[1] + (f64::from(iy) + jy) * step,
                ]);
                if dist(c, centre) > span {
                    continue;
                }
                let mut nearest = f64::INFINITY;
                let mut nearest_kin = f64::INFINITY;
                let mut clash = false;
                for q in placed {
                    let d = dist(c, q.site);
                    if d < QUARTER_GAP * (q.radius + radius) {
                        clash = true;
                        break;
                    }
                    nearest = nearest.min(d);
                    if q.package == package {
                        nearest_kin = nearest_kin.min(d);
                    }
                }
                if clash {
                    continue;
                }
                let spread = if nearest.is_finite() {
                    (nearest / (radius * SITE_ELBOW).max(1e-9)).min(1.0)
                } else {
                    1.0
                };
                let kin = if nearest_kin.is_finite() {
                    1.0 - (nearest_kin / span.max(1e-9)).min(1.0)
                } else {
                    0.0
                };
                let pull = (dist(c, centre) / span.max(1e-9)).min(1.0);
                let score = quantize_f64(
                    self.ground_score(c) + SITE_SPREAD * spread + SITE_KIN * kin - SITE_PULL * pull,
                );
                let better = best.is_none_or(|(b, bp)| {
                    score > b || (score == b && (c[0], c[1]) < (bp[0], bp[1]))
                });
                if better {
                    best = Some((score, c));
                }
            }
        }
        best.map(|(_, c)| c)
    }

    /// Is this position legal ground for a plot of separation `sep`?
    ///
    /// The packing rule only. The connectivity rule is the causeway's business
    /// and is satisfied by how the chain is built, not by a test after the fact.
    fn legal_here(&self, c: Pt, sep: f64) -> bool {
        let mut ok = true;
        let plots = &self.plots;
        self.grid.scan_while(c, sep, |pi| {
            if dist(c, plots[pi as usize].pos) < sep - 1e-6 {
                ok = false;
                return false;
            }
            true
        });
        ok
    }

    /// Distance from `at` to the nearest settled plot, and where it is.
    fn nearest_plot(&self, at: Pt) -> Option<(f64, Pt)> {
        if self.plots.is_empty() {
            return None;
        }
        let mut r = self.params.sep_rim * 2.0;
        for _ in 0..14 {
            let mut best: Option<(f64, Pt)> = None;
            let plots = &self.plots;
            self.grid.scan(at, r, |pi| {
                let q = plots[pi as usize].pos;
                let d = quantize_f64(dist(at, q));
                if best.is_none_or(|(bd, bp)| d < bd || (d == bd && (q[0], q[1]) < (bp[0], bp[1])))
                {
                    best = Some((d, q));
                }
            });
            if best.is_some() {
                return best;
            }
            r *= 2.0;
        }
        None
    }

    /// Settle the road out to a new quarter, and the quarter's first plot at the
    /// end of it. `true` when the quarter was founded.
    ///
    /// One plot at a time, each inside the connectivity reach of the one before
    /// it, so the induction that makes `components = 1` — every plot after the
    /// first is within [`Params::touch_max`] separations of an earlier one —
    /// covers the causeway exactly as it covers the growth. The step bends onto
    /// the best of [`CAUSEWAY_BEND`]'s offsets, so the road follows the ground
    /// rather than ruling a line across it.
    ///
    /// The founding plot belongs to the **package**, not to the directory its
    /// age was read from, so every directory under the package seeds from it.
    ///
    /// The **last plot of the causeway is the founding plot**, and it has to be:
    /// an old package's separation is a fifth of the rim's, so its reach is a
    /// fifth too, and a quarter left one causeway stride short of its own site
    /// could never bud the rest of the way. A walk that cannot close founds
    /// nothing and says so.
    fn lay_causeway(&mut self, target: Pt, did: u32, cap: u32, born: u32) -> bool {
        let reach = self.params.sep_rim * self.params.touch_max;
        let stride = reach * CAUSEWAY_STRIDE;
        let sep = self.params.sep_rim;
        let road = self.territory.root();
        let road_cap = self.params.cap_at(1.0);
        let Some((start, _)) = self.nearest_plot(target) else {
            return false;
        };
        let mut budget = 4 + (start / stride).ceil().max(0.0) as usize * 2;
        loop {
            let Some((d, from)) = self.nearest_plot(target) else {
                return false;
            };
            if d <= reach {
                // The site is in reach of the town. The quarter's own plot goes
                // on it, or — when the growth has already taken that ground — on
                // the nearest legal position beside it.
                return self.plant_quarter(target, did, cap, born);
            }
            if budget == 0 {
                return false;
            }
            budget -= 1;
            let to = sub(target, from);
            let l = len(to);
            if l < 1e-9 {
                return false;
            }
            let dir = mul(to, 1.0 / l);
            // **The last step lands the quarter itself.** Between `reach` and
            // `reach + sep` there is a dead zone: too far for the site to be
            // connected, too near for another causeway plot to fit in front of
            // it. Walking into it is how a causeway got stuck 0.02 units outside
            // the reach on Django and abandoned a quarter of 2 581 files. So the
            // step that would land inside it is simply the founding plot.
            let closing = d - stride <= reach;
            let mut best: Option<(f64, Pt)> = None;
            for bend in CAUSEWAY_BEND {
                let (sn, cs) = det_sin_cos(bend);
                let u = [dir[0] * cs - dir[1] * sn, dir[0] * sn + dir[1] * cs];
                let c = qp([
                    from[0] + u[0] * stride.min(d),
                    from[1] + u[1] * stride.min(d),
                ]);
                // Never step away from the site, and never onto ground that is
                // already taken.
                if dist(c, target) >= d || !self.legal_here(c, sep) {
                    continue;
                }
                let score = quantize_f64(self.ground_score(c));
                let better = best.is_none_or(|(b, bp)| {
                    score > b || (score == b && (c[0], c[1]) < (bp[0], bp[1]))
                });
                if better {
                    best = Some((score, c));
                }
            }
            let Some((_, c)) = best else {
                return false;
            };
            if closing {
                self.settle_born(did, c, cap, born);
                self.relaxed.clean += 1;
                return true;
            }
            self.settle_born(road, c, road_cap, born);
            self.relaxed.clean += 1;
        }
    }

    /// Put the quarter's founding plot on its site, or as near it as the packing
    /// distance allows.
    ///
    /// The caller has already brought the settlement inside the connectivity
    /// reach of `site`, so every position tested here satisfies that rule too:
    /// they are all within one separation of the site.
    fn plant_quarter(&mut self, site: Pt, did: u32, cap: u32, born: u32) -> bool {
        let sep = self.params.sep_rim;
        if self.legal_here(site, sep) {
            self.settle_born(did, site, cap, born);
            self.relaxed.clean += 1;
            return true;
        }
        let mut best: Option<(f64, Pt)> = None;
        for k in 0..BAND_ANGLES {
            let ang = TAU * f64::from(k) / f64::from(BAND_ANGLES);
            let (sn, cs) = det_sin_cos(ang);
            let c = qp([site[0] + cs * sep * 1.05, site[1] + sn * sep * 1.05]);
            if !self.legal_here(c, sep) {
                continue;
            }
            let score = quantize_f64(self.ground_score(c));
            let better = best
                .is_none_or(|(b, bp)| score > b || (score == b && (c[0], c[1]) < (bp[0], bp[1])));
            if better {
                best = Some((score, c));
            }
        }
        let Some((_, c)) = best else {
            return false;
        };
        self.settle_born(did, c, cap, born);
        self.relaxed.clean += 1;
        true
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
    ///
    /// The plot's age is its district's, so the road mesh and the block sizes
    /// around it follow the same ramp its spacing did. Stored raw, `u32::MAX`
    /// included: the ramp, not a clamp, decides what an index it never saw reads
    /// as.
    fn settle_plot(&mut self, did: u32, pos: Pt, cap: u32) -> u32 {
        let birth = self.territory.nodes[did as usize].own_oldest;
        self.settle_born(did, pos, cap, birth)
    }

    /// [`Settlement::settle_plot`], with the age of the ground given rather than
    /// taken from the district.
    ///
    /// The one caller is the causeway. Its plots are the **root's** ground —
    /// they are the road out of town, not part of any quarter, and leaving them
    /// with the quarter would drag that quarter's whole search back along them
    /// to the old town — but their age is the age of the quarter they reach.
    /// Left to the root's own age they would read as the oldest ground in the
    /// city and the prune ramp would lay a fine mesh down an empty road.
    fn settle_born(&mut self, did: u32, pos: Pt, cap: u32, birth: u32) -> u32 {
        let id = u32::try_from(self.plots.len()).expect("plot count fits in u32");
        self.plots.push(Plot {
            pos,
            district: did,
            files: Vec::new(),
            cap,
            birth,
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
    // The founding runs once, before the first file: the quarters have to be
    // sited over the ground the town will *end up* occupying, which is not
    // knowable one file at a time. Everything after it is the same single growth
    // step the incremental path calls, so a city grown a file at a time and one
    // generated from scratch are still the same city (PRD §7.7).
    s.found();
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

    // -----------------------------------------------------------------------
    // The founding (PRD §7.1): several quarters, sited off the terrain
    // -----------------------------------------------------------------------

    /// A repository with **two** old subsystems and two later ones, each of
    /// which keeps growing after it is founded.
    ///
    /// Two old ones, because that is the case the founding exists for: PRD §7.1
    /// asks for an old town, and a repository that grew from two roots has two
    /// of them. And every package keeps receiving files — in new subdirectories,
    /// which is what a real one does — so each quarter carries its own age
    /// gradient instead of one flat age. A fixture where a package is uniformly
    /// old or uniformly new has a genuine single density centre and cannot tell
    /// the two growths apart.
    fn quartered_paths() -> Vec<String> {
        const BORN: [(&str, usize); 4] =
            [("alpha", 0), ("omega", 1), ("middle", 340), ("recent", 560)];
        let mut out = vec!["README.md".to_owned()];
        let mut count = [0usize; 4];
        for step in 0..880 {
            let k = step % 4;
            let (name, born) = BORN[k];
            if step < born {
                continue;
            }
            // A new subdirectory every dozen files, so the package's own
            // districts are founded across its whole life.
            out.push(format!("{name}/mod{:02}/f{}.rs", count[k] / 12, count[k]));
            count[k] += 1;
        }
        out
    }

    fn quartered_records() -> Vec<FileRec> {
        let owned = quartered_paths();
        let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
        corpus(&refs)
    }

    fn quartered() -> Settlement {
        settle_records(quartered_records())
    }

    /// The same corpus grown the way it was grown before the founding existed:
    /// one seed at the origin, everything budding outward from it.
    ///
    /// This is the control. It is not a copy of the pipeline — it is the
    /// pipeline with [`Settlement::found`] left out, which is exactly the change
    /// under test.
    fn unfounded() -> Settlement {
        let mut files = quartered_records();
        files.sort_by(|a, b| {
            (a.growth_index, a.path.as_str()).cmp(&(b.growth_index, b.path.as_str()))
        });
        let total = u32::try_from(files.len()).expect("small");
        let params = Params::for_file_count(files.len());
        let terrain = crate::terrain::TerrainField::generate(0x51, 100.0);
        let ramp = ramp_of(&files);
        let t = territory::build(&crate::districts::demands(&files), &|p| {
            p.as_str().starts_with("vendor")
        });
        let mut s = Settlement::new(t, params, terrain, ramp, total);
        for f in files {
            s.add_file(f);
        }
        s
    }

    /// The correlation between distance from the densest point and
    /// nearest-neighbour spacing — the statistic the fresh-eyes review measured
    /// on the renders, computed here on the ground itself.
    fn drain(s: &Settlement) -> f64 {
        let pts: Vec<Pt> = s.plots.iter().map(|p| p.pos).collect();
        let spacing: Vec<f64> = pts
            .iter()
            .map(|p| {
                pts.iter()
                    .filter(|q| *q != p)
                    .map(|q| dist(*p, *q))
                    .fold(f64::INFINITY, f64::min)
            })
            .collect();
        let mut order: Vec<usize> = (0..pts.len()).collect();
        order.sort_by(|&a, &b| spacing[a].total_cmp(&spacing[b]));
        let tight = &order[..(pts.len() / 10).max(3)];
        let acc = mul(
            tight.iter().fold([0.0, 0.0], |a, &i| add(a, pts[i])),
            1.0 / tight.len() as f64,
        );
        let radius: Vec<f64> = pts.iter().map(|p| dist(*p, acc)).collect();
        pearson(&radius, &spacing)
    }

    /// **The axle.** Spacing must be less of a function of distance-from-a-point
    /// than it was before the founding.
    ///
    /// Asserted against the control rather than against a constant, because the
    /// number a corpus can reach depends on the corpus — the fresh-eyes review
    /// measured +0.553 to +0.584 on three real cities and this fixture is 133
    /// files. What is asserted is that the mechanism does its job, and the
    /// control is this same pipeline with the mechanism taken out.
    #[test]
    fn the_founding_flattens_the_radial_density_field() {
        let before = drain(&unfounded());
        let after = drain(&quartered());
        assert!(
            after + 0.12 < before,
            "the founding moved the drain from {before:+.3} only to {after:+.3}"
        );
    }

    /// Two old subsystems, two old quarters — and they are somewhere else from
    /// each other.
    ///
    /// The density gradient is allowed, and required, to exist; what is not
    /// allowed is for it to have one centre.
    #[test]
    fn two_old_subsystems_get_two_old_quarters() {
        let s = quartered();
        let alpha = s.territory.get(&lp("alpha")).expect("alpha");
        let omega = s.territory.get(&lp("omega")).expect("omega");
        // The **core** of a package's quarter: the centroid of its oldest and
        // tightest ground, not of everything it owns. A package spreads as it
        // grows; what has to be in two places is the dense old part of it.
        let core = |pkg: u32| -> Pt {
            let mut mine: Vec<(u32, Pt)> = Vec::new();
            for plot in &s.plots {
                let mut up = Some(plot.district);
                while let Some(d) = up {
                    if d == pkg {
                        mine.push((plot.birth, plot.pos));
                        break;
                    }
                    up = s.territory.nodes[d as usize].parent;
                }
            }
            assert!(!mine.is_empty(), "a founding package settled no ground");
            mine.sort_by(|x, y| {
                x.0.cmp(&y.0)
                    .then_with(|| x.1[0].total_cmp(&y.1[0]))
                    .then_with(|| x.1[1].total_cmp(&y.1[1]))
            });
            mine.truncate((mine.len() / 4).max(1));
            let n = mine.len() as f64;
            mul(
                mine.iter().fold([0.0, 0.0], |acc, (_, p)| add(acc, *p)),
                1.0 / n,
            )
        };
        let a = core(alpha);
        let o = core(omega);
        let radius = s
            .plots
            .iter()
            .map(|p| dist(p.pos, s.town_centre()))
            .fold(0.0, f64::max);
        let apart = dist(a, o);
        assert!(
            apart > 0.30 * radius,
            "the two old quarters are {apart:.1} apart in a town of radius {radius:.1}"
        );
    }

    /// PRD §7.1's age structure has to survive the fix that killed the axle.
    ///
    /// The oldest ground must still be measurably finer-grained than the newest
    /// — this is the number the naive fix (flatten the ramp) trades away, and it
    /// is the spec.
    #[test]
    fn the_age_gradient_survives_the_founding() {
        let s = quartered();
        let mut by_age: Vec<(f64, f64)> = s
            .plots
            .iter()
            .map(|p| (s.ramp.at(p.birth), s.params.sep_at(s.ramp.at(p.birth))))
            .collect();
        by_age.sort_by(|a, b| a.0.total_cmp(&b.0));
        let q = by_age.len() / 4;
        assert!(q >= 2, "too few plots to quartile");
        let median = |v: &[(f64, f64)]| {
            let mut m: Vec<f64> = v.iter().map(|x| x.1).collect();
            m.sort_by(f64::total_cmp);
            m[m.len() / 2]
        };
        let oldest = median(&by_age[..q]);
        let newest = median(&by_age[by_age.len() - q..]);
        assert!(
            newest > oldest * 1.25,
            "the newest ground is only {newest:.3} to the oldest ground's {oldest:.3}"
        );
    }

    /// The civic square is at the origin, and the origin is **not** the middle
    /// of the map (PRD §8: the *historic* centre).
    #[test]
    fn the_civic_square_is_not_the_centre_of_the_map() {
        let s = quartered();
        assert_eq!(s.plots[0].cap, 0, "the first plot is not the civic square");
        assert_eq!(s.plots[0].pos, [0.0, 0.0]);
        let centre = s.town_centre();
        let radius = s
            .plots
            .iter()
            .map(|p| dist(p.pos, centre))
            .fold(0.0, f64::max);
        let off = dist([0.0, 0.0], centre);
        assert!(
            off > 0.10 * radius,
            "the civic square sits {:.1}% of the radius from the centre of the map",
            off / radius * 100.0
        );
    }

    /// The founding settles plots directly, outside the candidate loop, so the
    /// two hard rules have to be re-asserted against it: nothing overlaps, and
    /// nothing is founded across a gap.
    #[test]
    fn the_founding_keeps_both_hard_rules() {
        let s = quartered();
        let reach = s.params.sep_rim * s.params.touch_max;
        for (i, p) in s.plots.iter().enumerate() {
            for q in &s.plots[i + 1..] {
                assert!(
                    dist(p.pos, q.pos) >= s.params.sep_core - 1e-3,
                    "two plots are {} apart",
                    dist(p.pos, q.pos)
                );
            }
            if i == 0 {
                continue;
            }
            let nearest = s.plots[..i]
                .iter()
                .map(|q| dist(p.pos, q.pos))
                .fold(f64::INFINITY, f64::min);
            assert!(
                nearest <= reach + 1e-6,
                "plot {i} was founded {nearest} from the nearest ground, past {reach}"
            );
        }
    }

    /// The founding is a pure function of the repository, like everything else
    /// in this crate (PRD §7.4).
    #[test]
    fn the_founding_is_deterministic() {
        let a = quartered();
        let b = quartered();
        assert_eq!(a.plots.len(), b.plots.len());
        for (x, y) in a.plots.iter().zip(b.plots.iter()) {
            assert_eq!(x.pos, y.pos);
            assert_eq!(x.district, y.district);
            assert_eq!(x.birth, y.birth);
        }
        // And the input order cannot reach it: `grow` sorts, and the founding
        // reads the territory, which is sorted too.
        let mut shuffled = quartered_records();
        shuffled.reverse();
        shuffled.rotate_left(7);
        let c = settle_records(shuffled);
        assert_eq!(a.plots.len(), c.plots.len());
        for (x, y) in a.plots.iter().zip(c.plots.iter()) {
            assert_eq!(x.pos, y.pos);
        }
    }

    /// The grain field moves a district's separation without taking it outside
    /// the ramp: `[sep_core, sep_rim]` are the two numbers every other invariant
    /// in this module is stated against.
    #[test]
    fn the_grain_field_stays_inside_the_ramp() {
        let s = quartered();
        for d in 0..s.territory.nodes.len() {
            let id = u32::try_from(d).expect("fits");
            let sep = s.sep_of(id);
            assert!(
                sep <= s.params.sep_rim + 1e-9 && sep >= s.params.sep_core - 1e-9,
                "district {d} has separation {sep}"
            );
        }
        // And it is a real field, not a constant: two places disagree.
        let a = s.grain_at([0.0, 0.0]);
        let b = s.grain_at([40.0, -25.0]);
        assert!((a - b).abs() > 0.01, "the grain field is flat: {a} vs {b}");
    }

    /// A repository with one big package is founded exactly as it was before —
    /// one seed at the origin. Inventing a second quarter for it would be a lie
    /// about its history.
    #[test]
    fn one_package_is_still_one_town() {
        let paths: Vec<String> = (0..30).map(|i| format!("only/f{i}.rs")).collect();
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let s = settle(&refs);
        assert_eq!(s.founding_packages().len(), 1);
    }

    fn pearson(x: &[f64], y: &[f64]) -> f64 {
        let n = x.len() as f64;
        let mx = x.iter().sum::<f64>() / n;
        let my = y.iter().sum::<f64>() / n;
        let sxy: f64 = x.iter().zip(y).map(|(a, b)| (a - mx) * (b - my)).sum();
        let sxx: f64 = x.iter().map(|a| (a - mx) * (a - mx)).sum();
        let syy: f64 = y.iter().map(|b| (b - my) * (b - my)).sum();
        if sxx <= 0.0 || syy <= 0.0 {
            return 0.0;
        }
        sxy / (sxx * syy).sqrt()
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
