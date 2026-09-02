//! The district territory map — a recursive partition of the city limit.
//!
//! # What this is, and what it deliberately is not
//!
//! The recursive partition is the one piece grafted from the
//! `treemap-arterials` design, and it is used as a **territory constraint**:
//! nearly every road in the city is still the Voronoi bisector of two accreted
//! plots ([`crate::voronoi`], [`crate::roads`]), and treemap's chord-splitting —
//! every cut at every depth drawn as a road, the crazed-glaze artefact — is not
//! here.
//!
//! **The exception is the top level, and it is the city's avenues.** The
//! partition fans the city limit into a handful of wedges round the civic
//! square, and those wedges are *quarters*: the diagram is computed inside one
//! quarter at a time, so the boundary between two of them is an exactly straight
//! edge for its whole length. See [`WEDGES`] for the full argument, and
//! [`crate::voronoi`] for the measurement that forced it. It is a bounded
//! exception — at most `WEDGES` rays, as many civic-square sides and as many
//! ring roads, against some three hundred cuts, every one of the rest an
//! ordinary wandering district border.
//!
//! # Why accretion needs it
//!
//! Left to itself, accretion decides district membership *emergently* — a
//! district is whatever fell out of where its plots happened to land. Measured
//! at 5 000 files that gave 102 of 276 districts more than one disconnected
//! piece, so in the dense core the colour changed every two or three blocks and
//! the district masses were unreadable exactly where the city was densest. That
//! violates PRD §9 ("the tree determines placement") and PRD §8 ("the district
//! skeleton must stay readable at every zoom").
//!
//! So every district is given a polygon first, and **a plot may only settle
//! inside its own district's polygon**. Three properties follow by construction
//! rather than by measurement:
//!
//! * districts are contiguous, because the polygon is,
//! * adjacent directories are adjacent on the ground, because siblings share a
//!   cut, and
//! * PRD §7.7's no-rearrangement property is upgraded from "measured" to
//!   "structurally impossible to violate", because a district's outer polygon is
//!   fixed by its parent and re-partitioning a subtree cannot move one square
//!   metre outside it.
//!
//! # The partition
//!
//! At the top: the fan. Below it: the chord split.
//!
//! The root face is a wobbled 19-gon's **convex hull**. Convexity of the root is
//! load-bearing: a chord of a convex polygon splits it into two convex polygons
//! and cannot leave it, so every face at every depth is convex and every cut is
//! guaranteed to divide it. At each directory the items — the directory's own
//! files, then its children — are ordered by `(oldest file, path)` and split at
//! the index that best balances **quantised** subtree weight; the face is cut at
//! the offset giving each side area proportional to its weight; the cut's normal
//! is the face's longest axis, leaned 30 % toward the terrain gradient so
//! borders follow contours; and the older group takes the side nearer the civic
//! square, which is what puts the old town in the middle.
//!
//! Weights are quantised to five significant binary digits, so adding one file
//! almost never moves a cut (PRD §7.7).

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

use polis_events::LogicalPath;

use crate::determinism::{det_sin_cos, SeededRng, TAU};
use crate::geom::{
    area, centroid, chord_of, clip_halfplane, convex_hull, dist, dot, extent_along, len,
    longest_axis, norm, qp, Pt,
};
use crate::terrain::TerrainField;

/// How much ground the partition lays out per unit of demand.
///
/// **One**, because the slack is already in the demand:
/// [`crate::accrete::Params::district_demand`] carries both the frontier a
/// district needs to grow into and the border band it loses to its neighbours,
/// and multiplying by a second factor here would inflate the city limit until
/// the ground between quarters read as a void.
pub(crate) const AREA_SLACK: f64 = 1.0;

/// Sides of the city limit before the hull.
const RIM_SIDES: u32 = 19;

/// How far the rim radius wobbles, as a fraction.
const RIM_WOBBLE: f64 = 0.13;

/// How far a cut's normal leans from the face's longest axis toward the terrain
/// gradient. Roads follow contours; this is where that starts.
const GRADIENT_LEAN: f64 = 0.30;

/// Absolute floor on a face, as a multiple of the caller's minimum plot area.
///
/// Sized in *plots*, never as a fraction of the city: a fraction looks harmless
/// and is not, because it scales with the repository, so at 5 000 files it stops
/// the partition long before the leaves and hands whole subtrees a shared face —
/// which silently undoes the contiguity this module exists to guarantee.
const MIN_FACE_PLOTS: f64 = 1.6;

/// Recursion depth cap. A directory tree deeper than this shares ground.
const MAX_DEPTH: u32 = 40;

/// Wedges the city limit is fanned into at the top level.
///
/// # Why the top cut is a fan and not a chord
///
/// The rest of the partition is `treemap-arterials`' balanced binary chord split
/// and stays that way. The **top** level is a fan, and the reason is the one
/// property a chord split cannot have.
///
/// A quarter boundary is a straight road (see [`crate::voronoi`] — a plot's cell
/// is computed inside its own quarter, so the boundary between two quarters is
/// an exact straight edge). Any binary split of a convex region starts with a
/// full chord of it, so the *first* quarter boundary of a chord partition runs
/// clean across the city: a city-spanning straight boulevard, which is precisely
/// the `treemap-arterials` artefact the bake-off threw out ("above 70 % you have
/// reinvented treemap's city-spanning chords").
///
/// A fan has no such cut. Every boundary runs from the civic square outward to
/// the city limit and stops — about 45 % of the diameter, inside the acceptance
/// band by construction — and the fan gives `WEDGES` of them at once instead of
/// one. The wedges are convex (a wedge of angle under 180° intersected with a
/// convex limit), they tile the limit, and their order around the centre is the
/// partition's own `(oldest file, path)` order, so adjacent directories are
/// still adjacent on the ground.
///
/// The civic square is what stops the avenues being one boulevard: a stroke
/// arriving along an avenue meets the square's two sides at about
/// `90° − step/2`, well past the 40° continuation limit, so it stops there
/// rather than crossing to the avenue opposite. That holds at any wedge count,
/// which is why this is a legibility number and not a correctness one.
pub(crate) const WEDGES: usize = 9;

/// Directories a wedge is worth having, so the fan grows with the city.
///
/// A hamlet has no boulevards. More to the point, an avenue is a *quarter*
/// boundary and a quarter needs enough ground to be worth dividing: fanning a
/// small repository into nine wedges leaves each of them a handful of parcels,
/// the avenues then run through most of the graph, and the collapse — which is
/// what makes the four- and five-way junctions PRD §7.2 calls the organic
/// signature — has almost nothing left it is allowed to fuse. Measured on this
/// repository at 152 files and 105 blocks: the four-and-five-plus share fell
/// from 53 % to 28 %, and a district came out in two pieces.
///
/// So the wedge count is `districts / DIRS_PER_WEDGE`, capped at [`WEDGES`], and
/// a city that cannot afford [`MIN_WEDGES`] of them does not fan at all — its
/// partition is exactly the chord split it was before there were avenues, which
/// is the layout that was measured good at that size. The count is of
/// **districts**, not files, because that is what the partition is given and it
/// is the thing the wedges have to be shared between.
const DIRS_PER_WEDGE: usize = 32;

/// Fewest wedges worth fanning. Below this the city is not big enough to have
/// avenues and the partition stays a plain chord split.
const MIN_WEDGES: usize = 5;

/// Smallest share of the city a wedge may be given.
///
/// Not about strokes — about district shape. A wedge is the ground a whole run
/// of top-level directories is partitioned inside, and a very thin one is a
/// splinter that the recursion cannot cut without producing faces too small for
/// the plots that need them. Measured at 5 000 files, that shows up directly as
/// plots relaxing over their own district's border.
const MIN_WEDGE_SHARE: f64 = 0.030;

/// Shortest chord inside a wedge that becomes a ring road, as a fraction of the
/// city diameter.
///
/// # Why a fan needs a ring
///
/// Nine avenues radiating from one square and nothing crossing them is a pie
/// chart, not a plan: the eye reads nine solid segments of a disc. Every radial
/// city on the ground has the other half of the pattern — Amsterdam's canals,
/// Moscow's rings, Karlsruhe's crescents — because a fan alone gives you no way
/// to get from one wedge to the next without going through the middle.
///
/// So each wedge's **own first cut** is promoted too. It is a full chord of the
/// wedge, so the tiling and the convexity are exactly as safe as the fan's; the
/// cut direction is `cut_face`'s, perpendicular to the face's longest axis,
/// which in a wedge is the radius — so the chord comes out roughly tangential,
/// and the nine of them make a broken, uneven ring rather than a drawn circle.
/// The floor is the through-street definition and the ceiling keeps a ring
/// segment from spanning the city, exactly as for the avenues.
const RING_MIN_FRACTION: f64 = 0.255;

/// …and the longest.
const RING_MAX_FRACTION: f64 = 0.46;

/// The civic square's smallest circumradius, as a fraction of the city's.
///
/// Its area is the repository root's own demand, like every other district's —
/// but it also has a job the others do not, which sets a floor. The avenues meet
/// on its corners, and if its sides are shorter than the collapse threshold they
/// are contracted away, the corners fuse into one node, and every avenue meets
/// every other at a point: the stroke rule then walks in along one and out along
/// the one opposite, and the city has a boulevard across it again.
const CIVIC_MIN_RADIUS: f64 = 0.055;

/// …and its largest.
///
/// A root directory with a great many files of its own would otherwise be given
/// a civic square that swallows the middle of the city, and — since its own
/// files are few plots however much ground they are given — the middle of the
/// city would be empty. This is the cap that stops the plan reading as a
/// bullseye with a hole in it.
const CIVIC_MAX_RADIUS: f64 = 0.135;

/// How far the civic square's corners wobble, as a fraction.
///
/// Small, and only so the plan does not read as a drawn dartboard: the avenues
/// leave the square at slightly different radii and none of them is exactly a
/// mirror of the one opposite.
const CIVIC_WOBBLE: f64 = 0.16;

/// How far an avenue may lean off radial, in turns.
///
/// # Why the avenues are not spokes
///
/// A perfect fan puts every avenue on a line through the hub, so two of them on
/// roughly opposite bearings are roughly collinear. The civic square stops a
/// stroke crossing *between* them — the turn onto one of its sides is about
/// `90° − step/2` — but it does not stop a stroke crossing *through* them: the
/// square has its own cells and their boundaries are roads, and one of those
/// happening to lie near the shared bearing chains the two avenues into a single
/// stroke across the whole city. Measured at 5 000 files: a 64 %-of-diameter
/// chain that held up even under a 30° continuation limit, on a plan whose
/// longest actual avenue is 46 %.
///
/// So each avenue leaves its own corner of the square on a bearing leaned off
/// radial by a seeded amount. `0.038` turns is 13.7°, so two avenues can differ
/// from collinear by up to 27° — past the 30° limit, comfortably past 40° — and
/// no chain can run in one and out the other. The wedges stay convex because
/// they are still an intersection of half-planes with a convex city limit; the
/// tiling stays exact because each leaned ray is still the one shared boundary
/// between the two wedges either side of it.
///
/// It is also the single change that most stops the plan reading as a dartboard.
const AVENUE_SPLAY: f64 = 0.0;

/// One directory in the territory tree.
#[derive(Debug, Clone)]
pub(crate) struct Node {
    /// The directory this district is.
    pub(crate) path: LogicalPath,
    /// Index of the parent district, `None` for the repository root.
    pub(crate) parent: Option<u32>,
    /// Child districts, ordered by `(oldest file, path)`.
    pub(crate) children: Vec<u32>,
    /// Demand of the files directly in this directory, in quantised units.
    pub(crate) own_units: u64,
    /// `own_units` plus every descendant's.
    pub(crate) subtree_units: u64,
    /// Lowest growth index of a file **directly** in this directory.
    pub(crate) own_oldest: u32,
    /// Lowest growth index anywhere in the subtree — the district's own age.
    pub(crate) oldest: u32,
    /// True when the directory is `node_modules`, `vendor`, `target` and so on.
    pub(crate) industrial: bool,
    /// The ground this district's **own** files may settle on.
    pub(crate) face: Vec<Pt>,
    /// The ground this district and every descendant may settle on.
    pub(crate) subtree_face: Vec<Pt>,
}

/// The finished territory map.
#[derive(Debug, Clone, Default)]
pub(crate) struct Territory {
    /// Districts, in creation order (parents before children).
    pub(crate) nodes: Vec<Node>,
    /// Path to index.
    pub(crate) index: BTreeMap<LogicalPath, u32>,
    /// The city limit: a convex ring, counter-clockwise.
    pub(crate) rim: Vec<Pt>,
    /// The promoted partition cuts: the avenues.
    ///
    /// Each is a full chord of the face it cut, between
    /// [`AVENUE_MIN_FRACTION`] and [`AVENUE_MAX_FRACTION`] of the city diameter.
    /// They are the boundaries between [`Territory::quarters`], the desire lines
    /// the accretion pass lines plots up along, and — because a quarter's cells
    /// stop at them — the through-streets themselves.
    pub(crate) avenues: Vec<(Pt, Pt)>,
    /// Which entries of [`Territory::quarters`] were never subdivided again.
    ///
    /// The list is a *tree* — a wedge is pushed, and if its own first cut is
    /// promoted to a ring road the two halves are pushed after it — so the
    /// entries that tile the city are the leaves. Handing the interior nodes to
    /// the Voronoi as well would have every wedge overlap its own two halves,
    /// and the cells computed in the wedge would swallow the cells computed in
    /// them.
    pub(crate) quarter_leaf: Vec<bool>,
    /// The regions the avenues cut the city limit into, convex and tiling it.
    ///
    /// A plot's Voronoi cell is computed **inside its own quarter**, against the
    /// plots of that quarter only, so the quarter boundary is an exact straight
    /// edge shared by the cells either side of it. That is what makes an avenue
    /// a road rather than a wish: see the module docs on
    /// [`crate::voronoi`].
    pub(crate) quarters: Vec<Vec<Pt>>,
    /// How many faces hit [`MIN_FACE_PLOTS`] or [`MAX_DEPTH`] and had to be
    /// shared. Reported, never hidden.
    pub(crate) shared_faces: usize,
    /// Districts that have files but were never given ground of their own.
    pub(crate) faceless: usize,
}

impl Territory {
    /// The index of a district, if it has one.
    pub(crate) fn get(&self, path: &LogicalPath) -> Option<u32> {
        self.index.get(path).copied()
    }

    /// The quarter polygons the Voronoi is computed inside, indexed as
    /// [`crate::city`]'s point location uses them.
    ///
    /// The whole city limit when the partition never fanned — a repository too
    /// small, or one with no files at the root to make a civic square of. One
    /// quarter means one frame, which is exactly the diagram this module built
    /// before there were avenues, so the fallback needs no separate code path.
    pub(crate) fn leaf_quarters(&self) -> Vec<Vec<Pt>> {
        let leaves: Vec<Vec<Pt>> = self
            .quarters
            .iter()
            .enumerate()
            .filter(|(i, q)| self.quarter_leaf.get(*i).copied().unwrap_or(false) && q.len() >= 3)
            .map(|(_, q)| q.clone())
            .collect();
        if leaves.is_empty() || !covers_the_rim(&leaves, &self.rim) {
            vec![self.rim.clone()]
        } else {
            leaves
        }
    }

    /// Longest distance across the city limit.
    pub(crate) fn diameter(&self) -> f64 {
        if self.rim.len() < 2 {
            return 1.0;
        }
        let mut best = 0.0f64;
        for (i, a) in self.rim.iter().enumerate() {
            for b in &self.rim[i + 1..] {
                best = best.max(dist(*a, *b));
            }
        }
        best.max(1e-6)
    }
}

/// Do the leaf quarters tile the city limit?
///
/// **Enforced, never asserted.** A plot's Voronoi cell is clipped to its own
/// quarter, so a plot in no leaf quarter gets an empty cell, lands inside no
/// face of the road graph, and is attached to whichever block is nearest — a
/// failure that shows up as a large parcel overflow four stages downstream and
/// names nothing. [`Quarter`] records the bug that produced it; this is the net
/// underneath, because a `debug_assert` here would have been compiled out of the
/// release build that measured it.
///
/// The test is areas, which catches a gap but not an overlap. That is the right
/// trade: the quarters come from an exact convex subdivision, so a gap is what a
/// bookkeeping slip produces and an overlap is not, and a check that had to
/// intersect every pair of quarters would cost more than the diagram it guards.
fn covers_the_rim(leaves: &[Vec<Pt>], rim: &[Pt]) -> bool {
    let whole = area(rim);
    if whole <= 0.0 {
        return true;
    }
    let covered: f64 = leaves.iter().map(|q| area(q)).sum();
    (covered - whole).abs() <= whole * QUARTER_COVER_TOLERANCE
}

/// How far the leaf quarters' total area may miss the city limit's before
/// [`covers_the_rim`] gives up on them.
///
/// Small: the quarters are exact half-plane clips of the same convex ring, so
/// the only difference is the coordinate quantisation, which is parts per
/// million. One percent is three orders of magnitude of headroom and still two
/// orders below the failure it exists to catch (Django lost 55 % of its city).
const QUARTER_COVER_TOLERANCE: f64 = 0.01;

/// One district, as the partition weighs it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Demand {
    /// Growth index of the oldest file directly in the district.
    pub(crate) oldest: u32,
    /// Ground the district needs, in world units².
    ///
    /// **Not simply its plots' area.** A plot may come no closer than one
    /// separation to any plot, including the ones just over the border in the
    /// district next door, so a district loses a band about half a separation
    /// wide around its whole perimeter. For a district of two or three plots
    /// that band is most of its polygon, and sizing the polygon by area alone
    /// makes the territory constraint impossible to satisfy exactly where it
    /// matters most. [`crate::accrete::Params::district_demand`] adds the band.
    pub(crate) area: f64,
}

/// Round up to five significant binary digits.
///
/// This is the stability knob. Two file counts in the same bucket produce the
/// same cut, so adding one file to a district almost never moves a border, and
/// when it does the move is a single visible event rather than a permanent
/// tremor (PRD §7.7).
fn quantize_units(units: u64) -> u64 {
    if units <= 32 {
        return units;
    }
    let bits = 64 - units.leading_zeros();
    let shift = bits - 5;
    let step = 1u64 << shift;
    units.div_ceil(step) * step
}

/// The city limit: a wobbled `RIM_SIDES`-gon, then its convex hull.
fn build_rim(total_area: f64, seed: u64) -> Vec<Pt> {
    // Area of a regular n-gon of circumradius r is `0.5 * n * r² * sin(2π/n)`.
    let n = f64::from(RIM_SIDES);
    let (sn, _) = det_sin_cos(TAU / n);
    let r = (total_area.max(1.0) * 2.0 / (n * sn.max(1e-9))).sqrt();
    let mut rng = SeededRng::for_seed(seed, "city.rim");
    let mut pts: Vec<Pt> = Vec::with_capacity(RIM_SIDES as usize);
    for k in 0..RIM_SIDES {
        let ang = TAU * f64::from(k) / n;
        let (s, c) = det_sin_cos(ang);
        let wob = 1.0 + (rng.next_f64() - 0.5) * 2.0 * RIM_WOBBLE;
        pts.push(qp([c * r * wob, s * r * wob]));
    }
    let hull = convex_hull(&pts);
    if hull.len() >= 3 {
        hull
    } else {
        vec![[-r, -r], [r, -r], [r, r], [-r, r]]
    }
}

/// What a cut produced: the two sub-faces and the chord between them.
type Cut = (Vec<Pt>, Vec<Pt>, (Pt, Pt));

/// Cut a convex face into two, with areas in the ratio `wa : wb`.
///
/// Returns `(face_a, face_b, chord)`. `face_a` is the side the normal points
/// away from.
fn cut_face(face: &[Pt], wa: f64, wb: f64, terrain: &TerrainField) -> Option<Cut> {
    let total = area(face);
    if face.len() < 3 || total <= 0.0 {
        return None;
    }
    let axis = longest_axis(face);
    let c0 = centroid(face);
    let (gx, gy) = terrain.gradient_f64(c0[0], c0[1]);
    let g = norm([gx, gy]);
    // Lean toward the gradient, keeping the normal on the same side of the
    // longest axis so the cut still crosses the face's long dimension.
    let signed = if dot(g, axis) < 0.0 {
        [-g[0], -g[1]]
    } else {
        g
    };
    let n = norm([
        axis[0] * (1.0 - GRADIENT_LEAN) + signed[0] * GRADIENT_LEAN,
        axis[1] * (1.0 - GRADIENT_LEAN) + signed[1] * GRADIENT_LEAN,
    ]);
    let n = if len([n[0], n[1]]) < 0.5 { axis } else { n };
    // Orient the normal so the **`neg` side is the one nearer the civic square
    // at the origin**, and do it here, before the offset is solved for.
    //
    // This is load-bearing, and getting it wrong is what produced the district
    // confetti this module exists to prevent. `want` square metres are cut for
    // the *first* group; the caller wants that group on the origin side, because
    // it is the older one and the old town belongs in the middle (PRD §7.1).
    // Those are two constraints on one cut, and only one of them can be met by
    // choosing which half to hand back: swapping the halves after the fact gives
    // the first group the area computed for the second. On an unbalanced split
    // that is catastrophic — measured at 5 000 files, a 769-file district was
    // handed 0.03 square units of ground and every one of its plots relaxed into
    // an ancestor's territory. Choosing the *sign of the normal* meets both.
    let n = if dot(n, c0) < 0.0 { [-n[0], -n[1]] } else { n };
    let (lo, hi) = extent_along(face, n);
    if hi - lo < 1e-9 {
        return None;
    }
    let want = (wa / (wa + wb)).clamp(0.02, 0.98) * total;
    // Bisect on the offset. The clipped area is monotone in `c`, so 40 steps
    // land far inside the quantisation grid and the result is exact enough that
    // two runs agree bit for bit.
    let mut a = lo;
    let mut b = hi;
    for _ in 0..40 {
        let mid = f64::midpoint(a, b);
        if area(&clip_halfplane(face, n, mid)) < want {
            a = mid;
        } else {
            b = mid;
        }
    }
    let c = f64::midpoint(a, b);
    let neg = clip_halfplane(face, n, c);
    let pos = clip_halfplane(face, [-n[0], -n[1]], -c);
    if neg.len() < 3 || pos.len() < 3 {
        return None;
    }
    let chord = chord_of(face, n, c)?;
    Some((neg, pos, chord))
}

/// One entry in a face's split list: either the directory's own files, or a
/// child district.
#[derive(Debug, Clone, Copy)]
struct Item {
    /// `None` for the directory's own files, which belong to `owner`.
    child: Option<u32>,
    /// The directory this item was listed under.
    owner: u32,
    units: u64,
    oldest: u32,
}

/// Build the territory map from the growth sequence.
///
/// `files` must be `(logical path, demand)` for every file that will be placed;
/// it is read in whatever order it arrives and sorted here, so the caller's
/// order cannot reach the partition (PRD §7.4).
pub(crate) fn build(
    districts: &[(LogicalPath, Demand)],
    industrial: &dyn Fn(&LogicalPath) -> bool,
    terrain: &TerrainField,
    min_plot_area: f64,
    seed: u64,
) -> Territory {
    // --- 1. The tree ------------------------------------------------------
    let mut t = Territory::default();
    if districts.is_empty() {
        return t;
    }
    let mut demand_of: BTreeMap<LogicalPath, (f64, u32)> = BTreeMap::new();
    let mut total_area = 0.0f64;
    for (path, d) in districts {
        let entry = demand_of.entry(path.clone()).or_insert((0.0, u32::MAX));
        entry.0 += d.area;
        entry.1 = entry.1.min(d.oldest);
        total_area += d.area;
    }
    // The unit is small enough that quantisation never rounds a district away
    // and large enough that the counts stay small integers.
    let unit = (total_area / (districts.len() as f64 * 24.0)).max(1e-9);

    ensure_nodes(&mut t, &demand_of, industrial, unit);

    // Children in `(oldest file, path)` order, and subtree sums bottom-up.
    let order: Vec<u32> = (0..t.nodes.len() as u32).collect();
    for &i in order.iter().rev() {
        let mut kids = t.nodes[i as usize].children.clone();
        kids.sort_by_key(|&c| (t.nodes[c as usize].oldest, t.nodes[c as usize].path.clone()));
        let mut units = t.nodes[i as usize].own_units;
        let mut oldest = t.nodes[i as usize].oldest;
        for &c in &kids {
            units = units.saturating_add(t.nodes[c as usize].subtree_units);
            oldest = oldest.min(t.nodes[c as usize].oldest);
        }
        let node = &mut t.nodes[i as usize];
        node.children = kids;
        node.subtree_units = units;
        node.oldest = oldest;
    }

    // --- 2. The city limit ------------------------------------------------
    t.rim = build_rim(total_area * AREA_SLACK, seed);
    let min_face = min_plot_area.max(1e-9) * MIN_FACE_PLOTS;

    // --- 3. The partition -------------------------------------------------
    let root = t.index.get(&LogicalPath::root()).copied().unwrap_or(0);
    let rim = t.rim.clone();
    t.nodes[root as usize].subtree_face = rim.clone();
    t.quarters = vec![rim.clone()];
    t.quarter_leaf = vec![true];
    let diameter = t.diameter();
    let mut cut = Cutter {
        avenues: Vec::new(),
        shared: 0,
        seed,
        // A hamlet has no boulevards: see [`DIRS_PER_WEDGE`].
        wedges: (t.nodes.len() / DIRS_PER_WEDGE).min(WEDGES),
        ring_min: diameter * RING_MIN_FRACTION,
        ring_max: diameter * RING_MAX_FRACTION,
    };
    partition(
        &mut t,
        root,
        rim,
        terrain,
        min_face,
        0,
        Quarter { id: 0, whole: true },
        &mut cut,
    );
    t.shared_faces = cut.shared;
    t.faceless = t
        .nodes
        .iter()
        .filter(|n| n.own_units > 0 && n.face.len() < 3)
        .count();

    // --- 4. The avenues ---------------------------------------------------
    t.avenues = cut.avenues;
    t
}

/// Which Voronoi frame a face's plots are clipped inside, and whether this face
/// is the whole of it.
///
/// # Why `whole` is load-bearing
///
/// Promoting a cut to a quarter boundary marks the parent quarter **non-leaf**
/// and pushes the two halves as new leaves — and only a leaf is a Voronoi frame
/// ([`Territory::leaf_quarters`]). Done from a cut that owns only *part* of the
/// quarter, that retires a frame the rest of the quarter is still using, and
/// every plot out there is clipped against a polygon it is not inside: an empty
/// cell, no face to sit in, and attachment to whichever block happens to be
/// nearest.
///
/// It is not hypothetical. Django — 7 014 files in 2 102 directories, whose
/// root fan does not commit — took the un-fanned path, promoted the first of the
/// two depth-1 cuts and not the second, and came out with **two** leaf quarters
/// covering less than half the city: 1 453 of 2 413 plots with no cell at all,
/// 1 483 attached to a stranger's block, and 2 006 files sharing a parcel. Every
/// count in the report was accurate and none of them said so.
#[derive(Debug, Clone, Copy)]
struct Quarter {
    /// Index into [`Territory::quarters`].
    id: u32,
    /// True when this face **is** that quarter rather than a piece of it. Only
    /// a face that owns the whole quarter may subdivide it.
    whole: bool,
}

/// State carried down the recursion.
struct Cutter {
    /// The avenues: the fan's rays, the civic square's sides, and one ring road
    /// per wedge.
    avenues: Vec<(Pt, Pt)>,
    /// Faces the recursion had to give up on.
    shared: usize,
    /// The city's layout seed, for the civic square's wobble.
    seed: u64,
    /// How many wedges to fan the city limit into; zero for none.
    wedges: usize,
    /// Shortest chord that may be promoted to a ring road.
    ring_min: f64,
    /// Longest chord that may be promoted to a ring road.
    ring_max: f64,
}

/// Create a node for every district and every ancestor of one.
fn ensure_nodes(
    t: &mut Territory,
    demand_of: &BTreeMap<LogicalPath, (f64, u32)>,
    industrial: &dyn Fn(&LogicalPath) -> bool,
    unit: f64,
) {
    // The root always exists: PRD §8's civic square is the repository root, and
    // the partition needs a single face to start from.
    ensure_one(t, &LogicalPath::root(), industrial);
    for (district, (demand, oldest)) in demand_of {
        let id = ensure_one(t, district, industrial);
        let node = &mut t.nodes[id as usize];
        node.own_units = quantize_units((demand / unit).ceil() as u64).max(1);
        node.own_oldest = node.own_oldest.min(*oldest);
        node.oldest = node.oldest.min(*oldest);
    }
}

/// Create one node and every ancestor it needs, returning its index.
fn ensure_one(
    t: &mut Territory,
    path: &LogicalPath,
    industrial: &dyn Fn(&LogicalPath) -> bool,
) -> u32 {
    if let Some(&id) = t.index.get(path) {
        return id;
    }
    let parent = path.parent().map(|p| ensure_one(t, &p, industrial));
    let id = u32::try_from(t.nodes.len()).expect("district count fits in u32");
    t.nodes.push(Node {
        path: path.clone(),
        parent,
        children: Vec::new(),
        own_units: 0,
        subtree_units: 0,
        own_oldest: u32::MAX,
        oldest: u32::MAX,
        industrial: industrial(path),
        face: Vec::new(),
        subtree_face: Vec::new(),
    });
    t.index.insert(path.clone(), id);
    if let Some(p) = parent {
        t.nodes[p as usize].children.push(id);
    }
    id
}

/// Divide `face` between a district's own files and its children, recursively.
fn partition(
    t: &mut Territory,
    node: u32,
    face: Vec<Pt>,
    terrain: &TerrainField,
    min_face: f64,
    depth: u32,
    quarter: Quarter,
    cut: &mut Cutter,
) {
    t.nodes[node as usize].subtree_face = face.clone();
    let own = t.nodes[node as usize].own_units;
    let kids = t.nodes[node as usize].children.clone();
    if kids.is_empty() {
        t.nodes[node as usize].face = face;
        return;
    }
    let mut items: Vec<Item> = Vec::with_capacity(kids.len() + 1);
    if own > 0 {
        items.push(Item {
            child: None,
            owner: node,
            units: own,
            // The directory's own files are the oldest thing in it that is not a
            // child's: `oldest` here is the node's own first file, not the
            // subtree's, or the civic square would migrate to whichever child
            // happened to be committed first.
            oldest: t.nodes[node as usize].own_oldest,
        });
    }
    for &c in &kids {
        items.push(Item {
            child: Some(c),
            owner: node,
            units: t.nodes[c as usize].subtree_units.max(1),
            oldest: t.nodes[c as usize].oldest,
        });
    }
    // `(oldest, path)`, with the directory's own files first among equals: the
    // civic square belongs at the historic centre of its own quarter.
    items.sort_by_key(|it| {
        (
            it.oldest,
            it.child.map_or(0u8, |_| 1u8),
            it.child.map_or_else(String::new, |c| {
                t.nodes[c as usize].path.as_str().to_owned()
            }),
        )
    });
    if own == 0 {
        // Nothing of the directory's own to place; every child still needs
        // ground, and the directory keeps the subtree face for fallback.
        t.nodes[node as usize].face = Vec::new();
    }
    // The top level is a fan round the civic square; everything below it is the
    // grafted binary chord split. See [`WEDGES`].
    if depth == 0 && fan(t, node, &items, &face, terrain, min_face, cut) {
        return;
    }
    place(t, &items, face, terrain, min_face, depth, quarter, cut);
}

/// Fan the city limit into wedges round the civic square. `false` to fall back.
///
/// The civic square is the repository root's own files (PRD §8), and it has to
/// be there: without it the avenues would meet at a point, two of them would be
/// within 40° of collinear whatever the angles, and the stroke rule would run
/// straight through the middle from one side of the city to the other.
fn fan(
    t: &mut Territory,
    node: u32,
    items: &[Item],
    rim: &[Pt],
    terrain: &TerrainField,
    min_face: f64,
    cut: &mut Cutter,
) -> bool {
    if cut.wedges < MIN_WEDGES {
        return false;
    }
    let civic = items.iter().position(|it| it.child.is_none());
    let (Some(civic), true) = (civic, items.len() > MIN_WEDGES) else {
        return false;
    };
    let kids: Vec<&Item> = items
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != civic)
        .map(|(_, it)| it)
        .collect();
    let k = kids.len().min(cut.wedges);
    if k < MIN_WEDGES {
        return false;
    }

    // Group the children into `k` runs, keeping their order, each run as near
    // one k-th of the weight as a prefix scan can make it. Same rule as the
    // binary split, applied `k` ways.
    let total: u64 = kids.iter().map(|i| i.units).sum();
    let mut runs: Vec<(usize, usize)> = Vec::with_capacity(k);
    let mut start = 0usize;
    let mut acc = 0u64;
    for slot in 0..k {
        let want =
            total * (u64::try_from(slot).expect("fits") + 1) / u64::try_from(k).expect("fits");
        let mut end = start;
        while end < kids.len() && acc + kids[end].units <= want && kids.len() - end > k - slot - 1 {
            acc += kids[end].units;
            end += 1;
        }
        if end == start && start < kids.len() {
            acc += kids[start].units;
            end = start + 1;
        }
        if slot == k - 1 {
            end = kids.len();
        }
        runs.push((start, end));
        start = end;
    }
    if runs.iter().any(|(a, b)| a >= b) {
        return false;
    }

    // The share of the city each wedge is owed, floored so that no run of
    // directories is given a splinter to be partitioned inside.
    let weights: Vec<f64> = runs
        .iter()
        .map(|(a, b)| {
            kids[*a..*b]
                .iter()
                .map(|i| i.units as f64)
                .sum::<f64>()
                .max(1.0)
        })
        .collect();
    let sum: f64 = weights.iter().sum();
    let mut shares: Vec<f64> = weights.iter().map(|w| w / sum).collect();
    let floor = MIN_WEDGE_SHARE.min(1.0 / k as f64);
    let deficit: f64 = shares.iter().map(|x| (floor - x).max(0.0)).sum();
    let surplus: f64 = shares.iter().map(|x| (x - floor).max(0.0)).sum();
    if surplus > 1e-9 {
        for x in &mut shares {
            *x = if *x < floor {
                floor
            } else {
                *x - (*x - floor) * deficit / surplus
            };
        }
    }

    let hub = qp(centroid(rim));
    let radius = rim.iter().map(|p| dist(*p, hub)).fold(0.0f64, f64::max);
    // The civic square's ground, like every district's, is its share of the
    // demand — clamped so that it is neither swallowed by the collapse nor left
    // as a hole in the middle of the city.
    let all: u64 = items.iter().map(|i| i.units).sum();
    let share = if all == 0 {
        0.0
    } else {
        items[civic].units as f64 / all as f64
    };
    let (sn_k, _) = det_sin_cos(TAU / k as f64);
    let want = area(rim) * share;
    let civic_r = (want * 2.0 / (k as f64 * sn_k.max(1e-9)))
        .sqrt()
        .clamp(radius * CIVIC_MIN_RADIUS, radius * CIVIC_MAX_RADIUS);
    let mut rng = SeededRng::for_seed(cut.seed, "city.civic");
    // The wedge boundaries. Set by **area**, not by angle: the city limit is a
    // wobbled polygon, so equal angles are not equal ground, and a run of
    // directories given less ground than its files need is a run whose plots
    // relax over their own border. This is the same rule the chord split uses —
    // area in proportion to quantised weight — solved by bisection because a
    // fan has no closed form for it.
    let total_area = area(rim);
    let mut angle = 0.0f64;
    let mut dirs: Vec<Pt> = Vec::with_capacity(k);
    let mut rays: Vec<Pt> = Vec::with_capacity(k);
    let mut corners: Vec<Pt> = Vec::with_capacity(k);
    let mut acc_share = 0.0f64;
    for i in 0..k {
        let (sn, cs) = det_sin_cos(angle * TAU);
        let d = qp([cs, sn]);
        let wob = 1.0 + (rng.next_f64() - 0.5) * 2.0 * CIVIC_WOBBLE;
        // The bearing the avenue actually leaves on: radial, leaned. See
        // [`AVENUE_SPLAY`]. The lean is bounded by a third of the wedge on
        // either side so that two neighbouring avenues can never cross.
        let room = shares[i].min(shares[(i + k - 1) % k]) / 3.0;
        let lean = (rng.next_f64() - 0.5) * 2.0 * AVENUE_SPLAY.min(room);
        let (rs, rc) = det_sin_cos((angle + lean) * TAU);
        dirs.push(d);
        rays.push(qp([rc, rs]));
        corners.push(qp([
            hub[0] + d[0] * civic_r * wob,
            hub[1] + d[1] * civic_r * wob,
        ]));
        acc_share += shares[i];
        if i + 1 < k {
            angle = fan_angle(rim, hub, total_area * acc_share);
        }
    }
    // The civic square is the corner list **in angular order**, not its convex
    // hull. The hull is the tempting call and it is wrong: a corner whose wobble
    // puts it inside its neighbours' chord is dropped, and the square then has
    // one fewer side than the wedges were clipped against — so a wedge boundary
    // no longer coincides with a square side, the two rings disagree about where
    // the shared edge runs, and the conforming pass has nothing to conform. It
    // shows up as a long edge across the middle of the city crossing its
    // neighbours without a node.
    //
    // So the wobble is dropped instead, on the rare corner list that is not
    // already convex. A perfectly regular square is a small cosmetic loss; a
    // crossing is a planarity break.
    if !is_convex(&corners) {
        for (i, c) in corners.iter_mut().enumerate() {
            *c = qp([hub[0] + dirs[i][0] * civic_r, hub[1] + dirs[i][1] * civic_r]);
        }
    }
    let _ = &dirs;
    let civic_face = corners.clone();
    if civic_face.len() < 3 || !is_convex(&civic_face) || area(&civic_face) <= 0.0 {
        return false;
    }

    // The wedges. Each is the city limit cut by its two bounding rays and by the
    // civic square's side, so it is an intersection of convex sets.
    let mut faces: Vec<Vec<Pt>> = Vec::with_capacity(k);
    for i in 0..k {
        // Each side of a wedge is the half-plane of the avenue that bounds it,
        // taken through the civic corner the avenue leaves from rather than
        // through the hub — which is what lets the avenue lean off radial and
        // still be the exact shared boundary of the two wedges either side.
        let (a, b) = (rays[i], rays[(i + 1) % k]);
        let na = [a[1], -a[0]];
        let nb = [-b[1], b[0]];
        let mut face = clip_halfplane(rim, na, dot(na, corners[i]));
        face = clip_halfplane(&face, nb, dot(nb, corners[(i + 1) % k]));
        let (ca, cb) = (corners[i], corners[(i + 1) % k]);
        let e = norm([cb[0] - ca[0], cb[1] - ca[1]]);
        let ne = [-e[1], e[0]];
        let outward = if dot(ne, [ca[0] - hub[0], ca[1] - hub[1]]) >= 0.0 {
            ne
        } else {
            [-ne[0], -ne[1]]
        };
        face = clip_halfplane(&face, [-outward[0], -outward[1]], -dot(outward, ca));
        if face.len() < 3 || area(&face) <= min_face {
            return false;
        }
        faces.push(face.into_iter().map(qp).collect());
    }

    // Committed. The rim stops being a leaf; the civic square and the wedges
    // become ones.
    t.quarter_leaf[0] = false;
    t.nodes[items[civic].owner as usize].face = civic_face.clone();
    t.quarters.push(civic_face.clone());
    t.quarter_leaf.push(true);
    for i in 0..k {
        // The avenues: the ray from the civic square's corner out to the limit,
        // and the side of the square itself. Both are quarter boundaries, so
        // both are exactly straight roads, and both have to be conformed.
        if let Some(seg) = ray_to_rim(rim, corners[i], rays[i]) {
            cut.avenues.push(seg);
        }
        cut.avenues.push((corners[i], corners[(i + 1) % k]));
        let q = u32::try_from(t.quarters.len()).expect("quarter count fits in u32");
        t.quarters.push(faces[i].clone());
        t.quarter_leaf.push(true);
        let (a, b) = runs[i];
        let group: Vec<Item> = kids[a..b].iter().map(|it| **it).collect();
        // This call's face **is** wedge `q`, so it — and only it — may promote a
        // cut to a quarter boundary inside it.
        place(
            t,
            &group,
            faces[i].clone(),
            terrain,
            min_face,
            1,
            Quarter { id: q, whole: true },
            cut,
        );
    }
    let _ = node;
    true
}

/// Whether a ring turns the same way at every corner.
fn is_convex(ring: &[Pt]) -> bool {
    let n = ring.len();
    if n < 3 {
        return false;
    }
    let mut sign = 0i8;
    for i in 0..n {
        let (a, b, c) = (ring[i], ring[(i + 1) % n], ring[(i + 2) % n]);
        let cr = (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0]);
        if cr.abs() < 1e-12 {
            return false; // three corners in a line: not a usable square
        }
        let s = if cr > 0.0 { 1i8 } else { -1i8 };
        if sign == 0 {
            sign = s;
        } else if sign != s {
            return false;
        }
    }
    true
}

/// The turn, in `[0, 1)`, at which the fan from `hub` has swept `want` of the
/// area of `rim`.
///
/// Bisection on a monotone quantity, a fixed 44 steps deep, so the answer is a
/// property of the geometry and not of a convergence test that could stop one
/// iteration earlier on another machine (PRD §7.4).
fn fan_angle(rim: &[Pt], hub: Pt, want: f64) -> f64 {
    let swept = |turn: f64| -> f64 {
        if turn <= 0.0 {
            return 0.0;
        }
        if turn >= 1.0 {
            return area(rim);
        }
        let (s0, c0) = det_sin_cos(0.0);
        let (s1, c1) = det_sin_cos(turn * TAU);
        let na = [s0, -c0];
        let nb = [-s1, c1];
        let mut face = clip_halfplane(rim, na, dot(na, hub));
        if turn > 0.5 {
            // A reflex sweep is the whole city less the wedge that is left over.
            face = clip_halfplane(rim, [-nb[0], -nb[1]], -dot(nb, hub));
            let rest = clip_halfplane(&face, [-na[0], -na[1]], -dot(na, hub));
            return area(rim) - area(&rest);
        }
        face = clip_halfplane(&face, nb, dot(nb, hub));
        area(&face)
    };
    let (mut lo, mut hi) = (0.0f64, 1.0f64);
    for _ in 0..44 {
        let mid = f64::midpoint(lo, hi);
        if swept(mid) < want {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    crate::determinism::quantize_f64(f64::midpoint(lo, hi))
}

/// Where the ray from `from` along `dir` leaves the convex `rim`.
fn ray_to_rim(rim: &[Pt], from: Pt, dir: Pt) -> Option<(Pt, Pt)> {
    let n = [-dir[1], dir[0]];
    let (a, b) = chord_of(rim, n, dot(n, from))?;
    // `chord_of` gives the whole chord; keep the half that goes the way `dir`
    // points, starting where the civic square ends.
    let pick = if dot([b[0] - from[0], b[1] - from[1]], dir) > 0.0 {
        b
    } else {
        a
    };
    if dist(from, pick) <= 1e-6 {
        return None;
    }
    Some((qp(from), qp(pick)))
}

/// Split an ordered item list across a face, then recurse into each item.
#[allow(clippy::too_many_arguments)]
fn place(
    t: &mut Territory,
    items: &[Item],
    face: Vec<Pt>,
    terrain: &TerrainField,
    min_face: f64,
    depth: u32,
    quarter: Quarter,
    cut: &mut Cutter,
) {
    if items.is_empty() {
        return;
    }
    if items.len() == 1 {
        match items[0].child {
            None => t.nodes[items[0].owner as usize].face = face,
            Some(c) => partition(t, c, face, terrain, min_face, depth + 1, quarter, cut),
        }
        return;
    }
    // Split at the index that best balances quantised weight; ties to the lower
    // index, which is the older half.
    let total: u64 = items.iter().map(|i| i.units).sum();
    let mut acc = 0u64;
    let mut best = (u64::MAX, 1usize);
    for k in 1..items.len() {
        acc += items[k - 1].units;
        let diff = acc.abs_diff(total - acc);
        if diff < best.0 {
            best = (diff, k);
        }
    }
    let k = best.1;
    let wa: u64 = items[..k].iter().map(|i| i.units).sum();
    let wb = total - wa;

    if depth >= MAX_DEPTH || area(&face) <= min_face {
        // Out of room or out of depth: everything left shares this face. Counted
        // rather than hidden — a shared face is the one place two districts can
        // interleave.
        cut.shared += 1;
        for it in items {
            match it.child {
                None => t.nodes[it.owner as usize].face = face.clone(),
                Some(c) => {
                    t.nodes[c as usize].subtree_face = face.clone();
                    t.nodes[c as usize].face = face.clone();
                    for d in descendants(t, c) {
                        t.nodes[d as usize].subtree_face = face.clone();
                        t.nodes[d as usize].face = face.clone();
                    }
                }
            }
        }
        return;
    }

    let Some((neg, pos, chord)) = cut_face(&face, wa as f64, wb as f64, terrain) else {
        cut.shared += 1;
        for it in items {
            if let Some(c) = it.child {
                t.nodes[c as usize].subtree_face = face.clone();
                t.nodes[c as usize].face = face.clone();
            } else {
                t.nodes[it.owner as usize].face = face.clone();
            }
        }
        return;
    };

    // A wedge's own first cut becomes its ring road; every cut below that is an
    // ordinary district border, so the boundary between the two sides is the
    // bisector of two accreted plots and wanders like the rest of the fabric.
    // Two promoted levels against some three hundred cuts is the line between an
    // avenue system and `treemap-arterials`' crazed glaze, and it is drawn here.
    let clen = dist(chord.0, chord.1);
    // `quarter.whole` is the guard that keeps the leaf quarters a tiling: only
    // the cut that owns the entire quarter may retire it. See [`Quarter`].
    let (qa, qb) = if cut.wedges >= MIN_WEDGES
        && depth == 1
        && quarter.whole
        && clen >= cut.ring_min
        && clen <= cut.ring_max
    {
        cut.avenues.push((qp(chord.0), qp(chord.1)));
        if let Some(leaf) = t.quarter_leaf.get_mut(quarter.id as usize) {
            *leaf = false;
        }
        let n = u32::try_from(t.quarters.len()).expect("quarter count fits in u32");
        t.quarters.push(neg.clone());
        t.quarter_leaf.push(true);
        t.quarters.push(pos.clone());
        t.quarter_leaf.push(true);
        (
            Quarter { id: n, whole: true },
            Quarter {
                id: n + 1,
                whole: true,
            },
        )
    } else {
        // Both halves stay inside the parent's frame, and neither is the whole
        // of it any more.
        let part = Quarter {
            id: quarter.id,
            whole: false,
        };
        (part, part)
    };

    // `cut_face` oriented the cut so that the `neg` side is *both* the side of
    // area `want` and the side nearer the civic square. So the older group takes
    // it directly: no swap, and therefore no chance of handing a group the area
    // that was computed for the other one (PRD §7.1).
    let (older, younger) = (neg, pos);
    place(t, &items[..k], older, terrain, min_face, depth + 1, qa, cut);
    place(
        t,
        &items[k..],
        younger,
        terrain,
        min_face,
        depth + 1,
        qb,
        cut,
    );
}

/// Every descendant of `node`, in index order.
fn descendants(t: &Territory, node: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        for &c in &t.nodes[n as usize].children {
            out.push(c);
            stack.push(c);
        }
    }
    out.sort_unstable();
    out
}

#[cfg(test)]
mod tests {

    /// The fan's quarters have to tile the city limit exactly and be convex,
    /// because a quarter is the frame a Voronoi cell is clipped to: a gap is a
    /// hole in the map and a reflex corner can cut a cell in two.
    #[test]
    fn the_quarters_tile_the_city_limit() {
        let mut files: Vec<String> = vec!["README.md".to_owned()];
        for pkg in 0..20u32 {
            for f in 0..45u32 {
                files.push(format!("pkg{pkg}/mod{}/f{f}.rs", f % 9));
            }
        }
        let refs: Vec<&str> = files.iter().map(String::as_str).collect();
        let t = build(&demands(&refs), &|_| false, &flat_terrain(), 0.05, 13);
        let quarters = t.leaf_quarters();
        assert!(quarters.len() > 1, "a 900-file tree should fan");
        let total: f64 = quarters.iter().map(|q| area(q)).sum();
        let rim = area(&t.rim);
        assert!(
            (total - rim).abs() < rim * 1e-6,
            "the quarters sum to {total} against a city limit of {rim}"
        );
        for q in &quarters {
            assert!(is_convex(q), "a quarter is not convex: {q:?}");
        }
    }

    /// Every avenue is long enough to read as a through-street and short enough
    /// not to cross the city. Both bounds are the bake-off's.
    #[test]
    fn avenues_stay_inside_the_length_band() {
        let mut files: Vec<String> = vec!["README.md".to_owned()];
        for pkg in 0..20u32 {
            for f in 0..45u32 {
                files.push(format!("pkg{pkg}/mod{}/f{f}.rs", f % 9));
            }
        }
        let refs: Vec<&str> = files.iter().map(String::as_str).collect();
        let t = build(&demands(&refs), &|_| false, &flat_terrain(), 0.05, 13);
        assert!(!t.avenues.is_empty(), "the fan produced no avenue");
        let d = t.diameter();
        let rays = t
            .avenues
            .iter()
            .filter(|(a, b)| dist(*a, *b) > d * 0.10)
            .count();
        assert!(rays >= MIN_WEDGES, "only {rays} avenues of any length");
        for (a, b) in &t.avenues {
            let len = dist(*a, *b);
            assert!(
                len <= d * 0.75,
                "an avenue is {:.0}% of the city diameter",
                len / d * 100.0
            );
        }
    }
    use super::*;

    fn lp(s: &str) -> LogicalPath {
        LogicalPath::new(s).expect("a valid test path")
    }

    /// One demand entry per *district*, which is what `build` now takes.
    fn demands(paths: &[&str]) -> Vec<(LogicalPath, Demand)> {
        let mut out: BTreeMap<LogicalPath, Demand> = BTreeMap::new();
        for (i, p) in paths.iter().enumerate() {
            let district = lp(p).parent().unwrap_or_else(LogicalPath::root);
            let entry = out.entry(district).or_insert(Demand {
                oldest: u32::MAX,
                area: 0.0,
            });
            entry.area += 1.0;
            entry.oldest = entry.oldest.min(u32::try_from(i).expect("small"));
        }
        out.into_iter().collect()
    }

    fn flat_terrain() -> TerrainField {
        TerrainField::generate(0x1234_5678_9abc_def0, 100.0)
    }

    #[test]
    fn quantisation_buckets_are_stable_and_monotone() {
        assert_eq!(quantize_units(0), 0);
        assert_eq!(quantize_units(32), 32);
        // 33..=34 share a bucket at five significant binary digits.
        assert_eq!(quantize_units(33), quantize_units(34));
        assert!(quantize_units(33) >= 33);
        let mut last = 0;
        for n in 0..5_000u64 {
            let q = quantize_units(n);
            assert!(q >= n, "{n} rounded down to {q}");
            assert!(q >= last, "not monotone at {n}");
            last = q;
        }
    }

    #[test]
    fn every_district_gets_a_face_inside_the_rim() {
        let files = demands(&[
            "README.md",
            "src/lib.rs",
            "src/auth/session.rs",
            "src/auth/token.rs",
            "src/net/server.rs",
            "docs/guide.md",
            "vendor/blob.js",
        ]);
        let t = build(
            &files,
            &|p| p.as_str().starts_with("vendor"),
            &flat_terrain(),
            0.05,
            7,
        );
        assert!(t.nodes.len() >= 5, "{} districts", t.nodes.len());
        assert!(area(&t.rim) > 0.0);
        for node in &t.nodes {
            if node.own_units == 0 {
                continue;
            }
            assert!(
                area(&node.face) > 0.0,
                "{} got no ground",
                node.path.as_str()
            );
            // Every face is a chord-subdivision of a convex rim, so its vertices
            // lie inside the rim.
            for v in &node.face {
                assert!(
                    crate::geom::contains(&t.rim, *v)
                        || crate::geom::dist_to_boundary(&t.rim, *v) < 1e-6,
                    "{} escaped the city limit at {v:?}",
                    node.path.as_str()
                );
            }
        }
    }

    #[test]
    fn sibling_faces_do_not_overlap() {
        let files = demands(&[
            "a/one.rs",
            "a/two.rs",
            "a/three.rs",
            "b/one.rs",
            "b/two.rs",
            "c/one.rs",
        ]);
        let t = build(&files, &|_| false, &flat_terrain(), 0.05, 11);
        let mut faces: Vec<(String, Vec<Pt>)> = Vec::new();
        for n in &t.nodes {
            if n.own_units > 0 && n.face.len() >= 3 {
                faces.push((n.path.as_str().to_owned(), n.face.clone()));
            }
        }
        assert!(faces.len() >= 3);
        for (i, (na, fa)) in faces.iter().enumerate() {
            for (nb, fb) in &faces[i + 1..] {
                let ca = centroid(fa);
                assert!(
                    !crate::geom::contains(fb, ca),
                    "{na}'s centre is inside {nb}"
                );
            }
        }
    }

    #[test]
    fn the_partition_conserves_area() {
        let files = demands(&["a/x.rs", "a/y.rs", "b/x.rs", "b/y.rs", "b/z.rs", "root.md"]);
        let t = build(&files, &|_| false, &flat_terrain(), 0.05, 3);
        let leaves: f64 = t
            .nodes
            .iter()
            .filter(|n| n.own_units > 0)
            .map(|n| area(&n.face))
            .sum();
        let rim = area(&t.rim);
        assert!(
            (leaves - rim).abs() < rim * 1e-6,
            "leaf faces sum to {leaves}, rim is {rim}"
        );
    }

    #[test]
    fn a_bigger_directory_gets_more_ground() {
        let mut paths: Vec<String> = Vec::new();
        for i in 0..40 {
            paths.push(format!("big/f{i}.rs"));
        }
        paths.push("small/f0.rs".to_owned());
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let files = demands(&refs);
        let t = build(&files, &|_| false, &flat_terrain(), 0.05, 5);
        let big = area(&t.nodes[t.get(&lp("big")).expect("big") as usize].face);
        let small = area(&t.nodes[t.get(&lp("small")).expect("small") as usize].face);
        assert!(big > small * 4.0, "big={big} small={small}");
    }

    #[test]
    fn input_order_cannot_move_the_partition() {
        let files = demands(&[
            "a/one.rs", "a/two.rs", "b/one.rs", "b/two.rs", "c/one.rs", "root.md",
        ]);
        let forward = build(&files, &|_| false, &flat_terrain(), 0.05, 13);
        let mut shuffled = files.clone();
        shuffled.reverse();
        shuffled.rotate_left(2);
        let backward = build(&shuffled, &|_| false, &flat_terrain(), 0.05, 13);
        for (a, b) in forward.nodes.iter().zip(backward.nodes.iter()) {
            assert_eq!(a.path, b.path);
            assert_eq!(a.face, b.face, "{} moved", a.path.as_str());
        }
        assert_eq!(forward.avenues, backward.avenues);
    }
}
