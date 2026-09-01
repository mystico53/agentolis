//! Step 2 — road growth by space colonization (PRD §7.2).
//!
//! > Scatter attraction points weighted by where files need to be reachable.
//! > Grow segments toward unclaimed points, consuming points within a kill
//! > radius. Segments follow the terrain gradient where the slope exceeds a
//! > threshold.
//!
//! # The organic signature
//!
//! > when a new segment's endpoint lands within `snap_radius` of an existing
//! > intersection, snap to it rather than creating a new node. Those irregular
//! > four- and five-way junctions are what the eye reads as "grown." **Without
//! > snapping you get a tree, and trees read as artificial.**
//!
//! Snapping is therefore not an optimisation and not optional: it is the single
//! rule that makes the whole look work. It lives on the graph itself —
//! [`RoadGraph::add_node_snapped`] and [`RoadGraph::snap_tolerance`] — so that
//! growth code cannot accidentally bypass it and so that an incremental step
//! snaps with exactly the tolerance the initial generation used. **Never push
//! onto [`RoadGraph::nodes`] directly.**
//!
//! [`RoadGrowth`] is the one place in the crate that pushes directly, through a
//! single private `push_node`, and it does so only after its own spatial index
//! has answered the snap query that [`RoadGraph::snap_target`] would have
//! answered by scanning every node. That index reproduces `snap_target`'s
//! semantics exactly — nearest within tolerance, ties to the lowest node index,
//! same [`Point::distance_squared`] — and
//! `snap_index_agrees_with_the_graphs_own_scan` and
//! `indexed_growth_matches_the_naive_snapping_reference` assert it, the second
//! by growing the same city both ways and comparing the graphs field by field.
//! The index exists because PRD §13.1 budgets a cold start of 3 s for a
//! 5 000-file repo and an `O(n²)` snap scan spends most of it.
//!
//! # Growth order is the history
//!
//! Attractors are consumed in the order they are supplied, and PRD §7.1 supplies
//! them in **git growth order**. That is the whole mechanism behind "files added
//! in the repo's first year form the old town": the early attractors get a
//! sparse graph and produce tangled, irregular junctions, and the late ones grow
//! into an already-dense network and look planned. Shuffling the attractor list
//! — or parallelising the growth loop — destroys the effect and the determinism
//! at once.
//!
//! # Sequential, not simultaneous — and why that is the point
//!
//! Classic space colonization (Runions et al.) resolves *every* attractor in
//! lockstep: each iteration finds the nearest node for all attractors at once
//! and grows every influenced node one step. That formulation is
//! order-independent, which sounds like a determinism win and is in fact fatal
//! here, twice over:
//!
//! * Order-independence means git history cannot reach the geometry, and PRD
//!   §7.1's old town is exactly the claim that it does.
//! * Lockstep growth is not **prefix-stable**: adding one attractor perturbs
//!   every iteration after it, so an incremental step would have to regenerate
//!   the world — the thing PRD §7.4 says it must not do.
//!
//! So attractors are resolved **one at a time, in growth order**. Each grows a
//! branch from the node currently nearest it, snapping at every step. The result
//! is prefix-stable by construction: `grow(A ++ B)` equals `grow(A)` followed by
//! `grow_incremental(B)`, which is what `incremental_growth_equals_full_regrowth`
//! asserts over every split of a real attractor list. The colonization character
//! survives — branches reach out of the network toward unclaimed points and stop
//! when a point is served — because each attractor's turn begins by asking
//! whether some node already lies within the kill radius.
//!
//! # Planarity
//!
//! > Blocks are the closed loops (faces) in the resulting planar road graph.
//!
//! Face extraction is undefined on a graph with crossing edges and no node at
//! the crossing, so crossings are resolved **during** growth rather than in a
//! post-pass: a post-pass would have to run over the whole graph after every
//! incremental step, which is a regeneration by another name. Splitting during
//! growth also makes each junction a snap target for later branches, which is
//! where a good share of the four-way junctions in [`junction_stats`] come from.
//!
//! Because every node position is `f32` and every split point is itself snapped,
//! "planar" is a statement at snap resolution: [`crossings`] reports an
//! intersection only when it lies farther than [`RoadGraph::snap_tolerance`] from
//! every endpoint of both segments. See [`crossings_with_tolerance`] for the
//! exact contract.

use std::collections::{BTreeMap, BTreeSet};

use polis_events::LogicalPath;
use polis_repo::RepoTree;

use crate::determinism::{debug_assert_canonical_order, det_sin_cos, narrow, SeededRng, TAU};
use crate::terrain::TerrainField;
use crate::{
    NodeId, Point, RoadClass, RoadGraph, RoadNode, RoadSegment, Vec2, DEFAULT_SNAP_TOLERANCE,
};

// ---------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------

/// Default length of one grown segment, in city-space units.
///
/// Five times [`DEFAULT_SNAP_TOLERANCE`], which is the ratio PRD §7.2's
/// "roughly a fifth of a segment" asks for. The ratio is the load-bearing part:
/// a step shorter than twice the snap radius is swallowed by snapping and the
/// branch stops making progress.
pub const DEFAULT_SEGMENT_LENGTH: f32 = 10.0;

/// Default distance at which an attractor counts as reached.
pub const DEFAULT_KILL_RADIUS: f32 = 7.0;

/// Default slope above which segments bend to follow the terrain gradient.
///
/// `terrain`'s height is in city-space units, so slope is a plain dimensionless
/// rise over run and `0.3` is a noticeable hill.
pub const DEFAULT_SLOPE_THRESHOLD: f32 = 0.30;

/// Default cap on growth steps for one attractor.
pub const DEFAULT_MAX_STEPS: u32 = 24;

/// Default branches an attractor of neutral weight pulls out of the network.
///
/// **One is a tree.** With a single branch per attractor the algorithm is a
/// greedy nearest-neighbour spanning tree: every attractor adds one node and one
/// segment, the tip always lands in empty space so snapping never fires, and the
/// graph comes out with `segments == nodes - 1` and not one cycle in it. That is
/// measured, not theorised — `the_grown_city_is_not_a_tree` failed exactly that
/// way before this constant existed. Two branches converging on the same
/// attractor from two different parts of an already-connected network close a
/// loop, which is where the cycles and the four-way junctions come from.
pub const DEFAULT_BRANCHES: u32 = 2;

/// Hard cap on branches per attractor, whatever the weight asks for.
pub const MAX_BRANCHES: u32 = 5;

/// Default radius within which an attractor influences existing nodes.
///
/// Runions' influence radius, kept: an attractor pulls on *every* node within
/// it, not only the nearest. Two and a half segments, so a branch converging on
/// an attractor has room to meet one coming from a different direction.
pub const DEFAULT_INFLUENCE_RADIUS: f32 = 25.0;

/// How far the terrain gradient may pull a segment away from its attractor.
///
/// Strictly below `1`, and that is a termination proof rather than taste: the
/// step direction is `normalize(toward_attractor + bend · downhill)`, so a bend
/// of `1` could cancel the attractor term exactly and leave a road wandering
/// downhill forever. At `0.75` the attractor component can never drop below a
/// quarter, so every step closes some distance and growth terminates.
pub const MAX_TERRAIN_BEND: f64 = 0.75;

/// Distance a step must close before the branch is considered stalled.
///
/// Guards the case where snapping repeatedly lands the branch on nodes that are
/// no nearer the attractor than the last one.
const MIN_PROGRESS: f32 = 1e-3;

/// Crossings resolved while inserting a single segment before the rest is
/// committed unsplit.
///
/// A cascade deeper than this means the new segment is threading a knot, which
/// in a snapped graph means the knot is smaller than the snap radius and the
/// extra splits would be noise. Bounds the work list, so
/// [`RoadGrowth::extend`] cannot spin inside PRD §13.1's 50 ms budget.
const MAX_SPLITS_PER_SEGMENT: u32 = 32;

/// Rings of grid cells scanned before a nearest-node query gives up on the
/// index and scans linearly.
///
/// Past this radius the ring scan costs more `BTreeMap` lookups than the whole
/// node vector costs to walk, so the fallback is faster as well as simpler.
const MAX_NEAREST_RINGS: i32 = 32;

/// Smallest usable spatial-index cell, so a degenerate parameter cannot ask for
/// an unbounded number of cells.
const MIN_CELL: f64 = 1e-3;

/// Tunables for road growth. Grouped so PRD §16's golden-file test can pin them
/// in one place, and so a change to one is visibly a change to the city.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GrowthParams {
    /// Distance at which an attractor is consumed.
    pub kill_radius: f32,
    /// **The organic signature.** Endpoints landing within this of an existing
    /// intersection snap to it instead of creating a node. Copied onto
    /// [`RoadGraph::snap_tolerance`] at construction so the two cannot diverge.
    pub snap_radius: f32,
    /// Length of one grown segment.
    pub segment_length: f32,
    /// Above this terrain slope, segments follow the gradient rather than the
    /// straight line to their attractor.
    pub slope_threshold: f32,
    /// Maximum growth steps, so a pathological attractor set cannot spin
    /// forever inside PRD §13.1's 50 ms incremental budget.
    pub max_steps: u32,
    /// Branches an attractor of neutral weight pulls out of the network.
    ///
    /// Scaled by [`Attractor::weight`] and clamped to
    /// `[1, MAX_BRANCHES]`, so a file in a 200-file directory pulls twice the
    /// road a file in a three-file directory does. **Setting this to one makes
    /// the road graph a tree**; see [`DEFAULT_BRANCHES`].
    ///
    /// [`MAX_BRANCHES`]: crate::roads::MAX_BRANCHES
    pub branches: u32,
    /// Radius within which an attractor influences existing nodes — the nodes
    /// its branches may grow out of.
    ///
    /// The nearest node is always influenced, whatever this is, so the network
    /// cannot fragment into components no road connects.
    pub influence_radius: f32,
}

impl Default for GrowthParams {
    fn default() -> Self {
        Self {
            kill_radius: DEFAULT_KILL_RADIUS,
            snap_radius: DEFAULT_SNAP_TOLERANCE,
            segment_length: DEFAULT_SEGMENT_LENGTH,
            slope_threshold: DEFAULT_SLOPE_THRESHOLD,
            max_steps: DEFAULT_MAX_STEPS,
            branches: DEFAULT_BRANCHES,
            influence_radius: DEFAULT_INFLUENCE_RADIUS,
        }
    }
}

impl GrowthParams {
    /// True when every tunable is finite, positive where it must be, and the
    /// segment is long enough not to be swallowed by snapping.
    ///
    /// The last clause is the one that bites: with `segment_length <= 2 ·
    /// snap_radius` every step lands inside the previous node's snap disc, no
    /// branch ever advances, and the city comes out as a single node with no
    /// error anywhere.
    #[must_use]
    pub fn is_sane(&self) -> bool {
        self.kill_radius.is_finite()
            && self.kill_radius >= 0.0
            && self.snap_radius.is_finite()
            && self.snap_radius >= 0.0
            && self.segment_length.is_finite()
            && self.segment_length > 0.0
            && self.slope_threshold.is_finite()
            && self.slope_threshold >= 0.0
            && self.max_steps > 0
            && self.segment_length > 2.0 * self.snap_radius
            && self.branches >= 1
            && self.influence_radius.is_finite()
            && self.influence_radius >= 0.0
    }

    /// True when this configuration can only ever produce a tree.
    ///
    /// One branch per attractor is a greedy nearest-neighbour spanning tree, and
    /// **trees read as artificial** (PRD §7.2). Separate from
    /// [`GrowthParams::is_sane`] because a tree is a perfectly well-formed graph
    /// — nothing crashes, nothing is `NaN`, the city just looks wrong.
    #[must_use]
    pub fn grows_a_tree(&self) -> bool {
        self.branches <= 1
    }

    /// A copy with every non-finite or non-positive tunable replaced by its
    /// default.
    ///
    /// Degrading loudly-but-visibly beats propagating a `NaN`: a `NaN`
    /// `segment_length` produces a city with no roads and no error, which is the
    /// failure mode PRD §16's golden files exist to make impossible.
    #[must_use]
    pub fn sanitized(self) -> Self {
        let defaults = Self::default();
        let mut out = self;
        if !out.kill_radius.is_finite() || out.kill_radius < 0.0 {
            out.kill_radius = defaults.kill_radius;
        }
        if !out.snap_radius.is_finite() || out.snap_radius < 0.0 {
            out.snap_radius = defaults.snap_radius;
        }
        if !out.segment_length.is_finite() || out.segment_length <= 0.0 {
            out.segment_length = defaults.segment_length;
        }
        if !out.slope_threshold.is_finite() || out.slope_threshold < 0.0 {
            out.slope_threshold = defaults.slope_threshold;
        }
        if out.max_steps == 0 {
            out.max_steps = defaults.max_steps;
        }
        if out.branches == 0 {
            out.branches = defaults.branches;
        }
        out.branches = out.branches.min(MAX_BRANCHES);
        if !out.influence_radius.is_finite() || out.influence_radius < 0.0 {
            out.influence_radius = defaults.influence_radius;
        }
        out
    }
}

/// One attraction point: a place a file needs to be reachable from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Attractor {
    /// Where.
    pub at: Point,
    /// How strongly. Weighted by where files need to be reachable — a district
    /// with many files pulls harder than one with two.
    ///
    /// Clamped to `[MIN_ATTRACTOR_WEIGHT, MAX_ATTRACTOR_WEIGHT]` at use, and
    /// applied by scaling [`GrowthParams::branches`] — the number of roads that
    /// converge on this point. That is what turns "a directory with 200 files"
    /// into more road than "a directory with three" beyond the raw count of
    /// attraction points, and it is visible geometry rather than a number in a
    /// struct.
    ///
    /// [`MIN_ATTRACTOR_WEIGHT`]: crate::roads::MIN_ATTRACTOR_WEIGHT
    /// [`MAX_ATTRACTOR_WEIGHT`]: crate::roads::MAX_ATTRACTOR_WEIGHT
    pub weight: f32,
}

impl Attractor {
    /// An attraction point with a weight.
    #[must_use]
    pub const fn new(at: Point, weight: f32) -> Self {
        Self { at, weight }
    }

    /// An attraction point of neutral weight.
    #[must_use]
    pub const fn neutral(at: Point) -> Self {
        Self::new(at, 1.0)
    }

    /// True when the point and its weight are both usable.
    #[must_use]
    pub fn is_finite(&self) -> bool {
        self.at.is_finite() && self.weight.is_finite()
    }
}

// ---------------------------------------------------------------------------
// The attractor scatter (PRD §7.1, §7.2)
// ---------------------------------------------------------------------------

/// Directions in the layout's angle table.
///
/// Same size as [`crate::determinism::UNIT_DIRECTIONS`] and for the same reason:
/// [`crate::determinism`] rule 4 forbids calling `sin`/`cos` on a value that
/// reaches the layout, so every angle in this module comes from this fixed table
/// of 256 quantised directions rather than from an arbitrary angle. 1.4° apart —
/// finer than anything the eye can pick out of a road network.
pub const DIRECTIONS: u32 = 256;

/// Step through [`DIRECTIONS`] that approximates the golden angle.
///
/// `158 / 256 = 0.617` against `1/φ = 0.618`. Successive multiples are
/// low-discrepancy, which is what keeps a spiral scatter from banding — the same
/// trick a sunflower uses, quantised to the table.
const GOLDEN_STEP: u32 = 158;

/// Mean spacing between attractors inside a district, in city-space units.
///
/// Comparable to [`DEFAULT_SEGMENT_LENGTH`] on purpose: attractors about a
/// segment apart give roughly one grown segment per file, which is the density
/// at which snapping produces junctions rather than a fan of dead ends.
pub const ATTRACTOR_SPACING: f32 = 12.0;

/// `1 / sqrt(π)`, written out. Turns a file count into the radius of a disc of
/// that many [`ATTRACTOR_SPACING`]-sized cells.
const INV_SQRT_PI: f64 = 0.564_189_583_547_756_3;

/// Slack between neighbouring district discs, as a multiple of their radii.
const DISTRICT_GAP: f64 = 1.35;

/// Per-file positional jitter, as a fraction of [`ATTRACTOR_SPACING`].
const JITTER_FRACTION: f64 = 0.25;

/// Per-district anchor jitter, as a fraction of the district's own radius.
const DISTRICT_JITTER_FRACTION: f64 = 0.12;

/// Lower clamp on [`Attractor::weight`].
pub const MIN_ATTRACTOR_WEIGHT: f32 = 0.5;

/// Upper clamp on [`Attractor::weight`].
pub const MAX_ATTRACTOR_WEIGHT: f32 = 2.0;

/// Margin added to [`suggested_extent`] beyond the outermost district.
const EXTENT_MARGIN: f64 = 1.10;

/// A unit vector from the layout's fixed direction table (rule 4).
///
/// `index` wraps, so callers may add golden-angle steps without reducing.
/// Pinned by `the_direction_table_is_pinned`.
#[must_use]
pub fn direction(index: u32) -> Vec2 {
    let index = index % DIRECTIONS;
    let turns = f64::from(index) / f64::from(DIRECTIONS);
    let (sin, cos) = det_sin_cos(turns * TAU);
    Vec2::new(narrow(cos), narrow(sin))
}

/// The district a file belongs to — its containing directory, the repository
/// root for a file at top level.
fn district_of(path: &LogicalPath) -> LogicalPath {
    path.parent().unwrap_or_else(LogicalPath::root)
}

/// Where one district's files are scattered.
#[derive(Debug, Clone, Copy, PartialEq)]
struct DistrictAnchor {
    /// Ordinal in growth order — district `0` holds the repository's oldest
    /// file and sits on the civic square.
    ordinal: u32,
    centre_x: f64,
    centre_y: f64,
    /// Radius of the disc the district's files fill.
    radius: f64,
    /// Files in the district.
    count: u32,
}

/// The district plan: anchors keyed by district, plus the radius the outermost
/// district reaches.
fn plan_districts(ordered: &[&LogicalPath]) -> (BTreeMap<LogicalPath, DistrictAnchor>, f64) {
    let mut counts: BTreeMap<LogicalPath, u32> = BTreeMap::new();
    let mut order: Vec<LogicalPath> = Vec::new();
    for path in ordered {
        let district = district_of(path);
        let entry = counts.entry(district.clone()).or_insert(0);
        if *entry == 0 {
            order.push(district);
        }
        *entry += 1;
    }

    let spacing = f64::from(ATTRACTOR_SPACING);
    let mut anchors = BTreeMap::new();
    // The cumulative *area* already claimed, divided by π. A district anchored at
    // `sqrt(claimed)` sits exactly outside everything placed before it, so the
    // scatter has constant areal density and the first district — the one holding
    // the repository's oldest file — lands on the origin, PRD §8's civic square.
    let mut claimed = 0.0_f64;
    let mut reach = 0.0_f64;
    for (ordinal, district) in order.iter().enumerate() {
        let count = counts[district];
        let radius = spacing * INV_SQRT_PI * f64::from(count).sqrt();
        let ring = claimed.sqrt();
        let ordinal = u32::try_from(ordinal).unwrap_or(u32::MAX);

        let heading = direction(ordinal.wrapping_mul(GOLDEN_STEP));
        let mut draw = SeededRng::for_seed(district.layout_seed(), "road district anchor");
        let jitter = radius * DISTRICT_JITTER_FRACTION;
        let centre_x = ring * f64::from(heading.x) + draw.range_f64(-jitter, jitter);
        let centre_y = ring * f64::from(heading.y) + draw.range_f64(-jitter, jitter);

        anchors.insert(
            district.clone(),
            DistrictAnchor {
                ordinal,
                centre_x,
                centre_y,
                radius,
                count,
            },
        );
        let gapped = DISTRICT_GAP * radius;
        claimed += gapped * gapped;
        reach = reach.max((centre_x * centre_x + centre_y * centre_y).sqrt() + radius);
    }
    (anchors, reach)
}

/// The files of a tree in **git growth order** (PRD §7.1).
///
/// `RepoTree::files` is a `BTreeMap`, so it arrives in path order; a *stable*
/// sort by `growth_index` therefore yields `(growth_index, path)` order without
/// cloning a key per element. Untracked files carry `growth_index ==
/// u32::MAX` and land last, in path order, which is the right answer: a file
/// git has never seen is newer than every file it has.
fn files_in_growth_order(tree: &RepoTree) -> Vec<&LogicalPath> {
    debug_assert_canonical_order(tree.files.keys(), "RepoTree::files");
    let mut files: Vec<(&LogicalPath, u32)> = tree
        .files
        .iter()
        .map(|(path, meta)| (path, meta.growth_index))
        .collect();
    files.sort_by_key(|(_, growth_index)| *growth_index);
    files.into_iter().map(|(path, _)| path).collect()
}

/// Scatters one attraction point per file, in git growth order (PRD §7.1, §7.2).
///
/// Density follows the [`RepoTree`] in two independent ways, which is what
/// PRD §7.2's "weighted by where files need to be reachable" asks for:
///
/// * **Count.** One attractor per file, so a directory of 200 files puts 200
///   points on the map and a directory of three puts three.
/// * **Weight.** `sqrt(files in district)` over the mean of that root across
///   districts, clamped to `[MIN_ATTRACTOR_WEIGHT, MAX_ATTRACTOR_WEIGHT]`. It
///   scales [`GrowthParams::branches`], so a file in a busy directory has up to
///   four roads meeting on it and a file in a quiet one has a single approach.
///
/// The geometry is a spiral of district discs ordered by the age of their oldest
/// file, and within each disc a golden-angle spiral ordered by file age. Both
/// spirals put old things in the middle: the repository's first file sits on the
/// civic square and last month's files ring the edge, which is PRD §7.1's old
/// town before a single segment has been grown.
///
/// `extent` clamps the result into the square the city occupies. Pass
/// [`suggested_extent`] unless the caller has its own reason to size the city;
/// a smaller extent squashes the outer districts, which is visible rather than
/// silent.
#[must_use]
pub fn scatter_attractors(tree: &RepoTree, extent: f32) -> Vec<Attractor> {
    let ordered = files_in_growth_order(tree);
    if ordered.is_empty() {
        return Vec::new();
    }
    let (anchors, _) = plan_districts(&ordered);

    // Normalised against the mean of `sqrt(count)` over districts rather than
    // against the mean count. The mean count is dominated by one huge directory
    // — two districts of 200 and 3 files have a mean of 101, which puts the
    // 200-file district at a weight of 1.4 and says almost nothing — whereas the
    // mean of the roots is 7.9 and separates them properly, at 1.8 against 0.2.
    #[allow(clippy::cast_precision_loss)] // district counts are far below 2^53
    let mean_root = anchors
        .values()
        .map(|anchor| f64::from(anchor.count).sqrt())
        .sum::<f64>()
        / anchors.len() as f64;
    let extent = if extent.is_finite() && extent > 0.0 {
        f64::from(extent)
    } else {
        f64::INFINITY
    };
    let spacing = f64::from(ATTRACTOR_SPACING);
    let jitter = spacing * JITTER_FRACTION;

    let mut within: BTreeMap<LogicalPath, u32> = BTreeMap::new();
    let mut out = Vec::with_capacity(ordered.len());
    for path in ordered {
        let district = district_of(path);
        let Some(anchor) = anchors.get(&district) else {
            continue;
        };
        let rank = within.entry(district).or_insert(0);
        let index = *rank;
        *rank += 1;

        // `sqrt` of the fractional rank spreads the district's files at constant
        // areal density; the golden-angle step spreads their directions.
        let radius =
            anchor.radius * ((f64::from(index) + 0.5) / f64::from(anchor.count.max(1))).sqrt();
        let dir = direction(
            anchor
                .ordinal
                .wrapping_mul(GOLDEN_STEP)
                .wrapping_add(1)
                .wrapping_add(index.wrapping_mul(GOLDEN_STEP)),
        );

        let mut rng = SeededRng::for_path(path, "road attractor jitter");
        let x = anchor.centre_x + radius * f64::from(dir.x) + rng.range_f64(-jitter, jitter);
        let y = anchor.centre_y + radius * f64::from(dir.y) + rng.range_f64(-jitter, jitter);

        let weight = narrow(f64::from(anchor.count).sqrt() / mean_root)
            .clamp(MIN_ATTRACTOR_WEIGHT, MAX_ATTRACTOR_WEIGHT);
        out.push(Attractor::new(
            Point::new(
                narrow(x.clamp(-extent, extent)),
                narrow(y.clamp(-extent, extent)),
            ),
            weight,
        ));
    }
    out
}

/// The city extent [`scatter_attractors`] wants for a tree.
///
/// The radius the outermost district reaches, plus a margin. `city` sizes
/// [`crate::CityLayout::extent`] and [`TerrainField::generate`] from this, so
/// terrain features scale with the repository instead of with a constant.
#[must_use]
pub fn suggested_extent(tree: &RepoTree) -> f32 {
    let ordered = files_in_growth_order(tree);
    if ordered.is_empty() {
        return ATTRACTOR_SPACING;
    }
    let (_, reach) = plan_districts(&ordered);
    narrow((reach * EXTENT_MARGIN + f64::from(ATTRACTOR_SPACING)).max(f64::from(ATTRACTOR_SPACING)))
}

// ---------------------------------------------------------------------------
// The spatial index
// ---------------------------------------------------------------------------

/// A uniform grid over city space, keyed by cell.
///
/// `BTreeMap` rather than `HashMap`, and not only out of habit: candidate lists
/// are read back in cell order and a `HashMap`'s per-process iteration order
/// would put a different city on the screen at every launch
/// ([`crate::determinism`], rule 2).
#[derive(Debug, Clone, Default)]
struct Grid {
    cell: f64,
    cells: BTreeMap<(i32, i32), Vec<u32>>,
}

/// Half the `i32` range, so a cell index can be offset by a ring radius without
/// overflowing.
const CELL_LIMIT: f64 = 1.0e9;

impl Grid {
    fn new(cell: f64) -> Self {
        Self {
            cell: if cell.is_finite() && cell > MIN_CELL {
                cell
            } else {
                MIN_CELL
            },
            cells: BTreeMap::new(),
        }
    }

    #[allow(clippy::cast_possible_truncation)] // clamped to ±1e9 first
    fn axis(&self, value: f32) -> i32 {
        let scaled = f64::from(value) / self.cell;
        if !scaled.is_finite() {
            return 0;
        }
        scaled.floor().clamp(-CELL_LIMIT, CELL_LIMIT) as i32
    }

    fn key(&self, at: Point) -> (i32, i32) {
        (self.axis(at.x), self.axis(at.y))
    }

    fn insert_point(&mut self, at: Point, id: u32) {
        self.cells.entry(self.key(at)).or_default().push(id);
    }

    /// Indexes a segment by every cell its bounding box touches.
    ///
    /// A bounding box over-approximates the cells a segment really crosses,
    /// which is exactly what a broad phase wants: it may hand the narrow phase a
    /// pair that does not intersect, never miss one that does.
    fn insert_bbox(&mut self, a: Point, b: Point, id: u32) {
        let (lo_x, hi_x) = min_max(self.axis(a.x), self.axis(b.x));
        let (lo_y, hi_y) = min_max(self.axis(a.y), self.axis(b.y));
        for x in lo_x..=hi_x {
            for y in lo_y..=hi_y {
                self.cells.entry((x, y)).or_default().push(id);
            }
        }
    }

    /// Every id indexed in a cell rectangle, sorted and de-duplicated.
    fn query_rect(&self, lo: (i32, i32), hi: (i32, i32)) -> Vec<u32> {
        let mut out = Vec::new();
        for x in lo.0..=hi.0 {
            for y in lo.1..=hi.1 {
                if let Some(ids) = self.cells.get(&(x, y)) {
                    out.extend_from_slice(ids);
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Every id whose cell could hold a point within `radius` of `at`.
    fn query_disc(&self, at: Point, radius: f64) -> Vec<u32> {
        let key = self.key(at);
        let rings = self.rings_for(radius);
        self.query_rect(
            (key.0.saturating_sub(rings), key.1.saturating_sub(rings)),
            (key.0.saturating_add(rings), key.1.saturating_add(rings)),
        )
    }

    /// Every id indexed in the cells a segment's bounding box touches.
    ///
    /// No margin is needed and none is taken: two segments can only intersect
    /// inside both bounding boxes, and each was indexed by every cell its own
    /// box touches, so the crossing's cell is in both sets.
    fn query_bbox(&self, a: Point, b: Point) -> Vec<u32> {
        let (lo_x, hi_x) = min_max(self.axis(a.x), self.axis(b.x));
        let (lo_y, hi_y) = min_max(self.axis(a.y), self.axis(b.y));
        self.query_rect((lo_x, lo_y), (hi_x, hi_y))
    }

    /// Cell rings that cover `radius`.
    ///
    /// A point within Euclidean distance `radius` of the query differs on each
    /// axis by at most `radius`, so its cell index differs by at most
    /// `ceil(radius / cell)`.
    #[allow(clippy::cast_possible_truncation)] // clamped below
    fn rings_for(&self, radius: f64) -> i32 {
        if !radius.is_finite() || radius <= 0.0 {
            return 0;
        }
        (radius / self.cell).ceil().clamp(0.0, CELL_LIMIT) as i32
    }
}

fn min_max(a: i32, b: i32) -> (i32, i32) {
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

/// A segment's endpoints as an order-independent key, for duplicate rejection.
fn pair_key(from: NodeId, to: NodeId) -> (u32, u32) {
    if from.0 <= to.0 {
        (from.0, to.0)
    } else {
        (to.0, from.0)
    }
}

/// Two segment indices as an order-independent key.
fn ordered_pair(left: usize, right: usize) -> (usize, usize) {
    if left <= right {
        (left, right)
    } else {
        (right, left)
    }
}

/// What [`RoadGrowth::split_segment`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Split {
    /// Split, producing a new tail segment at this index.
    Done(usize),
    /// The node was already an endpoint. Nothing to do, and the crossing is
    /// resolved as far as this segment is concerned.
    AlreadyEndpoint,
    /// One half already existed, so the segment was shortened onto the junction
    /// rather than split. Resolved, and no new segment.
    Rewired,
    /// Both halves already existed, so the segment is a redundant chord and
    /// removing it would renumber the graph. The crossing survives and the
    /// caller must stop retrying it.
    Blocked,
}

impl Split {
    /// True when the segment now has the junction as an endpoint.
    fn is_resolved(self) -> bool {
        !matches!(self, Self::Blocked)
    }
}

// ---------------------------------------------------------------------------
// Growth
// ---------------------------------------------------------------------------

/// What one call to [`RoadGrowth::extend`] did.
///
/// Reported rather than logged: PRD §13.1 budgets an incremental step at 50 ms
/// and the only way to know which half of that a repository spends is to count
/// the work, not to time it — a timing is not reproducible and cannot go in a
/// golden file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GrowthReport {
    /// Attractors taken off the pending list and grown toward.
    pub attractors_processed: usize,
    /// Attractors removed by [`consume_reached`] because a road came within the
    /// kill radius of them first.
    pub attractors_consumed: usize,
    /// Segments added to the graph, including the halves of split segments.
    pub segments_added: usize,
    /// Nodes added to the graph. Lower than `segments_added` in a healthy city:
    /// the difference is snapping doing its job.
    pub nodes_added: usize,
    /// Growth steps taken across all branches.
    pub steps: usize,
    /// Segment crossings turned into real nodes, keeping the graph planar.
    pub crossings_resolved: usize,
    /// Crossings the splitter refused, because a half would have duplicated a
    /// segment that already exists. Each one is a hole in planarity, so this is
    /// expected to be zero and worth looking at when it is not.
    pub crossings_blocked: usize,
    /// Segments whose crossing cascade hit [`MAX_SPLITS_PER_SEGMENT`] and were
    /// left partly unresolved. Also expected to be zero.
    pub cascades_truncated: usize,
}

/// A road network being grown, with the spatial indices that keep it fast.
///
/// The type PRD §7.4's "growth is genuinely incremental" is designed around:
/// construct it once, call [`RoadGrowth::extend`] with each batch of new
/// attractors, and the indices survive between calls. [`grow`] and
/// [`grow_incremental`] are thin wrappers for callers that hold only a
/// [`RoadGraph`]; they rebuild the indices in `O(nodes + segments)`, which is
/// well inside the 50 ms step budget but is pure waste in a long-running
/// process, so `city` should hold a `RoadGrowth`.
#[derive(Debug, Clone)]
pub struct RoadGrowth {
    graph: RoadGraph,
    params: GrowthParams,
    /// Node ids by cell. Serves both the snap query and the nearest-node query.
    node_index: Grid,
    /// Segment indices by cell, over-approximated by bounding box.
    segment_index: Grid,
    /// Endpoint pairs already joined, reproducing [`RoadGraph::add_segment`]'s
    /// duplicate rejection without its linear scan.
    pairs: BTreeSet<(u32, u32)>,
}

impl RoadGrowth {
    /// An empty network.
    ///
    /// `params.snap_radius` is copied onto [`RoadGraph::snap_tolerance`], which
    /// is then the authority: a later [`RoadGrowth::from_graph`] with different
    /// params keeps the graph's tolerance, so an incremental step cannot snap
    /// differently from the generation that produced the graph.
    #[must_use]
    pub fn new(params: GrowthParams) -> Self {
        let params = params.sanitized();
        Self::from_graph(RoadGraph::new(params.snap_radius), params)
    }

    /// Rebuilds the indices for an existing graph.
    ///
    /// The graph's own [`RoadGraph::snap_tolerance`] wins over
    /// `params.snap_radius`; see [`RoadGrowth::new`].
    #[must_use]
    pub fn from_graph(graph: RoadGraph, params: GrowthParams) -> Self {
        let mut params = params.sanitized();
        params.snap_radius = graph.snap_tolerance;
        let cell = f64::from(params.segment_length)
            .max(f64::from(params.snap_radius))
            .max(MIN_CELL);

        let mut node_index = Grid::new(cell);
        for (index, node) in graph.nodes.iter().enumerate() {
            node_index.insert_point(node.position, u32::try_from(index).unwrap_or(u32::MAX));
        }
        let mut segment_index = Grid::new(cell);
        let mut pairs = BTreeSet::new();
        for (index, segment) in graph.segments.iter().enumerate() {
            let (Some(a), Some(b)) = (graph.position(segment.from), graph.position(segment.to))
            else {
                continue;
            };
            segment_index.insert_bbox(a, b, u32::try_from(index).unwrap_or(u32::MAX));
            pairs.insert(pair_key(segment.from, segment.to));
        }
        Self {
            graph,
            params,
            node_index,
            segment_index,
            pairs,
        }
    }

    /// The network so far.
    #[must_use]
    pub fn graph(&self) -> &RoadGraph {
        &self.graph
    }

    /// The finished network.
    #[must_use]
    pub fn into_graph(self) -> RoadGraph {
        self.graph
    }

    /// The tunables in force, after sanitisation and after the graph's snap
    /// tolerance has overridden `snap_radius`.
    #[must_use]
    pub fn params(&self) -> GrowthParams {
        self.params
    }

    /// Junction counts and the degree distribution — PRD §7.2's organic
    /// signature, measured.
    #[must_use]
    pub fn stats(&self) -> JunctionStats {
        junction_stats(&self.graph)
    }

    /// A node's position. Panics only on a corrupt index, which cannot happen:
    /// every id this type produces came from its own `push`.
    fn position(&self, id: NodeId) -> Point {
        self.graph
            .position(id)
            .unwrap_or_else(|| unreachable!("node {id:?} is not in the graph it was created in"))
    }

    /// [`RoadGraph::snap_target`], answered from the index.
    ///
    /// Reproduces the scan exactly: same [`Point::distance_squared`], same
    /// `<= tolerance²` test, same "ties break to the lowest node index" rule —
    /// which is why candidates are sorted before the minimum is taken.
    /// `snap_index_agrees_with_the_graphs_own_scan` asserts the equivalence on
    /// pseudo-random point sets.
    fn snap_target(&self, at: Point) -> Option<NodeId> {
        if !at.is_finite() {
            return None;
        }
        let tolerance = self.graph.snap_tolerance;
        if !tolerance.is_finite() || tolerance < 0.0 {
            return None;
        }
        let limit = tolerance * tolerance;
        let mut best: Option<(u32, f32)> = None;
        for id in self.node_index.query_disc(at, f64::from(tolerance)) {
            let Some(position) = self.graph.position(NodeId(id)) else {
                continue;
            };
            let d2 = position.distance_squared(at);
            if d2 <= limit && best.is_none_or(|(_, best_d2)| d2 < best_d2) {
                best = Some((id, d2));
            }
        }
        best.map(|(id, _)| NodeId(id))
    }

    /// The nearest node to a point, or `None` for an empty graph.
    ///
    /// Expands rings until the best candidate found is provably nearer than
    /// anything an unscanned ring could hold, then falls back to a linear scan
    /// past [`MAX_NEAREST_RINGS`] — at that radius the ring scan costs more
    /// `BTreeMap` lookups than the node vector costs to walk. Ties break to the
    /// lowest node index, as everywhere else in this module.
    fn nearest_node(&self, at: Point) -> Option<NodeId> {
        if self.graph.nodes.is_empty() || !at.is_finite() {
            return None;
        }
        let key = self.node_index.key(at);
        let mut best: Option<(u32, f32)> = None;
        let mut ring = 0_i32;
        while ring <= MAX_NEAREST_RINGS {
            for id in self.node_index.query_rect(
                (key.0.saturating_sub(ring), key.1.saturating_sub(ring)),
                (key.0.saturating_add(ring), key.1.saturating_add(ring)),
            ) {
                let Some(position) = self.graph.position(NodeId(id)) else {
                    continue;
                };
                let d2 = position.distance_squared(at);
                if best.is_none_or(|(_, best_d2)| d2 < best_d2) {
                    best = Some((id, d2));
                }
            }
            // Anything in an unscanned cell is at least `ring · cell` away on one
            // axis, so a candidate nearer than that is the global nearest.
            if let Some((_, best_d2)) = best {
                let covered = f64::from(ring) * self.node_index.cell;
                if f64::from(best_d2) <= covered * covered {
                    return best.map(|(id, _)| NodeId(id));
                }
            }
            ring += 1;
        }
        self.nearest_node_linear(at)
    }

    /// The fallback half of [`RoadGrowth::nearest_node`]. Same answer, no index.
    fn nearest_node_linear(&self, at: Point) -> Option<NodeId> {
        let mut best: Option<(u32, f32)> = None;
        for (index, node) in self.graph.nodes.iter().enumerate() {
            let d2 = node.position.distance_squared(at);
            if best.is_none_or(|(_, best_d2)| d2 < best_d2) {
                best = Some((u32::try_from(index).unwrap_or(u32::MAX), d2));
            }
        }
        best.map(|(id, _)| NodeId(id))
    }

    /// Adds a node, snapping to an existing one where PRD §7.2 says to.
    ///
    /// **The organic signature.** Equivalent to
    /// [`RoadGraph::add_node_snapped`], and every growth step goes through it.
    /// It pushes only after [`RoadGrowth::snap_target`] has answered `None`,
    /// which is the same question `add_node_snapped` asks by scanning every
    /// node — the index is an acceleration, never a way around the rule.
    fn add_node_snapped(&mut self, at: Point) -> NodeId {
        if let Some(existing) = self.snap_target(at) {
            return existing;
        }
        self.push_node(at)
    }

    /// The crate's only direct push onto [`RoadGraph::nodes`]. Two callers, both
    /// in this file, both documented at their call site.
    fn push_node(&mut self, at: Point) -> NodeId {
        let Ok(index) = u32::try_from(self.graph.nodes.len()) else {
            // Unreachable for any real repository; returning an existing id keeps
            // the graph consistent rather than aliasing node 2^32 onto node 0.
            return NodeId(u32::MAX);
        };
        self.graph.nodes.push(RoadNode { position: at });
        self.node_index.insert_point(at, index);
        NodeId(index)
    }

    /// True when splitting `index` at `at` would be refused.
    ///
    /// Only the "both halves already exist" case, which is the one
    /// [`RoadGrowth::split_segment`] cannot resolve without renumbering the
    /// graph.
    fn split_blocked(&self, index: usize, at: NodeId) -> bool {
        let Some(&segment) = self.graph.segments.get(index) else {
            return true;
        };
        if segment.from == at || segment.to == at {
            return false;
        }
        self.pairs.contains(&pair_key(segment.from, at))
            && self.pairs.contains(&pair_key(at, segment.to))
    }

    /// The node a crossing becomes.
    ///
    /// Normally the snapped node — this is PRD §7.2's organic signature and
    /// nothing here weakens it. The one documented exception: when snapping
    /// would land on a node that already has segments to *both* ends of a
    /// segment being split, the split is impossible without renumbering the
    /// graph, and refusing it leaves a crossing with no node at it. Face
    /// extraction is undefined on such a graph, so in that narrow case the
    /// crossing gets its own node at the exact intersection instead.
    ///
    /// A new node cannot be blocked — no segment can already reach an id that
    /// did not exist a moment ago — so this terminates the retry immediately.
    /// Measured on the fixture city: 16 crossings in 994 segments took this
    /// path, and every one of them was an unresolvable crossing before it
    /// existed.
    fn junction_node(&mut self, at: Point, first: usize, second: usize) -> NodeId {
        if let Some(existing) = self.snap_target(at) {
            if !self.split_blocked(first, existing) && !self.split_blocked(second, existing) {
                return existing;
            }
        }
        self.push_node(at)
    }

    /// Adds a segment, rejecting self-loops and duplicates in either direction.
    ///
    /// Equivalent to [`RoadGraph::add_segment`] without its linear scan.
    fn commit_segment(&mut self, from: NodeId, to: NodeId, class: RoadClass) -> bool {
        if from == to {
            return false;
        }
        if !self.pairs.insert(pair_key(from, to)) {
            return false;
        }
        let Ok(index) = u32::try_from(self.graph.segments.len()) else {
            return false;
        };
        let (a, b) = (self.position(from), self.position(to));
        self.graph.segments.push(RoadSegment { from, to, class });
        self.segment_index.insert_bbox(a, b, index);
        true
    }

    /// Splits an existing segment at a node lying on it.
    ///
    /// The shrunk half keeps its grid entries, which now cover more cells than
    /// it occupies. That is safe: the segment index is a broad phase, and an
    /// over-approximation costs a narrow-phase test, never a missed crossing.
    fn split_segment(&mut self, index: usize, at: NodeId) -> Split {
        let Some(&segment) = self.graph.segments.get(index) else {
            return Split::Blocked;
        };
        if segment.from == at || segment.to == at {
            return Split::AlreadyEndpoint;
        }
        let old = pair_key(segment.from, segment.to);
        let head = pair_key(segment.from, at);
        let tail = pair_key(at, segment.to);

        // The junction is a *snapped* node, so it can be one the network already
        // reaches. When one half of the split already exists as a segment, the
        // right answer is not to refuse the split — it is to keep only the half
        // that is missing. Connectivity is unchanged (`from → at → to` is still
        // walkable) and the crossing is resolved, which is what matters.
        match (self.pairs.contains(&head), self.pairs.contains(&tail)) {
            (false, false) => {
                let Ok(tail_index) = u32::try_from(self.graph.segments.len()) else {
                    return Split::Blocked;
                };
                self.pairs.remove(&old);
                self.pairs.insert(head);
                self.pairs.insert(tail);
                self.graph.segments[index].to = at;
                self.graph.segments.push(RoadSegment {
                    from: at,
                    to: segment.to,
                    class: segment.class,
                });
                let (a, b) = (self.position(at), self.position(segment.to));
                self.segment_index.insert_bbox(a, b, tail_index);
                Split::Done(tail_index as usize)
            }
            (true, false) => {
                self.pairs.remove(&old);
                self.pairs.insert(tail);
                self.graph.segments[index].from = at;
                self.reindex(index);
                Split::Rewired
            }
            (false, true) => {
                self.pairs.remove(&old);
                self.pairs.insert(head);
                self.graph.segments[index].to = at;
                self.reindex(index);
                Split::Rewired
            }
            // Both halves already exist, so this segment is a redundant chord of
            // a path the network already has. Removing it would renumber every
            // later segment — and therefore every id in the grid, the work list
            // and the skip set — so it is left in place and counted instead.
            (true, true) => Split::Blocked,
        }
    }

    /// Re-indexes a segment whose geometry changed. Old cell entries are left
    /// behind: the index is a broad phase and over-approximating costs a
    /// narrow-phase test, never a missed crossing.
    fn reindex(&mut self, index: usize) {
        let Some(&segment) = self.graph.segments.get(index) else {
            return;
        };
        let (a, b) = (self.position(segment.from), self.position(segment.to));
        self.segment_index
            .insert_bbox(a, b, u32::try_from(index).unwrap_or(u32::MAX));
    }

    /// The first crossing on segment `index`, as `(other segment, point)`.
    ///
    /// "First" means smallest parameter along `index`, ties to the lowest other
    /// index — a total order, so the split sequence is the same on every
    /// machine. Candidates sharing a node with `index` are skipped: they meet at
    /// a junction, which is what snapping was for.
    fn first_crossing(
        &self,
        index: usize,
        skip: &BTreeSet<(usize, usize)>,
    ) -> Option<(usize, Point)> {
        let &segment = self.graph.segments.get(index)?;
        let (head, tail) = (self.position(segment.from), self.position(segment.to));
        let mut best: Option<(f64, usize, Point)> = None;
        for candidate in self.segment_index.query_bbox(head, tail) {
            let other = candidate as usize;
            if other == index || skip.contains(&ordered_pair(index, other)) {
                continue;
            }
            let Some(&second) = self.graph.segments.get(other) else {
                continue;
            };
            if second.from == segment.from
                || second.from == segment.to
                || second.to == segment.from
                || second.to == segment.to
            {
                continue;
            }
            let (other_head, other_tail) = (self.position(second.from), self.position(second.to));
            let Some((along, point)) = segment_intersection(head, tail, other_head, other_tail)
            else {
                continue;
            };
            // Strictly less, over candidates already in ascending index order, so
            // a tie in `along` resolves to the lower index.
            if best.is_none_or(|(best_along, _, _)| along < best_along) {
                best = Some((along, other, point));
            }
        }
        best.map(|(_, other, point)| (other, point))
    }

    /// Adds a segment and resolves every crossing it makes into a real node.
    ///
    /// Commits first, then verifies: the work list holds *segment indices* to
    /// re-check, not pairs of nodes to place. That distinction is the fix for a
    /// real bug — splitting a crossed segment at a snapped junction bends it by
    /// up to one snap radius, which can put it across a third segment it did not
    /// previously touch. A work list of node pairs re-checks only the incoming
    /// road and leaves those behind; `the_grown_city_is_planar` counted 168 of
    /// them in a 14 000-segment city. Pushing both halves of both split segments
    /// back onto the list closes it.
    ///
    /// Each crossing splits **both** segments at one node, so that node has
    /// degree four by construction — which is where a good share of PRD §7.2's
    /// "irregular four- and five-way junctions" come from, and why they are
    /// justified rather than decorative.
    ///
    /// Terminates: every iteration either finds no crossing (and drops the item)
    /// or spends one of [`MAX_SPLITS_PER_SEGMENT`], and a pair that could not be
    /// split goes into `skip` so it can never be chosen twice.
    fn add_segment_planar(
        &mut self,
        from: NodeId,
        to: NodeId,
        class: RoadClass,
        report: &mut GrowthReport,
    ) {
        if from == to || !self.commit_segment(from, to, class) {
            return;
        }
        report.segments_added += 1;

        let mut work = vec![self.graph.segments.len() - 1];
        let mut skip: BTreeSet<(usize, usize)> = BTreeSet::new();
        let mut budget = MAX_SPLITS_PER_SEGMENT;
        while let Some(index) = work.pop() {
            if budget == 0 {
                report.cascades_truncated += 1;
                break;
            }
            let Some((other, point)) = self.first_crossing(index, &skip) else {
                continue;
            };
            budget -= 1;

            let junction = self.junction_node(point, index, other);
            let on_this = self.split_segment(index, junction);
            let on_other = self.split_segment(other, junction);
            if let Split::Done(tail) = on_this {
                report.segments_added += 1;
                work.push(tail);
            }
            if let Split::Done(tail) = on_other {
                report.segments_added += 1;
                work.push(tail);
            }
            if on_this.is_resolved() && on_other.is_resolved() {
                report.crossings_resolved += 1;
            } else {
                report.crossings_blocked += 1;
                skip.insert(ordered_pair(index, other));
            }
            work.push(index);
            work.push(other);
        }
    }

    /// The nodes an attractor influences, nearest first.
    ///
    /// Runions' influence radius: an attractor pulls on every node within it,
    /// not only the nearest. Ordered by distance with ties broken to the lowest
    /// node index, and truncated to `limit`. Always non-empty for a non-empty
    /// graph — the nearest node is included whatever the radius, so growth can
    /// never leave a district stranded with no road to it.
    fn influenced_nodes(&self, at: Point, radius: f32, limit: u32) -> Vec<NodeId> {
        let Some(nearest) = self.nearest_node(at) else {
            return Vec::new();
        };
        if limit <= 1 {
            return vec![nearest];
        }
        let mut candidates: Vec<(u32, f32)> = self
            .node_index
            .query_disc(at, f64::from(radius))
            .into_iter()
            .filter_map(|id| {
                self.graph
                    .position(NodeId(id))
                    .map(|position| (id, position.distance(at)))
            })
            .filter(|(_, distance)| *distance <= radius)
            .collect();
        // Distance first, then node index: a total order over a set gathered in
        // cell order, so the branch origins are the same on every machine.
        candidates.sort_by(|(left_id, left), (right_id, right)| {
            left.total_cmp(right).then(left_id.cmp(right_id))
        });

        let mut out = vec![nearest];
        for (id, _) in candidates {
            if out.len() >= limit as usize {
                break;
            }
            if !out.contains(&NodeId(id)) {
                out.push(NodeId(id));
            }
        }
        out
    }

    /// Grows the branches one attractor pulls out of the network.
    ///
    /// Prefix-stable, which is the whole design (see the module docs). The rule
    /// that carries it: the "already served" test uses the same unweighted kill
    /// radius [`consume_reached`] sweeps with, so an attractor swept away during
    /// a full run and an attractor skipped during an incremental one are the
    /// same attractor, and neither grows any road.
    ///
    /// [`Attractor::weight`] sets how many branches converge here. That is where
    /// "a directory with 200 files pulls more road than one with three" stops
    /// being a count of attraction points and becomes visible geometry: heavy
    /// attractors get up to four roads meeting on them, light ones get one.
    fn grow_one(
        &mut self,
        terrain: &TerrainField,
        target: Attractor,
        class: RoadClass,
        pending: &mut Vec<Attractor>,
        report: &mut GrowthReport,
    ) {
        let params = self.params;
        let sweep_radius = params.kill_radius;

        if self.graph.nodes.is_empty() {
            // An empty graph starts at the origin — PRD §8's civic square, and
            // therefore the point the repository's oldest file grows out of.
            self.add_node_snapped(Point::ORIGIN);
        }
        let Some(nearest) = self.nearest_node(target.at) else {
            return;
        };
        if self.position(nearest).distance(target.at) <= sweep_radius {
            return;
        }

        let weight = target
            .weight
            .clamp(MIN_ATTRACTOR_WEIGHT, MAX_ATTRACTOR_WEIGHT);
        // Rounded through a floor of `+ 0.5` rather than `f32::round`, so the
        // tie at exactly `.5` resolves one documented way on every target.
        let scaled = (f64::from(params.branches) * f64::from(weight) + 0.5)
            .floor()
            .clamp(1.0, f64::from(MAX_BRANCHES));
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // clamped above
        let branches = scaled as u32;

        for origin in self.influenced_nodes(target.at, params.influence_radius, branches) {
            self.grow_branch(terrain, origin, target.at, class, pending, report);
        }
    }

    /// Grows one branch from `origin` until it arrives at the attractor.
    ///
    /// Arriving matters: [`step_towards`] clamps the last step to the remaining
    /// distance, so every branch ends on the attractor point itself and the
    /// second branch's tip snaps onto the first's. That merge is the cycle.
    fn grow_branch(
        &mut self,
        terrain: &TerrainField,
        origin: NodeId,
        target: Point,
        class: RoadClass,
        pending: &mut Vec<Attractor>,
        report: &mut GrowthReport,
    ) {
        let params = self.params;
        let arrival = params.snap_radius.max(MIN_PROGRESS);
        let mut current = origin;
        let mut distance = self.position(current).distance(target);

        for _ in 0..params.max_steps {
            if distance <= arrival {
                break;
            }
            report.steps += 1;
            let start = self.position(current);
            let next = self.add_node_snapped(step_towards(start, target, terrain, params));
            if next == current {
                // Snapping swallowed the step. Nothing here will advance.
                break;
            }
            report.attractors_consumed +=
                consume_reached(pending, self.position(next), params.kill_radius);
            self.add_segment_planar(current, next, class, report);
            current = next;

            let reached = self.position(current).distance(target);
            if reached >= distance - MIN_PROGRESS {
                break;
            }
            distance = reached;
        }
    }

    /// Grows the network toward a batch of attractors, in the order given.
    ///
    /// This is PRD §7.4's growth step. Adding one file means calling this with
    /// one attractor; the indices, the graph and the snap tolerance all persist,
    /// and nothing is regenerated.
    ///
    /// `attractors` must arrive in **git growth order** (PRD §7.1).
    pub fn extend(
        &mut self,
        terrain: &TerrainField,
        attractors: &[Attractor],
        class: RoadClass,
    ) -> GrowthReport {
        let mut report = GrowthReport::default();
        let nodes_before = self.graph.nodes.len();
        // Reversed, so `pop` yields growth order in `O(1)`. `consume_reached`
        // retains relative order, so the reversal survives every sweep.
        let mut pending: Vec<Attractor> = attractors.iter().rev().copied().collect();
        while let Some(target) = pending.pop() {
            if !target.is_finite() {
                continue;
            }
            report.attractors_processed += 1;
            self.grow_one(terrain, target, class, &mut pending, &mut report);
        }
        report.nodes_added = self.graph.nodes.len() - nodes_before;
        report
    }
}

/// Grows a network over a terrain field.
///
/// `attractors` must arrive in **git growth order** (PRD §7.1); see the module
/// docs. `class` sets the [`RoadClass`] of every segment this pass creates, so
/// arterials between districts and streets within one are two passes rather than
/// a per-segment decision.
#[must_use]
pub fn grow(
    terrain: &TerrainField,
    attractors: &[Attractor],
    params: GrowthParams,
    class: RoadClass,
) -> RoadGraph {
    let mut growth = RoadGrowth::new(params);
    growth.extend(terrain, attractors, class);
    growth.into_graph()
}

/// Runs one incremental growth step for newly-added attractors.
///
/// > Growth is genuinely incremental — a new file runs one growth step, it does
/// > not regenerate the world. (PRD §7.4)
///
/// Must produce the same graph as a full [`grow`] over the concatenated
/// attractor list; that equivalence is what makes an incrementally-grown city
/// and a freshly-generated one byte-identical, which PRD §16 asserts.
///
/// Returns the number of segments added. The graph's own
/// [`RoadGraph::snap_tolerance`] is used, not `params.snap_radius`, so a caller
/// that reconstructs its params cannot snap differently from the run that built
/// the graph.
///
/// Rebuilds the spatial indices from `graph` on every call. That is `O(nodes +
/// segments)` and comfortably inside PRD §13.1's 50 ms step, but a long-running
/// process should hold a [`RoadGrowth`] and call [`RoadGrowth::extend`] instead.
pub fn grow_incremental(
    graph: &mut RoadGraph,
    terrain: &TerrainField,
    attractors: &[Attractor],
    params: GrowthParams,
    class: RoadClass,
) -> usize {
    let tolerance = graph.snap_tolerance;
    let taken = std::mem::replace(graph, RoadGraph::new(tolerance));
    let mut growth = RoadGrowth::from_graph(taken, params);
    let report = growth.extend(terrain, attractors, class);
    *graph = growth.into_graph();
    report.segments_added
}

/// Chooses the next segment endpoint from a node towards an attractor.
///
/// Straight towards the attractor below [`GrowthParams::slope_threshold`], along
/// the terrain gradient above it. Split out from the growth loop because it is
/// the one piece of the algorithm with a testable, self-contained contract.
///
/// Above the threshold the direction is `normalize(toward_attractor + bend ·
/// downhill)`, with `bend` ramping from zero at the threshold towards
/// [`MAX_TERRAIN_BEND`]. Aligning fully with the gradient — the literal reading
/// of "follow the terrain gradient" — would describe a road that runs downhill
/// and never arrives; bounding the bend below `1` keeps the attractor term
/// dominant, which is both what makes growth terminate and what makes the
/// curvature read as a road going *around* a hill rather than wandering.
///
/// The step never overshoots: it is `min(segment_length, distance)`.
#[must_use]
pub fn step_towards(
    from: Point,
    attractor: Point,
    terrain: &TerrainField,
    params: GrowthParams,
) -> Point {
    if !from.is_finite() || !attractor.is_finite() {
        return from;
    }
    let params = params.sanitized();
    let (from_x, from_y) = (f64::from(from.x), f64::from(from.y));
    let to_x = f64::from(attractor.x) - from_x;
    let to_y = f64::from(attractor.y) - from_y;
    let distance = (to_x * to_x + to_y * to_y).sqrt();
    if distance <= 0.0 || !distance.is_finite() {
        return from;
    }
    let (mut dir_x, mut dir_y) = (to_x / distance, to_y / distance);

    // `sample` returns the *downhill* gradient; its length is the slope.
    let (_, down_x, down_y) = terrain.sample(from_x, from_y);
    let slope = (down_x * down_x + down_y * down_y).sqrt();
    let threshold = f64::from(params.slope_threshold);
    if slope > threshold && slope > 0.0 {
        // A bounded rational ramp: zero at the threshold, approaching
        // MAX_TERRAIN_BEND on steep ground, and free of `powf` (rule 4).
        let bend = (MAX_TERRAIN_BEND * (slope - threshold) / (slope + threshold))
            .clamp(0.0, MAX_TERRAIN_BEND);
        dir_x += bend * down_x / slope;
        dir_y += bend * down_y / slope;
        let length = (dir_x * dir_x + dir_y * dir_y).sqrt();
        // `length >= 1 - MAX_TERRAIN_BEND > 0` by construction; the guard is here
        // so a future change to MAX_TERRAIN_BEND cannot make this a division by
        // zero in silence.
        if length > 0.0 {
            dir_x /= length;
            dir_y /= length;
        }
    }

    let step = f64::from(params.segment_length).min(distance);
    Point::new(narrow(from_x + dir_x * step), narrow(from_y + dir_y * step))
}

/// Removes attractors within [`GrowthParams::kill_radius`] of a node.
///
/// Returns how many were consumed. Retains order among the survivors — the
/// growth order is the history (see the module docs), so a `swap_remove` here
/// would quietly reshuffle it.
///
/// A non-finite point or radius consumes nothing rather than consuming
/// everything, which is what a `NaN` comparison would do.
pub fn consume_reached(attractors: &mut Vec<Attractor>, at: Point, kill_radius: f32) -> usize {
    if !at.is_finite() || !kill_radius.is_finite() {
        return 0;
    }
    let limit = kill_radius * kill_radius;
    let before = attractors.len();
    // Compared through `partial_cmp`, so a `NaN` distance *retains* the
    // attractor. A plain `>` would do the opposite and quietly delete it.
    attractors.retain(|attractor| {
        attractor
            .at
            .distance_squared(at)
            .partial_cmp(&limit)
            .is_none_or(std::cmp::Ordering::is_gt)
    });
    before - attractors.len()
}

// ---------------------------------------------------------------------------
// Metrics — the organic signature, measured
// ---------------------------------------------------------------------------

/// Junction counts and the degree distribution of a road graph (PRD §7.2).
///
/// Exists because "without snapping you get a tree, and trees read as
/// artificial" is a **silent** product failure: a tree compiles, runs, renders,
/// and simply looks wrong. `the_grown_city_is_not_a_tree` asserts against it,
/// and this is what it asserts on.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct JunctionStats {
    /// Nodes in the graph.
    pub nodes: usize,
    /// Segments in the graph.
    pub segments: usize,
    /// Connected components, counting isolated nodes.
    pub components: usize,
    /// Independent cycles — `segments - nodes + components`, the cyclomatic
    /// number. **Zero means the graph is a tree or a forest**, which is the
    /// failure this whole struct exists to catch.
    pub cycles: usize,
    /// Node count by degree, in ascending degree order.
    pub degrees: BTreeMap<u32, usize>,
    /// Nodes with no segments at all.
    pub isolated: usize,
    /// Nodes of degree one.
    pub dead_ends: usize,
    /// Nodes of degree three or more — junctions.
    pub junctions: usize,
    /// Nodes of degree four or more — the irregular four- and five-way junctions
    /// PRD §7.2 says the eye reads as "grown".
    pub complex_junctions: usize,
    /// Highest degree in the graph.
    pub max_degree: u32,
}

impl JunctionStats {
    /// True when the graph contains no cycle. **A tree reads as artificial.**
    #[must_use]
    pub fn is_tree(&self) -> bool {
        self.cycles == 0
    }

    /// Junctions as a share of all nodes.
    #[must_use]
    pub fn junction_share(&self) -> f32 {
        share(self.junctions, self.nodes)
    }

    /// Degree-four-and-up junctions as a share of all nodes.
    #[must_use]
    pub fn complex_junction_share(&self) -> f32 {
        share(self.complex_junctions, self.nodes)
    }

    /// Cycles per node — a scale-free reading of how tangled the network is.
    #[must_use]
    pub fn cycle_share(&self) -> f32 {
        share(self.cycles, self.nodes)
    }
}

#[allow(clippy::cast_precision_loss)] // node counts are far below 2^24
fn share(part: usize, whole: usize) -> f32 {
    if whole == 0 {
        return 0.0;
    }
    part as f32 / whole as f32
}

/// Measures the junctions of a road graph.
#[must_use]
pub fn junction_stats(graph: &RoadGraph) -> JunctionStats {
    let node_count = graph.nodes.len();
    let mut degree = vec![0_u32; node_count];
    let mut parent: Vec<u32> = (0..u32::try_from(node_count).unwrap_or(u32::MAX)).collect();

    for segment in &graph.segments {
        let (Some(a), Some(b)) = (
            usize::try_from(segment.from.0)
                .ok()
                .filter(|i| *i < node_count),
            usize::try_from(segment.to.0)
                .ok()
                .filter(|i| *i < node_count),
        ) else {
            continue;
        };
        degree[a] += 1;
        degree[b] += 1;
        union(&mut parent, segment.from.0, segment.to.0);
    }

    let mut roots: BTreeSet<u32> = BTreeSet::new();
    for index in 0..u32::try_from(node_count).unwrap_or(u32::MAX) {
        roots.insert(find(&mut parent, index));
    }

    let mut stats = JunctionStats {
        nodes: node_count,
        segments: graph.segments.len(),
        components: roots.len(),
        ..JunctionStats::default()
    };
    for &d in &degree {
        *stats.degrees.entry(d).or_insert(0) += 1;
        match d {
            0 => stats.isolated += 1,
            1 => stats.dead_ends += 1,
            _ => {}
        }
        if d >= 3 {
            stats.junctions += 1;
        }
        if d >= 4 {
            stats.complex_junctions += 1;
        }
        stats.max_degree = stats.max_degree.max(d);
    }
    // `E - V + C`, saturating: a graph with dangling segment endpoints (which
    // this crate never builds) would otherwise underflow.
    stats.cycles = stats
        .segments
        .saturating_add(stats.components)
        .saturating_sub(stats.nodes);
    stats
}

fn find(parent: &mut [u32], mut node: u32) -> u32 {
    while parent[node as usize] != node {
        let grandparent = parent[parent[node as usize] as usize];
        parent[node as usize] = grandparent;
        node = grandparent;
    }
    node
}

fn union(parent: &mut [u32], a: u32, b: u32) {
    let (ra, rb) = (find(parent, a), find(parent, b));
    if ra == rb {
        return;
    }
    // Union by index rather than by rank: the smaller index always wins, so the
    // forest — and therefore the component count — is a pure function of the
    // segment list and not of the order the tree happened to balance.
    if ra < rb {
        parent[rb as usize] = ra;
    } else {
        parent[ra as usize] = rb;
    }
}

// ---------------------------------------------------------------------------
// Planarity
// ---------------------------------------------------------------------------

/// Two segments that intersect with no node at the intersection.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Crossing {
    /// Index of the lower-numbered segment.
    pub a: usize,
    /// Index of the higher-numbered segment.
    pub b: usize,
    /// Where they cross.
    pub at: Point,
}

/// Unresolved crossings in a road graph, at the graph's own snap tolerance.
///
/// Empty means the graph is planar and `blocks` can walk its faces.
#[must_use]
pub fn crossings(graph: &RoadGraph) -> Vec<Crossing> {
    crossings_with_tolerance(graph, graph.snap_tolerance)
}

/// Unresolved crossings, tolerating only an intersection that lands within
/// `tolerance` of an endpoint of **both** segments.
///
/// The `both` is load-bearing and was a real bug before
/// `an_unsplit_t_junction_is_reported_as_a_crossing` caught it: a T-junction
/// whose stem ends *on* another segment's interior sits at distance zero from
/// one of its own endpoints, so a rule that asked for "near any endpoint of
/// either segment" declared it fine. It is not fine — a face walk cannot turn a
/// corner that has no node — and this rule reports it.
///
/// The tolerance is not slack, it is the contract. Every node position is `f32`
/// and every split point is itself snapped, so a resolved crossing sits within
/// one snap radius of a node that is an endpoint of all four resulting pieces —
/// by construction, because that is what snapping *is*. Anything else is a real
/// hole in the planarity of the graph and face extraction downstream is
/// undefined.
///
/// Output is sorted by `(a, b)`.
#[must_use]
pub fn crossings_with_tolerance(graph: &RoadGraph, tolerance: f32) -> Vec<Crossing> {
    let tolerance = if tolerance.is_finite() && tolerance > 0.0 {
        f64::from(tolerance)
    } else {
        0.0
    };

    // Cell size from the mean segment length: small enough that a cell holds few
    // segments, large enough that one segment spans few cells.
    let mut total = 0.0_f64;
    let mut counted = 0.0_f64;
    for segment in &graph.segments {
        if let (Some(a), Some(b)) = (graph.position(segment.from), graph.position(segment.to)) {
            total += f64::from(a.distance(b));
            counted += 1.0;
        }
    }
    let cell = if counted > 0.0 {
        (total / counted).max(tolerance * 2.0).max(MIN_CELL)
    } else {
        MIN_CELL
    };

    let mut index = Grid::new(cell);
    for (i, segment) in graph.segments.iter().enumerate() {
        if let (Some(a), Some(b)) = (graph.position(segment.from), graph.position(segment.to)) {
            index.insert_bbox(a, b, u32::try_from(i).unwrap_or(u32::MAX));
        }
    }

    let mut out = Vec::new();
    for (i, first) in graph.segments.iter().enumerate() {
        let (Some(head), Some(tail)) = (graph.position(first.from), graph.position(first.to))
        else {
            continue;
        };
        for candidate in index.query_bbox(head, tail) {
            let other = candidate as usize;
            if other <= i {
                continue;
            }
            let Some(&second) = graph.segments.get(other) else {
                continue;
            };
            if second.from == first.from
                || second.from == first.to
                || second.to == first.from
                || second.to == first.to
            {
                continue;
            }
            let (Some(other_head), Some(other_tail)) =
                (graph.position(second.from), graph.position(second.to))
            else {
                continue;
            };
            let Some((_, at)) = segment_intersection(head, tail, other_head, other_tail) else {
                continue;
            };
            let on_first = f64::from(at.distance(head).min(at.distance(tail)));
            let on_second = f64::from(at.distance(other_head).min(at.distance(other_tail)));
            if on_first > tolerance || on_second > tolerance {
                out.push(Crossing { a: i, b: other, at });
            }
        }
    }
    out.sort_by_key(|crossing| (crossing.a, crossing.b));
    out
}

/// True when the graph has no unresolved crossings — PRD §7.2 step 3's
/// precondition.
#[must_use]
pub fn is_planar(graph: &RoadGraph) -> bool {
    crossings(graph).is_empty()
}

/// Where two closed segments meet, as `(parameter along `a→b`, point)`.
///
/// Closed on both parameters, so a segment that ends *on* another segment counts
/// as an intersection: an unsplit T-junction breaks a face walk exactly as a
/// crossing does.
///
/// Parallel and collinear pairs return `None`. Collinear overlap is a real case
/// in principle and is handled by snapping rather than here: two collinear
/// segments closer than the snap radius share their endpoints already, and the
/// growth loop never emits a second segment between the same pair of nodes.
///
/// All in `f64`, with no `mul_add` ([`crate::determinism`], rules 3 and 5).
#[must_use]
fn segment_intersection(
    first_head: Point,
    first_tail: Point,
    second_head: Point,
    second_tail: Point,
) -> Option<(f64, Point)> {
    let (ax, ay) = (f64::from(first_head.x), f64::from(first_head.y));
    let (rx, ry) = (f64::from(first_tail.x) - ax, f64::from(first_tail.y) - ay);
    let (cx, cy) = (f64::from(second_head.x), f64::from(second_head.y));
    let (sx, sy) = (f64::from(second_tail.x) - cx, f64::from(second_tail.y) - cy);

    let denominator = rx * sy - ry * sx;
    if denominator == 0.0 || !denominator.is_finite() {
        return None;
    }
    let (qx, qy) = (cx - ax, cy - ay);
    let along_first = (qx * sy - qy * sx) / denominator;
    let along_second = (qx * ry - qy * rx) / denominator;
    if !(0.0..=1.0).contains(&along_first) || !(0.0..=1.0).contains(&along_second) {
        return None;
    }
    Some((
        along_first,
        Point::new(narrow(ax + along_first * rx), narrow(ay + along_first * ry)),
    ))
}

#[cfg(test)]
mod tests {
    // Determinism assertions here are exact and bit-level on purpose (PRD §7.4,
    // §16); `float_cmp` exists to catch approximate equality written as `==`,
    // which is the opposite of what these tests are for.
    #![allow(clippy::float_cmp)]

    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::process::Command;

    use polis_events::{LogicalPath, WallTime};
    use polis_repo::{FileClass, FileMeta, RepoTree};

    use super::*;
    use crate::determinism::fnv1a64;

    fn lp(text: &str) -> LogicalPath {
        LogicalPath::new(text).expect("valid logical path")
    }

    /// A tree of `districts × per_district` files, in a growth order that walks
    /// the districts round-robin — the shape a real repository has, where a
    /// commit touches one area and the next touches another.
    fn synthetic_tree(districts: usize, per_district: usize) -> RepoTree {
        let mut files = BTreeMap::new();
        let mut growth_index = 0_u32;
        for file in 0..per_district {
            for district in 0..districts {
                let path = lp(&format!("crate{district:03}/src/mod{file:04}.rs"));
                files.insert(
                    path.clone(),
                    FileMeta {
                        path,
                        size_bytes: 1_024 + u64::from(growth_index),
                        growth_index,
                        added_at: WallTime::from_unix_seconds(i64::from(growth_index)),
                        last_touched: WallTime::from_unix_seconds(i64::from(growth_index)),
                        class: FileClass::Ordinary,
                        language: None,
                    },
                );
                growth_index += 1;
            }
        }
        RepoTree {
            root: PathBuf::from("/repo"),
            files,
            worktrees: BTreeMap::new(),
            head: "0".repeat(40),
        }
    }

    /// A tree whose districts differ wildly in size — the case PRD §7.2's
    /// "a directory with 200 files pulls more road than one with 3" is about.
    fn lopsided_tree() -> RepoTree {
        let mut files = BTreeMap::new();
        let mut growth_index = 0_u32;
        for count in [200_usize, 3] {
            let district = format!("d{count:03}");
            for file in 0..count {
                let path = lp(&format!("{district}/f{file:04}.rs"));
                files.insert(
                    path.clone(),
                    FileMeta {
                        path,
                        size_bytes: 100,
                        growth_index,
                        added_at: WallTime::UNIX_EPOCH,
                        last_touched: WallTime::UNIX_EPOCH,
                        class: FileClass::Ordinary,
                        language: None,
                    },
                );
                growth_index += 1;
            }
        }
        RepoTree {
            root: PathBuf::from("/repo"),
            files,
            worktrees: BTreeMap::new(),
            head: "1".repeat(40),
        }
    }

    /// The standard fixture: a terrain field and a scatter for one tree.
    fn fixture(districts: usize, per_district: usize) -> (TerrainField, Vec<Attractor>) {
        let tree = synthetic_tree(districts, per_district);
        let extent = suggested_extent(&tree);
        let mut terrain = TerrainField::generate(fnv1a64(b"roads test fixture"), extent);
        // Bias a few districts, as `city` will, so the gradient branch is live.
        for district in 0..districts.min(8) {
            terrain.bias_district(
                &lp(&format!("crate{district:03}/src")),
                Point::new(
                    extent * 0.3 * (if district % 2 == 0 { 1.0 } else { -1.0 }),
                    extent * 0.2,
                ),
            );
        }
        let attractors = scatter_attractors(&tree, extent);
        (terrain, attractors)
    }

    fn flat() -> TerrainField {
        TerrainField::default()
    }

    /// A structural fingerprint of a graph, for cross-run and cross-process
    /// comparison. Bit-level on the raw `f32` bits, not tolerance-based.
    fn graph_digest(graph: &RoadGraph) -> u64 {
        use crate::determinism::combine_seeds;
        let mut digest = fnv1a64(b"polis road graph digest v1");
        digest = combine_seeds(digest, u64::from(graph.snap_tolerance.to_bits()));
        for node in &graph.nodes {
            digest = combine_seeds(digest, u64::from(node.position.x.to_bits()));
            digest = combine_seeds(digest, u64::from(node.position.y.to_bits()));
        }
        for segment in &graph.segments {
            digest = combine_seeds(digest, u64::from(segment.from.0));
            digest = combine_seeds(digest, u64::from(segment.to.0));
            digest = combine_seeds(digest, segment.class as u64);
        }
        digest
    }

    fn graphs_are_identical(left: &RoadGraph, right: &RoadGraph) -> bool {
        left.snap_tolerance.to_bits() == right.snap_tolerance.to_bits()
            && left.nodes.len() == right.nodes.len()
            && left.segments.len() == right.segments.len()
            && left.nodes.iter().zip(&right.nodes).all(|(a, b)| {
                a.position.x.to_bits() == b.position.x.to_bits()
                    && a.position.y.to_bits() == b.position.y.to_bits()
            })
            && left
                .segments
                .iter()
                .zip(&right.segments)
                .all(|(a, b)| a == b)
    }

    // -----------------------------------------------------------------------
    // Tunables and the direction table
    // -----------------------------------------------------------------------

    #[test]
    fn defaults_are_pinned_and_sane() {
        let params = GrowthParams::default();
        assert_eq!(params.kill_radius, 7.0);
        assert_eq!(params.snap_radius, 2.0);
        assert_eq!(params.segment_length, 10.0);
        assert_eq!(params.slope_threshold, 0.30);
        assert_eq!(params.max_steps, 24);
        assert_eq!(params.branches, 2);
        assert_eq!(params.influence_radius, 25.0);
        assert!(
            params.is_sane(),
            "a segment shorter than two snap radii never advances"
        );
        assert!(
            !params.grows_a_tree(),
            "one branch per attractor is a spanning tree, and trees read as \
             artificial (PRD §7.2)"
        );
        assert_eq!(params, params.sanitized());
    }

    /// The regression guard for the failure this module's design is built
    /// around: one branch per attractor is a greedy nearest-neighbour spanning
    /// tree, `segments == nodes - 1`, and not one cycle in the whole city.
    #[test]
    fn a_single_branch_per_attractor_really_does_grow_a_tree() {
        let (terrain, attractors) = fixture(8, 12);
        let params = GrowthParams {
            branches: 1,
            ..GrowthParams::default()
        };
        assert!(params.grows_a_tree());
        let stats = junction_stats(&grow(&terrain, &attractors, params, RoadClass::Street));
        assert!(stats.nodes > 50);
        assert!(
            stats.is_tree(),
            "the single-branch rule is documented as producing a tree; if this \
             now produces cycles the documentation on DEFAULT_BRANCHES is stale"
        );
    }

    #[test]
    fn insane_params_degrade_to_the_defaults() {
        let broken = GrowthParams {
            kill_radius: f32::NAN,
            snap_radius: -1.0,
            segment_length: 0.0,
            slope_threshold: f32::INFINITY,
            max_steps: 0,
            branches: 0,
            influence_radius: f32::NAN,
        };
        assert!(!broken.is_sane());
        assert_eq!(broken.sanitized(), GrowthParams::default());
        // A segment swallowed by its own snap radius is unsane but not repaired:
        // the caller asked for those two numbers and both are finite.
        let swallowed = GrowthParams {
            segment_length: 1.0,
            snap_radius: 2.0,
            ..GrowthParams::default()
        };
        assert!(!swallowed.is_sane());
    }

    /// [`crate::determinism`] rule 7: pin every generator whose output reaches
    /// the layout with literal expected values.
    #[test]
    fn the_direction_table_is_pinned() {
        assert_eq!(direction(0), Vec2::new(1.0, 0.0));
        assert_eq!(direction(64), Vec2::new(0.0, 1.0));
        assert_eq!(direction(128), Vec2::new(-1.0, 0.0));
        assert_eq!(direction(192), Vec2::new(0.0, -1.0));
        assert_eq!(direction(256), direction(0), "the index wraps");

        let mut digest = fnv1a64(b"road direction table");
        for index in 0..DIRECTIONS {
            let d = direction(index);
            assert!((d.length() - 1.0).abs() < 1e-6, "direction {index} is unit");
            digest = crate::determinism::combine_seeds(digest, u64::from(d.x.to_bits()));
            digest = crate::determinism::combine_seeds(digest, u64::from(d.y.to_bits()));
        }
        assert_eq!(
            digest, 0x1637_1e99_0480_457a,
            "the direction table moved; every golden layout file is invalidated \
             (determinism rule 7)"
        );
    }

    // -----------------------------------------------------------------------
    // The scatter
    // -----------------------------------------------------------------------

    #[test]
    fn the_scatter_is_deterministic_across_runs() {
        let tree = synthetic_tree(7, 9);
        let extent = suggested_extent(&tree);
        let first = scatter_attractors(&tree, extent);
        let second = scatter_attractors(&tree, extent);
        assert_eq!(first.len(), 63);
        for (a, b) in first.iter().zip(&second) {
            assert_eq!(a.at.x.to_bits(), b.at.x.to_bits());
            assert_eq!(a.at.y.to_bits(), b.at.y.to_bits());
            assert_eq!(a.weight.to_bits(), b.weight.to_bits());
        }
    }

    #[test]
    fn the_scatter_follows_git_growth_order() {
        // Path order and growth order disagree on purpose: `z/...` is the oldest
        // file, `a/...` the newest. The scatter must follow git, not the alphabet.
        let mut files = BTreeMap::new();
        for (index, name) in ["z/old.rs", "m/mid.rs", "a/new.rs"].iter().enumerate() {
            let path = lp(name);
            files.insert(
                path.clone(),
                FileMeta {
                    path,
                    size_bytes: 1,
                    growth_index: u32::try_from(index).unwrap(),
                    added_at: WallTime::UNIX_EPOCH,
                    last_touched: WallTime::UNIX_EPOCH,
                    class: FileClass::Ordinary,
                    language: None,
                },
            );
        }
        let tree = RepoTree {
            root: PathBuf::from("/repo"),
            files,
            worktrees: BTreeMap::new(),
            head: String::new(),
        };
        assert_eq!(
            files_in_growth_order(&tree)
                .iter()
                .map(|p| p.as_str())
                .collect::<Vec<_>>(),
            vec!["z/old.rs", "m/mid.rs", "a/new.rs"]
        );

        // The oldest file's district anchors the civic square; the newest sits
        // further out. That is PRD §7.1's old town, before a road exists.
        let scatter = scatter_attractors(&tree, suggested_extent(&tree));
        let radius = |a: &Attractor| a.at.distance(Point::ORIGIN);
        assert!(
            radius(&scatter[0]) < radius(&scatter[2]),
            "the oldest file must sit nearer the centre than the newest: {scatter:?}"
        );
    }

    #[test]
    fn scatter_density_and_weight_follow_the_repo_tree() {
        let tree = lopsided_tree();
        let scatter = scatter_attractors(&tree, suggested_extent(&tree));
        assert_eq!(scatter.len(), 203, "one attraction point per file");

        // The first 200 are the 200-file district, the last three the small one.
        let heavy = scatter[0].weight;
        let light = scatter[202].weight;
        assert!(
            heavy > light,
            "a 200-file directory must pull harder than a 3-file one: {heavy} vs {light}"
        );
        // sqrt(200) / mean(sqrt(200), sqrt(3)) = 14.14 / 7.94.
        assert!((heavy - 1.781).abs() < 1e-3, "{heavy}");
        assert_eq!(light, MIN_ATTRACTOR_WEIGHT, "clamped from 0.218");

        // And the weight becomes geometry: more roads converge on a file in the
        // busy district than on one in the quiet district.
        let params = GrowthParams::default();
        let heavy_branches = (f64::from(params.branches) * f64::from(heavy) + 0.5).floor();
        let light_branches = (f64::from(params.branches) * f64::from(light) + 0.5).floor();
        assert!(
            heavy_branches > light_branches,
            "{heavy_branches} vs {light_branches} branches"
        );
    }

    #[test]
    fn an_empty_tree_scatters_nothing() {
        let tree = RepoTree {
            root: PathBuf::from("/repo"),
            files: BTreeMap::new(),
            worktrees: BTreeMap::new(),
            head: String::new(),
        };
        assert!(scatter_attractors(&tree, 100.0).is_empty());
        assert!(suggested_extent(&tree) > 0.0);
    }

    #[test]
    fn the_scatter_stays_inside_the_extent_it_is_given() {
        let tree = synthetic_tree(12, 20);
        let squashed = scatter_attractors(&tree, 40.0);
        for attractor in &squashed {
            assert!(attractor.at.x.abs() <= 40.0 && attractor.at.y.abs() <= 40.0);
        }
    }

    // -----------------------------------------------------------------------
    // step_towards
    // -----------------------------------------------------------------------

    #[test]
    fn a_step_on_flat_ground_is_straight_and_one_segment_long() {
        let params = GrowthParams::default();
        let next = step_towards(Point::ORIGIN, Point::new(100.0, 0.0), &flat(), params);
        assert!((next.x - params.segment_length).abs() < 1e-4, "{next:?}");
        assert!(next.y.abs() < 1e-6);
    }

    #[test]
    fn a_step_never_overshoots_its_attractor() {
        let params = GrowthParams::default();
        let close = Point::new(2.0, 0.0);
        let next = step_towards(Point::ORIGIN, close, &flat(), params);
        assert_eq!(next, close, "the step is min(segment_length, distance)");
    }

    #[test]
    fn a_step_bends_with_the_gradient_on_a_slope() {
        // A field with real relief, sampled where it is steep.
        let terrain = TerrainField::generate(fnv1a64(b"slope fixture"), 200.0);
        let params = GrowthParams {
            slope_threshold: 0.0,
            ..GrowthParams::default()
        };
        let mut bent = 0;
        let mut checked = 0;
        for i in 0..64_i16 {
            let from = Point::new(f32::from(i) * 3.0 - 96.0, f32::from(i % 7) * 9.0 - 27.0);
            let target = from + Vec2::new(90.0, 30.0);
            if terrain.slope(from) <= 1e-4 {
                continue;
            }
            checked += 1;
            let straight = step_towards(from, target, &flat(), params);
            let sloped = step_towards(from, target, &terrain, params);
            if straight.distance(sloped) > 1e-3 {
                bent += 1;
            }
            // Bounded bend: the step still closes distance on the attractor.
            assert!(
                sloped.distance(target) < from.distance(target),
                "a bent step must still make progress"
            );
        }
        assert!(checked > 8, "the fixture must actually have slopes");
        assert_eq!(bent, checked, "every steep step must follow the gradient");
    }

    #[test]
    fn a_step_below_the_threshold_ignores_the_terrain() {
        let terrain = TerrainField::generate(fnv1a64(b"slope fixture"), 200.0);
        let params = GrowthParams {
            slope_threshold: 1.0e9,
            ..GrowthParams::default()
        };
        let from = Point::new(11.0, -7.0);
        let target = Point::new(140.0, 60.0);
        assert_eq!(
            step_towards(from, target, &terrain, params),
            step_towards(from, target, &flat(), params)
        );
    }

    #[test]
    fn a_degenerate_step_returns_where_it_started() {
        let params = GrowthParams::default();
        assert_eq!(
            step_towards(Point::ORIGIN, Point::ORIGIN, &flat(), params),
            Point::ORIGIN
        );
        // `Point` is `PartialEq`, and a NaN never equals itself, so this one is
        // compared bit for bit.
        let nan = Point::new(f32::NAN, 0.0);
        let out = step_towards(nan, Point::ORIGIN, &flat(), params);
        assert_eq!(out.x.to_bits(), nan.x.to_bits());
        assert_eq!(out.y.to_bits(), nan.y.to_bits());
        assert_eq!(
            step_towards(Point::ORIGIN, nan, &flat(), params),
            Point::ORIGIN
        );
    }

    // -----------------------------------------------------------------------
    // consume_reached
    // -----------------------------------------------------------------------

    #[test]
    fn consume_reached_retains_the_growth_order_of_the_survivors() {
        let mut attractors: Vec<Attractor> = (0..10_i16)
            .map(|i| Attractor::neutral(Point::new(f32::from(i) * 10.0, 0.0)))
            .collect();
        // Kills index 3 and 4 (at x = 30 and 40), leaving the rest in order.
        let consumed = consume_reached(&mut attractors, Point::new(35.0, 0.0), 6.0);
        assert_eq!(consumed, 2);
        let xs: Vec<f32> = attractors.iter().map(|a| a.at.x).collect();
        assert_eq!(xs, vec![0.0, 10.0, 20.0, 50.0, 60.0, 70.0, 80.0, 90.0]);
    }

    #[test]
    fn consume_reached_is_inclusive_at_exactly_the_kill_radius() {
        let mut at_radius = vec![Attractor::neutral(Point::new(5.0, 0.0))];
        assert_eq!(consume_reached(&mut at_radius, Point::ORIGIN, 5.0), 1);
        let mut outside = vec![Attractor::neutral(Point::new(5.001, 0.0))];
        assert_eq!(consume_reached(&mut outside, Point::ORIGIN, 5.0), 0);
    }

    #[test]
    fn consume_reached_consumes_nothing_on_a_degenerate_radius() {
        let mut attractors = vec![Attractor::neutral(Point::ORIGIN); 4];
        assert_eq!(consume_reached(&mut attractors, Point::ORIGIN, f32::NAN), 0);
        assert_eq!(
            attractors.len(),
            4,
            "a NaN radius must not consume the city"
        );
        let nan_point = Point::new(f32::NAN, f32::NAN);
        assert_eq!(consume_reached(&mut attractors, nan_point, 1.0), 0);
        // A zero radius still consumes an exact coincidence.
        assert_eq!(consume_reached(&mut attractors, Point::ORIGIN, 0.0), 4);
    }

    // -----------------------------------------------------------------------
    // Snapping — the organic signature
    // -----------------------------------------------------------------------

    /// The accelerated snap query must answer exactly what
    /// [`RoadGraph::snap_target`]'s linear scan answers, including the
    /// ties-to-lowest-index rule. Otherwise the index is not an index, it is a
    /// second city.
    #[test]
    fn snap_index_agrees_with_the_graphs_own_scan() {
        for tolerance in [0.0_f32, 0.5, 2.0, 9.0] {
            let mut growth = RoadGrowth::new(GrowthParams {
                snap_radius: tolerance,
                segment_length: tolerance.mul_add(2.0, 1.0),
                ..GrowthParams::default()
            });
            let mut rng = SeededRng::for_seed(0x5eed, "snap index fixture");
            for _ in 0..400 {
                let at = Point::new(rng.range_f32(-40.0, 40.0), rng.range_f32(-40.0, 40.0));
                growth.add_node_snapped(at);
            }
            let mut probe = SeededRng::for_seed(0x9999, "snap index probe");
            for _ in 0..2_000 {
                let at = Point::new(probe.range_f32(-45.0, 45.0), probe.range_f32(-45.0, 45.0));
                assert_eq!(
                    growth.snap_target(at),
                    growth.graph().snap_target(at),
                    "indexed snap disagrees with the scan at {at:?} (tolerance {tolerance})"
                );
            }
        }
    }

    #[test]
    fn snap_radius_boundary_behaviour() {
        let mut growth = RoadGrowth::new(GrowthParams {
            snap_radius: 2.0,
            ..GrowthParams::default()
        });
        let origin = growth.add_node_snapped(Point::ORIGIN);
        assert_eq!(growth.graph().nodes.len(), 1);

        // Exactly on the radius: snaps. `snap_target` tests `d² <= tolerance²`.
        assert_eq!(growth.add_node_snapped(Point::new(2.0, 0.0)), origin);
        assert_eq!(growth.graph().nodes.len(), 1);

        // A hair outside: a genuinely new node.
        let outside = growth.add_node_snapped(Point::new(2.001, 0.0));
        assert_ne!(outside, origin);
        assert_eq!(growth.graph().nodes.len(), 2);

        // Ties break to the lowest index: equidistant from node 0 and node 1.
        assert_eq!(
            growth.snap_target(Point::new(1.0005, 0.0)),
            Some(origin),
            "a tie must resolve to the lower node index"
        );
    }

    #[test]
    fn a_zero_snap_radius_only_merges_exact_coincidences() {
        let mut growth = RoadGrowth::new(GrowthParams {
            snap_radius: 0.0,
            ..GrowthParams::default()
        });
        let a = growth.add_node_snapped(Point::new(3.0, 4.0));
        assert_eq!(growth.add_node_snapped(Point::new(3.0, 4.0)), a);
        assert_ne!(growth.add_node_snapped(Point::new(3.000_001, 4.0)), a);
        assert_eq!(growth.graph().nodes.len(), 2);
    }

    #[test]
    fn a_huge_snap_radius_collapses_the_city_to_one_node() {
        let (terrain, attractors) = fixture(4, 6);
        let graph = grow(
            &terrain,
            &attractors,
            GrowthParams {
                snap_radius: 10_000.0,
                segment_length: 30_000.0,
                ..GrowthParams::default()
            },
            RoadClass::Street,
        );
        assert_eq!(graph.nodes.len(), 1, "everything snapped to the seed node");
        assert!(graph.segments.is_empty(), "a self-loop is never added");
    }

    /// The rule the module header states: the indexed builder must produce the
    /// same graph as one built only through [`RoadGraph::add_node_snapped`] and
    /// [`RoadGraph::add_segment`].
    #[test]
    fn indexed_growth_matches_the_naive_snapping_reference() {
        let (_, attractors) = fixture(5, 8);
        let params = GrowthParams::default();

        // The reference never uses an index: it is `RoadGraph`'s own linear snap
        // and duplicate scans, driven by the same sequence of points and node
        // pairs. If the index ever disagrees, the two cities separate here.
        let mut reference = RoadGraph::new(params.snap_radius);
        let mut indexed = RoadGrowth::new(params);
        let mut rng = SeededRng::for_seed(0xabc, "naive reference points");
        let mut previous: Option<NodeId> = None;
        for attractor in attractors.iter().take(150) {
            for step in 0..3_i16 {
                let jitter = Vec2::new(rng.range_f32(-4.0, 4.0), rng.range_f32(-4.0, 4.0));
                let at = attractor.at + jitter * f32::from(step);
                let expected = reference.add_node_snapped(at);
                let actual = indexed.add_node_snapped(at);
                assert_eq!(actual, expected, "snapping diverged at {at:?}");
                if let Some(previous) = previous {
                    let want = reference.add_segment(previous, expected, RoadClass::Street);
                    let got = indexed.commit_segment(previous, expected, RoadClass::Street);
                    assert_eq!(got, want, "duplicate rejection diverged");
                }
                previous = Some(expected);
            }
        }
        assert!(
            reference.nodes.len() > 100,
            "the fixture must build a network"
        );
        assert!(reference.segments.len() > 100);
        assert!(
            graphs_are_identical(indexed.graph(), &reference),
            "indexed growth diverged from the naive reference"
        );
    }

    // -----------------------------------------------------------------------
    // The organic signature, asserted
    // -----------------------------------------------------------------------

    /// **The test this module exists for.**
    ///
    /// A road graph with no cycles is a tree, and a tree reads as artificial —
    /// but it compiles, runs and renders, so nothing else catches it.
    #[test]
    fn the_grown_city_is_not_a_tree() {
        let (terrain, attractors) = fixture(14, 26);
        let mut growth = RoadGrowth::new(GrowthParams::default());
        let report = growth.extend(&terrain, &attractors, RoadClass::Street);
        let stats = growth.stats();
        println!(
            "nodes={} segments={} components={} cycles={} junctions={} deg>=4={} max_deg={} \
             degrees={:?}",
            stats.nodes,
            stats.segments,
            stats.components,
            stats.cycles,
            stats.junctions,
            stats.complex_junctions,
            stats.max_degree,
            stats.degrees
        );
        println!("{report:?}");
        assert_eq!(
            report.crossings_blocked, 0,
            "a blocked crossing is a hole in planarity"
        );
        assert_eq!(
            report.cascades_truncated, 0,
            "MAX_SPLITS_PER_SEGMENT is too low"
        );

        assert!(stats.nodes > 100, "the fixture must build a real network");
        assert!(
            !stats.is_tree(),
            "the road graph is a TREE ({} nodes, {} segments, {} components): \
             snapping is not producing junctions, and a tree reads as artificial \
             (PRD §7.2)",
            stats.nodes,
            stats.segments,
            stats.components
        );
        // Measured on this fixture: 0.38 cycles, 0.54 junctions and 0.24
        // degree-four-or-more nodes per node. The thresholds sit well below
        // that, so a tuning change has room to move the city without failing —
        // but a change that quietly flattens the network towards a tree does.
        assert!(
            stats.cycle_share() >= 0.15,
            "only {} cycles over {} nodes ({:.3} per node); the network is nearly a \
             tree",
            stats.cycles,
            stats.nodes,
            stats.cycle_share()
        );
        assert!(
            stats.junction_share() >= 0.30,
            "only {:.1}% of nodes are junctions",
            stats.junction_share() * 100.0
        );
        assert!(
            stats.complex_junction_share() >= 0.10,
            "only {:.1}% of nodes have degree >= 4; PRD §7.2's irregular four- and \
             five-way junctions are what the eye reads as 'grown'",
            stats.complex_junction_share() * 100.0
        );
        assert!(stats.max_degree >= 5, "no five-way junction anywhere");
    }

    /// The terrain field's only job is to give roads contours to follow
    /// (PRD §7.2 step 1). If the default [`GrowthParams::slope_threshold`] is
    /// never exceeded by a real field, that job is not being done and the
    /// gradient branch in [`step_towards`] is dead code nobody notices.
    #[test]
    fn the_terrain_actually_bends_the_roads_at_the_default_threshold() {
        let (terrain, attractors) = fixture(10, 20);
        let params = GrowthParams::default();
        let on_terrain = grow(&terrain, &attractors, params, RoadClass::Street);
        let on_flat = grow(&flat(), &attractors, params, RoadClass::Street);

        let steep = (0..400)
            .filter(|i| {
                let extent = terrain.extent();
                let at = Point::new(
                    extent * (f32::from(i16::try_from(i % 20).unwrap()) / 10.0 - 1.0),
                    extent * (f32::from(i16::try_from(i / 20).unwrap()) / 10.0 - 1.0),
                );
                terrain.slope(at) > params.slope_threshold
            })
            .count();
        println!("steep samples: {steep}/400");
        assert!(
            steep > 40,
            "only {steep}/400 samples exceed the default slope threshold; the \
             terrain is too flat to give roads contours to follow"
        );
        assert!(
            !graphs_are_identical(&on_terrain, &on_flat),
            "the same attractors produced the same city over a hill and over a \
             flat plain: step_towards is not following the gradient"
        );
    }

    /// The counterfactual for PRD §7.2's claim, measured rather than assumed —
    /// and the measurement is more interesting than the claim.
    ///
    /// Turning snapping off does **not** produce a tree here, because crossing
    /// resolution is a second, independent source of junctions: every segment
    /// that crosses another gains a degree-four node whether or not anything
    /// snapped. What snapping does is merge branches that converge on the same
    /// attractor, and the effect of losing it is dramatic in a different way —
    /// on the fixture city, 416 nodes become 2 285 for the same 200 attractors,
    /// because every convergence that should have been one junction becomes a
    /// handful of nodes a hair apart.
    ///
    /// Worth knowing for anyone who later reaches for `snap_radius` as a tuning
    /// knob: it does not control how tangled the network is, it controls how
    /// *coarse* the network is.
    #[test]
    fn snapping_merges_converging_branches_instead_of_piling_up_nodes() {
        let (terrain, attractors) = fixture(10, 20);
        let snapped = junction_stats(&grow(
            &terrain,
            &attractors,
            GrowthParams::default(),
            RoadClass::Street,
        ));
        let unsnapped = junction_stats(&grow(
            &terrain,
            &attractors,
            GrowthParams {
                snap_radius: 0.0,
                ..GrowthParams::default()
            },
            RoadClass::Street,
        ));
        println!(
            "snapped: {} nodes, {} cycles, {} deg>=4 | unsnapped: {} nodes, {} cycles, {} deg>=4",
            snapped.nodes,
            snapped.cycles,
            snapped.complex_junctions,
            unsnapped.nodes,
            unsnapped.cycles,
            unsnapped.complex_junctions
        );
        assert!(
            unsnapped.nodes > snapped.nodes * 3,
            "turning snapping off produced {} nodes against {}; if the two are \
             close, endpoints are not landing near each other and snapping is \
             not doing anything",
            unsnapped.nodes,
            snapped.nodes
        );
        assert!(
            snapped.cycle_share() > unsnapped.cycle_share(),
            "the snapped network should be tangled per node, not merely smaller: \
             {:.3} against {:.3} cycles per node",
            snapped.cycle_share(),
            unsnapped.cycle_share()
        );
        // Neither is a tree, and the fact that the unsnapped one is not is the
        // point of the doc comment above.
        assert!(!snapped.is_tree() && !unsnapped.is_tree());
    }

    #[test]
    fn junction_stats_counts_a_hand_built_graph() {
        let mut graph = RoadGraph::new(0.5);
        // A square with both diagonals: the centre crossing is not a node, so
        // this is four corners of degree 3 plus two crossing segments.
        let corners = [
            graph.add_node_snapped(Point::new(0.0, 0.0)),
            graph.add_node_snapped(Point::new(10.0, 0.0)),
            graph.add_node_snapped(Point::new(10.0, 10.0)),
            graph.add_node_snapped(Point::new(0.0, 10.0)),
        ];
        for i in 0..4 {
            graph.add_segment(corners[i], corners[(i + 1) % 4], RoadClass::Street);
        }
        graph.add_segment(corners[0], corners[2], RoadClass::Alley);
        graph.add_segment(corners[1], corners[3], RoadClass::Alley);

        let stats = junction_stats(&graph);
        assert_eq!(stats.nodes, 4);
        assert_eq!(stats.segments, 6);
        assert_eq!(stats.components, 1);
        assert_eq!(stats.cycles, 3, "E - V + C = 6 - 4 + 1");
        assert_eq!(stats.degrees.get(&3), Some(&4));
        assert_eq!(stats.junctions, 4);
        assert_eq!(stats.complex_junctions, 0);
        assert_eq!(stats.max_degree, 3);
        assert!(!stats.is_tree());

        // An empty graph and a lone node are both trees, and both must be
        // survivable rather than a division by zero.
        let empty = junction_stats(&RoadGraph::default());
        assert!(empty.is_tree());
        assert_eq!(empty.junction_share(), 0.0);
        assert_eq!(empty.complex_junction_share(), 0.0);
    }

    #[test]
    fn a_path_graph_is_correctly_identified_as_a_tree() {
        let mut graph = RoadGraph::new(0.5);
        let mut previous = graph.add_node_snapped(Point::ORIGIN);
        for i in 1..8_i16 {
            let next = graph.add_node_snapped(Point::new(f32::from(i) * 10.0, 0.0));
            graph.add_segment(previous, next, RoadClass::Street);
            previous = next;
        }
        let stats = junction_stats(&graph);
        assert!(stats.is_tree());
        assert_eq!(stats.cycles, 0);
        assert_eq!(stats.dead_ends, 2);
    }

    // -----------------------------------------------------------------------
    // Planarity
    // -----------------------------------------------------------------------

    #[test]
    fn crossings_finds_an_unresolved_x() {
        let mut graph = RoadGraph::new(0.5);
        let a = graph.add_node_snapped(Point::new(-10.0, 0.0));
        let b = graph.add_node_snapped(Point::new(10.0, 0.0));
        let c = graph.add_node_snapped(Point::new(0.0, -10.0));
        let d = graph.add_node_snapped(Point::new(0.0, 10.0));
        graph.add_segment(a, b, RoadClass::Street);
        graph.add_segment(c, d, RoadClass::Street);

        let found = crossings(&graph);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].a, 0);
        assert_eq!(found[0].b, 1);
        assert!(found[0].at.distance(Point::ORIGIN) < 1e-5);
        assert!(!is_planar(&graph));
    }

    #[test]
    fn crossings_ignores_segments_that_meet_at_a_node() {
        let mut graph = RoadGraph::new(0.5);
        let hub = graph.add_node_snapped(Point::ORIGIN);
        for index in 0..8_u32 {
            let arm = graph.add_node_snapped((direction(index * 32) * 10.0).to_point());
            graph.add_segment(hub, arm, RoadClass::Street);
        }
        assert!(
            is_planar(&graph),
            "a star is planar: {:?}",
            crossings(&graph)
        );
        assert_eq!(junction_stats(&graph).max_degree, 8);
    }

    #[test]
    fn an_unsplit_t_junction_is_reported_as_a_crossing() {
        let mut graph = RoadGraph::new(0.1);
        let a = graph.add_node_snapped(Point::new(-10.0, 0.0));
        let b = graph.add_node_snapped(Point::new(10.0, 0.0));
        let stem = graph.add_node_snapped(Point::new(0.0, 8.0));
        let foot = graph.add_node_snapped(Point::new(0.0, 0.0));
        graph.add_segment(a, b, RoadClass::Street);
        graph.add_segment(stem, foot, RoadClass::Street);
        assert!(
            !is_planar(&graph),
            "an endpoint sitting on another segment's interior breaks a face walk \
             exactly as a crossing does"
        );
    }

    /// The precondition `blocks` is handed.
    #[test]
    fn the_grown_city_is_planar() {
        let (terrain, attractors) = fixture(14, 26);
        let graph = grow(
            &terrain,
            &attractors,
            GrowthParams::default(),
            RoadClass::Street,
        );
        let found = crossings(&graph);
        assert!(
            found.is_empty(),
            "{} unresolved crossings; face extraction downstream is undefined. \
             First few: {:?}",
            found.len(),
            &found[..found.len().min(5)]
        );

        // Far tighter than the snap tolerance, to show the split points really do
        // land on the crossings rather than merely inside the contract's slack.
        let tight = crossings_with_tolerance(&graph, 0.05);
        println!("crossings within 0.05 of a node: {}", tight.len());
        assert!(
            tight.len() * 200 <= graph.segments.len(),
            "{} crossings survive a 0.05 tolerance over {} segments",
            tight.len(),
            graph.segments.len()
        );
    }

    #[test]
    fn growth_splits_a_segment_it_crosses() {
        // Two attractors placed so the second branch must cross the first road.
        let terrain = flat();
        let params = GrowthParams::default();
        let attractors = vec![
            Attractor::neutral(Point::new(60.0, 0.0)),
            Attractor::neutral(Point::new(0.0, 60.0)),
            Attractor::neutral(Point::new(30.0, -30.0)),
            Attractor::neutral(Point::new(30.0, 30.0)),
        ];
        let graph = grow(&terrain, &attractors, params, RoadClass::Street);
        assert!(is_planar(&graph), "{:?}", crossings(&graph));
        assert!(graph.segments.len() >= attractors.len());
    }

    // -----------------------------------------------------------------------
    // Incremental growth
    // -----------------------------------------------------------------------

    /// > Growth is genuinely incremental — a new file runs one growth step, it
    /// > does not regenerate the world. (PRD §7.4)
    ///
    /// The property that makes that claim true rather than aspirational.
    #[test]
    fn incremental_growth_equals_full_regrowth() {
        let (terrain, attractors) = fixture(10, 20);
        let params = GrowthParams::default();
        let full = grow(&terrain, &attractors, params, RoadClass::Street);
        assert!(full.segments.len() > 200, "the fixture must be a real city");

        for split in [
            0,
            1,
            7,
            33,
            attractors.len() / 3,
            attractors.len() / 2,
            attractors.len() - 1,
        ] {
            let mut incremental = grow(&terrain, &attractors[..split], params, RoadClass::Street);
            grow_incremental(
                &mut incremental,
                &terrain,
                &attractors[split..],
                params,
                RoadClass::Street,
            );
            assert!(
                graphs_are_identical(&incremental, &full),
                "a city grown in two parts (split at {split}) differs from one grown \
                 in one: {} vs {} nodes, {} vs {} segments",
                incremental.nodes.len(),
                full.nodes.len(),
                incremental.segments.len(),
                full.segments.len()
            );
        }
    }

    /// The real §7.4 shape: one file at a time, as commits land.
    #[test]
    fn one_attractor_at_a_time_equals_one_pass() {
        let (terrain, attractors) = fixture(4, 9);
        let params = GrowthParams::default();
        let full = grow(&terrain, &attractors, params, RoadClass::Street);

        let mut step_by_step = RoadGrowth::new(params);
        for attractor in &attractors {
            step_by_step.extend(&terrain, std::slice::from_ref(attractor), RoadClass::Street);
        }
        assert!(
            graphs_are_identical(step_by_step.graph(), &full),
            "growing one attractor at a time diverged from a single pass"
        );

        // And the same through `grow_incremental`, which throws the spatial
        // indices away and rebuilds them from the graph on every call. If a
        // rebuilt index ever differs from a live one, this is where it shows.
        let mut rebuilt = RoadGraph::new(params.snap_radius);
        for chunk in attractors.chunks(5) {
            grow_incremental(&mut rebuilt, &terrain, chunk, params, RoadClass::Street);
        }
        assert!(
            graphs_are_identical(&rebuilt, &full),
            "a graph grown through repeated index rebuilds diverged from one pass"
        );
    }

    #[test]
    fn an_incremental_step_keeps_the_graphs_own_snap_tolerance() {
        let (terrain, attractors) = fixture(3, 5);
        let mut graph = grow(
            &terrain,
            &attractors[..10],
            GrowthParams::default(),
            RoadClass::Street,
        );
        assert_eq!(graph.snap_tolerance, DEFAULT_SNAP_TOLERANCE);
        // A caller that reconstructed its params wrongly must not be able to snap
        // differently from the run that built this graph (PRD §7.2, §16).
        grow_incremental(
            &mut graph,
            &terrain,
            &attractors[10..],
            GrowthParams {
                snap_radius: 25.0,
                ..GrowthParams::default()
            },
            RoadClass::Street,
        );
        assert_eq!(graph.snap_tolerance, DEFAULT_SNAP_TOLERANCE);
    }

    #[test]
    fn growing_nothing_changes_nothing() {
        let (terrain, attractors) = fixture(3, 4);
        let params = GrowthParams::default();
        let mut graph = grow(&terrain, &attractors, params, RoadClass::Street);
        let before = graph_digest(&graph);
        assert_eq!(
            grow_incremental(&mut graph, &terrain, &[], params, RoadClass::Street),
            0
        );
        assert_eq!(graph_digest(&graph), before);

        assert!(grow(&terrain, &[], params, RoadClass::Street)
            .nodes
            .is_empty());
    }

    // -----------------------------------------------------------------------
    // Determinism
    // -----------------------------------------------------------------------

    #[test]
    fn two_runs_in_one_process_are_byte_identical() {
        let (terrain, attractors) = fixture(6, 11);
        let params = GrowthParams::default();
        let first = grow(&terrain, &attractors, params, RoadClass::Street);
        let second = grow(&terrain, &attractors, params, RoadClass::Street);
        assert!(graphs_are_identical(&first, &second));
        assert_eq!(graph_digest(&first), graph_digest(&second));
    }

    /// The only way a test suite can see a dependency on something randomised
    /// per process — `RandomState`, ASLR-ordered pointers, an environment
    /// variable read three layers down.
    #[test]
    fn two_fresh_processes_are_identical() {
        let first = child_digest();
        let second = child_digest();
        assert_eq!(first, second, "two child processes disagree");
        assert_eq!(
            first,
            reference_digest(),
            "a child process disagrees with this one"
        );
    }

    fn reference_digest() -> u64 {
        let (terrain, attractors) = fixture(5, 10);
        graph_digest(&grow(
            &terrain,
            &attractors,
            GrowthParams::default(),
            RoadClass::Street,
        ))
    }

    fn child_digest() -> u64 {
        let exe = std::env::current_exe().expect("test binary path");
        let output = Command::new(exe)
            .args([
                "--exact",
                "roads::tests::print_reference_digest",
                "--ignored",
                "--nocapture",
            ])
            .output()
            .expect("re-invoke the test binary");
        assert!(
            output.status.success(),
            "child process failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        let line = stdout
            .lines()
            .find_map(|line| line.strip_prefix("POLIS_ROADS_DIGEST="))
            .unwrap_or_else(|| panic!("child printed no digest:\n{stdout}"));
        u64::from_str_radix(line.trim(), 16).expect("hex digest")
    }

    /// The child half of [`two_fresh_processes_are_identical`].
    #[test]
    #[ignore = "child process of two_fresh_processes_are_identical"]
    fn print_reference_digest() {
        println!("POLIS_ROADS_DIGEST={:016x}", reference_digest());
    }

    // -----------------------------------------------------------------------
    // Robustness and performance
    // -----------------------------------------------------------------------

    #[test]
    fn pathological_attractors_terminate() {
        let params = GrowthParams::default();
        // Every attractor on the same point, plus non-finite ones.
        let mut attractors = vec![Attractor::neutral(Point::new(100.0, 100.0)); 200];
        attractors.push(Attractor::neutral(Point::new(f32::NAN, 0.0)));
        attractors.push(Attractor::new(Point::ORIGIN, f32::INFINITY));
        attractors.push(Attractor::new(Point::new(1e30, 1e30), 1.0));
        let graph = grow(&flat(), &attractors, params, RoadClass::Street);
        for node in &graph.nodes {
            assert!(node.position.is_finite(), "{node:?}");
        }
        assert!(is_planar(&graph));
    }

    #[test]
    fn growth_reports_what_it_did() {
        let (terrain, attractors) = fixture(5, 10);
        let mut growth = RoadGrowth::new(GrowthParams::default());
        let report = growth.extend(&terrain, &attractors, RoadClass::Arterial);
        assert!(report.attractors_processed > 0);
        assert!(report.segments_added > 0);
        assert!(report.nodes_added > 0);
        assert!(report.steps >= report.segments_added - report.crossings_resolved);
        assert!(
            report.attractors_processed + report.attractors_consumed <= attractors.len(),
            "every attractor is either processed or consumed, never both"
        );
        assert!(growth
            .graph()
            .segments
            .iter()
            .all(|s| s.class == RoadClass::Arterial));
        assert_eq!(growth.stats().segments, growth.graph().segments.len());
    }

    /// PRD §13.1: cold start under 3 s for a 5 000-file repo, and an incremental
    /// step under 50 ms off-thread. Timing is not a determinism assertion, so the
    /// bounds are generous and the measured numbers are printed — a regression
    /// shows up as a number, not only as a failure.
    #[test]
    fn a_five_thousand_file_repo_grows_within_budget() {
        use std::time::Instant;

        let tree = synthetic_tree(50, 100);
        assert_eq!(tree.files.len(), 5_000);
        let extent = suggested_extent(&tree);

        let scatter_start = Instant::now();
        let attractors = scatter_attractors(&tree, extent);
        let scatter_ms = scatter_start.elapsed().as_secs_f64() * 1000.0;

        let mut terrain = TerrainField::generate(fnv1a64(b"5k benchmark"), extent);
        for district in 0..50 {
            terrain.bias_district(
                &lp(&format!("crate{district:03}/src")),
                Point::new(extent * 0.4, extent * -0.2),
            );
        }

        let grow_start = Instant::now();
        let mut growth = RoadGrowth::new(GrowthParams::default());
        let report = growth.extend(&terrain, &attractors, RoadClass::Street);
        let grow_ms = grow_start.elapsed().as_secs_f64() * 1000.0;

        let stats = growth.stats();
        let graph = growth.into_graph();

        // One more file lands: the §7.4 incremental step, index rebuild included.
        let newcomer = [Attractor::new(
            Point::new(extent * 0.61, extent * 0.13),
            1.0,
        )];
        let mut incremental = graph.clone();
        let step_start = Instant::now();
        grow_incremental(
            &mut incremental,
            &terrain,
            &newcomer,
            GrowthParams::default(),
            RoadClass::Street,
        );
        let step_ms = step_start.elapsed().as_secs_f64() * 1000.0;

        let planarity_start = Instant::now();
        let unresolved = crossings(&graph);
        let planarity_ms = planarity_start.elapsed().as_secs_f64() * 1000.0;

        println!(
            "5k-file repo: extent={extent:.0} attractors={} scatter={scatter_ms:.1}ms \
             grow={grow_ms:.1}ms incremental_step={step_ms:.2}ms planarity_check={planarity_ms:.1}ms",
            attractors.len()
        );
        println!(
            "  nodes={} segments={} cycles={} junctions={} deg>=4={} max_deg={} unresolved={}",
            stats.nodes,
            stats.segments,
            stats.cycles,
            stats.junctions,
            stats.complex_junctions,
            stats.max_degree,
            unresolved.len()
        );
        println!("  {report:?}");

        assert!(unresolved.is_empty());
        assert_eq!(report.crossings_blocked, 0);
        assert_eq!(report.cascades_truncated, 0);
        assert!(!stats.is_tree());
        assert!(
            scatter_ms + grow_ms < 3_000.0,
            "cold-start road growth took {:.0} ms; PRD §13.1 budgets 3 s for the \
             whole first frame",
            scatter_ms + grow_ms
        );
        assert!(
            step_ms < 50.0,
            "an incremental step took {step_ms:.1} ms against PRD §13.1's 50 ms"
        );
    }
}
