//! Step 4 — lots (PRD §7.2), and the vacancy model (PRD §7.5).
//!
//! > **Lots** by recursive subdivision of each block along its longest axis
//! > until lot area falls under target. Irregular blocks give irregular lots for
//! > free.
//!
//! # Why the longest axis, and why that is enough
//!
//! Splitting perpendicular to the longest edge is what keeps parcels from
//! degenerating into slivers as the recursion deepens, and it is why no
//! aspect-ratio heuristic is needed on top. [`crate::Polygon::longest_edge`]
//! breaks ties towards the lowest index precisely so this recursion is
//! reproducible; a tie decided by floating-point noise subdivides one block two
//! different ways on two machines and fails PRD §16.
//!
//! # The split position is drawn, not centred
//!
//! A centred split gives a suspiciously regular grid inside every block. The
//! offset comes from a path-seeded [`crate::determinism::SeededRng`] — seeded
//! from the *block*, not from wall clock and not from a shared stream — so the
//! same block always subdivides the same way (PRD §7.4).
//!
//! # Termination is proved, not hoped for
//!
//! A block boundary comes out of a face walk on a grown road graph. It can be
//! deeply concave, it can touch itself at a snapped junction, and it can be a
//! sliver with three near-collinear corners. None of those may hang a pipeline
//! stage or blow the stack, so the recursion is bounded four independent ways
//! and *any one of them alone* terminates it:
//!
//! 1. The recursion is an **explicit work stack**, not a call stack. A
//!    pathological block cannot overflow anything.
//! 2. [`MAX_SUBDIVISION_DEPTH`] caps the tree depth.
//! 3. [`MAX_LOTS_PER_BLOCK`] caps the leaf count.
//! 4. [`split_once`] refuses any cut that fails to make progress — either piece
//!    below [`MIN_LOT_AREA`], or either piece not measurably smaller than its
//!    parent — and a refused cut makes the parcel a leaf.
//!
//! A refused split is a normal outcome, not an error: it is what makes an
//! irregular block give irregular lots.
//!
//! # Concave blocks and the cut line
//!
//! Cutting a concave polygon with a line can produce more than two pieces. The
//! half-plane clip here returns exactly two, joined along the cut where a
//! multi-piece answer would have separated them — the standard
//! Sutherland–Hodgman behaviour. The **areas are exact** (the connecting run
//! lies on the cut line and contributes nothing to the shoelace sum), and the
//! next split along the new longest axis separates the lobes. Buildings are lots
//! inset by a setback (PRD §7.2 step 5), and a zero-width join insets away to
//! nothing, so the artefact never reaches the screen.
//!
//! # Files and lots never match, and they fail in opposite directions
//!
//! Both mismatches happen on a real repository, and both are silent.
//!
//! **More files than lots.** Densification, ancestor hosting and shared parcels,
//! in that order — see [`plan`] for the whole policy. **Silently dropping
//! buildings is the worst possible answer**, so a file that ends up without a
//! lot of its own is handed back in [`LotPlan::overflow`], never discarded, and
//! [`LotReport::files`] is an accounting identity the test suite asserts.
//!
//! **Far more lots than files** is the quieter failure and needs no error path,
//! only a constant. Road growth on a repository-sized tree leaves well under one
//! file per block, so a fixed target area emits several empty parcels for every
//! building: a map that should read from across the room reads as a survey plan,
//! and PRD §16's golden file fills with geometry nobody stands on.
//! [`target_area_for`] scales the target *up* to [`MAX_LOT_AREA`] for a sparse
//! district, so a quiet directory comes out as large plots rather than as a
//! field of surveyed emptiness.
//!
//! # Deletion leaves a vacant lot (PRD §7.5)
//!
//! > Deleted files leave **vacant lots** that go to seed over time. Files
//! > untouched for a long window grow **overgrowth**. Both are free
//! > consequences of tracking `last_touched`, and together they make dead code
//! > visible without anyone running an analysis.
//!
//! [`Lot::occupant`] alone cannot express that — `None` is equally "nobody ever
//! built here". [`VacancyLedger`] carries the difference: which lot, whose
//! building it was, and when it went. [`Vacancy::seed_progress`] turns that into
//! the `0..1` the renderer needs, from a `now` the **caller** supplies — nothing
//! in `polis-layout` may read a clock (PRD §7.4).

use std::collections::{BTreeMap, BTreeSet};

use polis_events::{LogicalPath, WallTime};
use polis_repo::RepoTree;
use serde::{Deserialize, Serialize};

use crate::blocks::{district_of, group_by_district};
use crate::determinism::{combine_seeds, narrow, seed_for_path, SeededRng};
use crate::{Block, BlockId, Lot, LotId, Point, Polygon};

// ---------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------

/// Target lot area. Subdivision stops when a parcel falls under this.
///
/// Layout-visible, so it is a constant here rather than a caller's parameter.
/// This is the **default** target — what [`subdivide`] is asked for when nobody
/// has a better number. [`target_area_for`] moves it in both directions to suit
/// the district: down towards [`MIN_LOT_AREA`] where there are more files than
/// this would house, up towards [`MAX_LOT_AREA`] where there are far fewer.
pub const TARGET_LOT_AREA: f32 = 12.0;

/// Minimum lot area. A parcel below this is discarded rather than emitted —
/// PRD §7.3 clamps footprints to `[min_lot, block_area * 0.6]`, and a lot too
/// small to hold `min_lot` cannot hold a building.
pub const MIN_LOT_AREA: f32 = 2.0;

/// Largest lot [`target_area_for`] will ask for, in a district with far more
/// block than files.
///
/// One typical block: [`crate::roads::DEFAULT_SEGMENT_LENGTH`] squared. Past
/// that a "lot" stops meaning anything — it is the block — so a quiet directory
/// bottoms out at roughly one building per block rather than at one building per
/// eight surveyed-but-empty parcels.
///
/// Without this ceiling the sparse direction is the more damaging of the two.
/// Road growth on a real repository leaves well under one file per block, so a
/// fixed target puts seven empty parcels on screen for every building, bloats
/// PRD §16's serialized golden file with geometry nobody stands on, and makes a
/// map that is supposed to be readable from across the room read as a survey
/// plan. Overflow is loud; over-subdivision is quiet, which is why it needs a
/// constant rather than good intentions.
pub const MAX_LOT_AREA: f32 =
    crate::roads::DEFAULT_SEGMENT_LENGTH * crate::roads::DEFAULT_SEGMENT_LENGTH;

/// Deepest the subdivision tree may go — `2^14` parcels from one block, before
/// the other three bounds bite.
///
/// A depth cap alone would be enough to terminate; it is here so that a block
/// whose geometry defeats the area test still finishes in bounded time rather
/// than in bounded-but-astronomical time.
pub const MAX_SUBDIVISION_DEPTH: u32 = 14;

/// Most parcels one block may produce.
///
/// A block with a thousand lots is already past the point where an individual
/// building is legible; a block trying for a hundred thousand is a bug in the
/// area test, and this is what stops that bug from becoming a hang.
pub const MAX_LOTS_PER_BLOCK: usize = 4096;

/// Narrowest fraction of the split axis a cut may take.
///
/// The cut lands in `[MIN_SPLIT_FRACTION, 1 - MIN_SPLIT_FRACTION]` of the
/// parcel's extent along its longest axis, so every child keeps at least this
/// much of the parent's span. Drawn rather than centred — a centred split gives
/// a suspiciously regular grid inside every block — but bounded, because a cut
/// at 2 % of the span is a sliver dressed up as variety.
pub const MIN_SPLIT_FRACTION: f32 = 0.35;

/// Mean leaf area as a fraction of the target, measured over grown blocks.
///
/// The recursion stops when a parcel is at or under the target, so leaves land
/// between roughly half the target and the target itself. Turning "how many lots
/// do I want" into "what target area do I ask for" needs this number, and
/// guessing it wrong is the difference between a district that houses its files
/// and one that overflows. Pinned by `the_lot_yield_matches_the_estimate`.
pub const LEAF_FILL: f32 = 0.72;

/// How many lots a district aims for per file.
///
/// Slack is not waste: an empty parcel reads as open ground, and PRD §7.5 wants
/// vacancy to be *visible*. It also leaves somewhere for the next file to be
/// built without re-subdividing the district, which is what makes
/// [`VacancyLedger::settle`] agree with a full [`plan`].
pub const LOT_SLACK: f32 = 1.15;

/// Days a vacant lot takes to go fully to seed (PRD §7.5).
///
/// Twice PRD §8's 90-day overgrowth window: a deleted file's plot should not
/// reach full overgrowth faster than a merely neglected one.
pub const SEED_WINDOW_DAYS: u32 = 180;

// ---------------------------------------------------------------------------
// Subdivision
// ---------------------------------------------------------------------------

/// Subdivides one block into lots.
///
/// The split axis is the block's longest, and the split position is drawn from a
/// block-seeded generator, so the same block always subdivides the same way
/// (PRD §7.4). Lots come back with [`Lot::occupant`] as `None`; assignment is
/// [`assign`]'s job, because it needs the growth order.
///
/// Parcels come back in binary-heap order over the subdivision tree — root
/// `1`, children `2n` and `2n + 1`, low side first — which is a total order
/// independent of the order the work stack happened to pop them in.
#[must_use]
pub fn subdivide(block: &Block, target_area: f32, seed: u64) -> Vec<Polygon> {
    subdivide_boundary(&block.boundary, target_area, seed)
}

/// [`subdivide`] for a bare polygon.
///
/// Split out so the recursion stays a pure geometry operation with no opinion
/// about blocks, districts or ids — which is what makes it testable against a
/// hand-built pathological shape.
#[must_use]
pub fn subdivide_boundary(boundary: &Polygon, target_area: f32, seed: u64) -> Vec<Polygon> {
    let target = if target_area.is_finite() {
        target_area.max(MIN_LOT_AREA)
    } else {
        TARGET_LOT_AREA
    };
    let mut leaves: Vec<(u64, Polygon)> = Vec::new();
    // `(node id in the subdivision tree, depth, parcel)`. An explicit stack, so
    // a pathological block cannot overflow the call stack.
    let mut stack: Vec<(u64, u32, Polygon)> = vec![(1, 0, boundary.clone())];
    while let Some((node, depth, parcel)) = stack.pop() {
        if !parcel.is_valid() || parcel.area() < MIN_LOT_AREA {
            continue;
        }
        if depth >= MAX_SUBDIVISION_DEPTH
            || leaves.len() + stack.len() + 1 >= MAX_LOTS_PER_BLOCK
            || parcel.area() <= target
        {
            leaves.push((node, parcel));
            continue;
        }
        // One stream per tree node, derived from the node's position in the
        // tree rather than from a running counter, so the draw does not depend
        // on the order the stack is worked through.
        let mut draw = SeededRng::for_seed(combine_seeds(seed, node), "lot split");
        let offset = draw.range_f32(MIN_SPLIT_FRACTION, 1.0 - MIN_SPLIT_FRACTION);
        if let Some((low, high)) = split_once(&parcel, offset) {
            stack.push((node * 2 + 1, depth + 1, high));
            stack.push((node * 2, depth + 1, low));
        } else {
            leaves.push((node, parcel));
        }
    }
    leaves.sort_by_key(|(node, _)| *node);
    leaves.into_iter().map(|(_, parcel)| parcel).collect()
}

/// Splits one polygon in two along a line perpendicular to its longest edge.
///
/// `offset` is the **fraction** of the parcel's extent along that axis at which
/// to cut, clamped into
/// `[MIN_SPLIT_FRACTION, 1 - MIN_SPLIT_FRACTION]`; a non-finite offset cuts at
/// the midpoint. The low-side piece — the one on the negative side of the cut,
/// towards the longest edge's start — comes back first, always, so the two
/// children of a node are distinguishable without looking at their geometry.
///
/// Returns `None` when the polygon is degenerate or the cut produces a piece
/// below [`MIN_LOT_AREA`] — a normal outcome that ends the recursion, not an
/// error — and also when a cut fails to shrink either child measurably, which
/// is the progress guarantee the termination argument rests on.
#[must_use]
pub fn split_once(polygon: &Polygon, offset: f32) -> Option<(Polygon, Polygon)> {
    if !polygon.is_valid() {
        return None;
    }
    let parent_area = polygon.area();
    if parent_area < MIN_LOT_AREA * 2.0 {
        return None;
    }
    let (edge, _) = polygon.longest_edge()?;
    let count = polygon.vertices.len();
    let start = polygon.vertices[edge];
    let end = polygon.vertices[(edge + 1) % count];

    // The cut's normal is the longest edge's direction: cutting *perpendicular*
    // to the long axis is what halves the long dimension. Arithmetic in `f64`
    // with one rounding at the end (`determinism` rule 3).
    let (dx, dy) = (
        f64::from(end.x) - f64::from(start.x),
        f64::from(end.y) - f64::from(start.y),
    );
    let length = (dx * dx + dy * dy).sqrt();
    if !length.is_finite() || length <= 0.0 {
        return None;
    }
    let (ux, uy) = (dx / length, dy / length);

    let projected: Vec<f64> = polygon
        .vertices
        .iter()
        .map(|p| {
            (f64::from(p.x) - f64::from(start.x)) * ux + (f64::from(p.y) - f64::from(start.y)) * uy
        })
        .collect();
    let (low, high) = projected
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| {
            (lo.min(*v), hi.max(*v))
        });
    let span = high - low;
    if !span.is_finite() || span <= 0.0 {
        return None;
    }
    let fraction = if offset.is_finite() {
        f64::from(offset.clamp(MIN_SPLIT_FRACTION, 1.0 - MIN_SPLIT_FRACTION))
    } else {
        0.5
    };
    let cut = low + span * fraction;

    let signed: Vec<f64> = projected.iter().map(|v| v - cut).collect();
    let mut below: Vec<Point> = Vec::with_capacity(count + 2);
    let mut above: Vec<Point> = Vec::with_capacity(count + 2);
    for i in 0..count {
        let j = (i + 1) % count;
        let (here, next) = (signed[i], signed[j]);
        if here <= 0.0 {
            below.push(polygon.vertices[i]);
        }
        if here >= 0.0 {
            above.push(polygon.vertices[i]);
        }
        if (here < 0.0 && next > 0.0) || (here > 0.0 && next < 0.0) {
            let t = here / (here - next);
            let crossing = interpolate(polygon.vertices[i], polygon.vertices[j], t);
            below.push(crossing);
            above.push(crossing);
        }
    }

    let below = Polygon::new(dedupe_ring(below));
    let above = Polygon::new(dedupe_ring(above));
    if !viable(&below, parent_area) || !viable(&above, parent_area) {
        return None;
    }
    Some((below, above))
}

/// A piece is usable when it can hold a building and is measurably smaller than
/// what it came from.
///
/// The second half is the progress guarantee: without it a cut that every vertex
/// happens to land on one side of returns the parent unchanged, and the
/// recursion re-splits the same shape until a depth cap saves it.
fn viable(piece: &Polygon, parent_area: f32) -> bool {
    if !piece.is_valid() {
        return false;
    }
    let area = piece.area();
    area >= MIN_LOT_AREA && area <= parent_area * 0.999
}

/// A point `t` of the way from `a` to `b`, in `f64` with one rounding.
fn interpolate(a: Point, b: Point, t: f64) -> Point {
    let t = t.clamp(0.0, 1.0);
    Point::new(
        narrow(f64::from(a.x) + (f64::from(b.x) - f64::from(a.x)) * t),
        narrow(f64::from(a.y) + (f64::from(b.y) - f64::from(a.y)) * t),
    )
}

/// Drops consecutive duplicate points, including across the closing edge.
///
/// A vertex sitting exactly on the cut line is emitted to both pieces and is
/// also a crossing candidate, so duplicates are routine rather than
/// exceptional. A repeated vertex is not wrong — the area survives it — but it
/// wastes a subdivision level and shows up in the golden file.
fn dedupe_ring(points: Vec<Point>) -> Vec<Point> {
    let mut out: Vec<Point> = Vec::with_capacity(points.len());
    for point in points {
        if out.last().is_some_and(|last| *last == point) {
            continue;
        }
        out.push(point);
    }
    while out.len() > 1 && out[0] == out[out.len() - 1] {
        out.pop();
    }
    out
}

/// Assigns identity and parentage to freshly subdivided parcels.
///
/// Separate from [`subdivide`] so the recursion can stay a pure polygon
/// operation and the [`LotId`] numbering can stay a single monotonically
/// increasing sequence over the whole city — which is what makes a lot index
/// stable between an incremental step and a full regeneration.
#[must_use]
pub fn number(parcels: Vec<Polygon>, block: BlockId, next_id: &mut u32) -> Vec<Lot> {
    parcels
        .into_iter()
        .map(|boundary| {
            let id = LotId(*next_id);
            *next_id = next_id.saturating_add(1);
            Lot {
                id,
                block,
                boundary,
                occupant: None,
            }
        })
        .collect()
}

/// The subdivision seed for one block (PRD §7.4).
///
/// Derived from the block's **district** and its index, never from a shared
/// stream: adding a draw elsewhere in the pipeline cannot reshuffle a block's
/// parcels, and two blocks in the same district subdivide differently.
#[must_use]
pub fn block_seed(block: &Block) -> u64 {
    combine_seeds(
        seed_for_path(&block.district, "block subdivision"),
        u64::from(block.id.0),
    )
}

/// The target lot area that houses `files` files in `block_area` of block.
///
/// Subdivision stops at the target, so leaves land between roughly half the
/// target and the target itself — [`LEAF_FILL`] of it on average. Wanting
/// [`LOT_SLACK`] lots per file therefore means asking for
/// `block_area / (files × LEAF_FILL × LOT_SLACK)`, clamped to
/// `[MIN_LOT_AREA, MAX_LOT_AREA]`.
///
/// The clamp is where both interesting cases live, and they fail in opposite
/// directions:
///
/// * **More files than the district can house** hits the [`MIN_LOT_AREA`]
///   floor. The surplus is handled by [`plan`]'s overflow policy rather than by
///   parcels too small to build on.
/// * **Far more block than files** hits the [`MAX_LOT_AREA`] ceiling and comes
///   out as large plots with room around each building — a quiet corner of
///   town, which is what a small directory should read as. Left unclamped at the
///   other end (a fixed [`TARGET_LOT_AREA`] for every district) the same
///   repository produces seven empty parcels per building, which is a survey
///   plan rather than a city.
#[must_use]
pub fn target_area_for(block_area: f32, files: usize) -> f32 {
    if files == 0 || !block_area.is_finite() || block_area <= 0.0 {
        return TARGET_LOT_AREA;
    }
    #[allow(clippy::cast_precision_loss)] // a district's file count is far below 2^53
    let wanted = files as f64 * f64::from(LEAF_FILL) * f64::from(LOT_SLACK);
    narrow(f64::from(block_area) / wanted).clamp(MIN_LOT_AREA, MAX_LOT_AREA)
}

// ---------------------------------------------------------------------------
// Assignment (PRD §7.4)
// ---------------------------------------------------------------------------

/// A file that could not get a lot to itself.
///
/// Never a dropped building: an overflow entry is a file the caller still has to
/// draw, sharing a parcel with another. `shares` is `None` only when there was
/// no lot in the district at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Overflow {
    /// The file with nowhere of its own.
    pub path: LogicalPath,
    /// The lot it shares, or `None` when the district has no lots.
    pub shares: Option<LotId>,
}

/// Which file stands on which lot.
///
/// Produced by [`assign`] and consumed by [`plan`]. The `placed` map is the
/// product: the same file lands on the same lot on every machine and every run,
/// which is what makes spatial memory work.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Assignment {
    placed: BTreeMap<LogicalPath, LotId>,
    overflow: Vec<Overflow>,
    vacant: Vec<LotId>,
}

impl Assignment {
    /// File to lot, in path order.
    #[must_use]
    pub fn placed(&self) -> &BTreeMap<LogicalPath, LotId> {
        &self.placed
    }

    /// The lot a file stands on.
    #[must_use]
    pub fn lot_of(&self, path: &LogicalPath) -> Option<LotId> {
        self.placed.get(path).copied()
    }

    /// Files that had to share, in the order they were processed.
    #[must_use]
    pub fn overflow(&self) -> &[Overflow] {
        &self.overflow
    }

    /// Lots nobody was assigned to, in id order. Undeveloped ground, not
    /// [`Vacancy`] — nothing was ever built here.
    #[must_use]
    pub fn unoccupied(&self) -> &[LotId] {
        &self.vacant
    }

    /// Files that got a lot of their own.
    #[must_use]
    pub fn len(&self) -> usize {
        self.placed.len()
    }

    /// True when nothing was placed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.placed.is_empty()
    }
}

/// Assigns files to lots, deterministically and independently of input order.
///
/// `files` is `(logical path, growth index)` in **any** order: the first thing
/// this does is sort them into `(growth index, path)`, so a caller that hands
/// over a `HashMap`'s iteration order gets the same city as one that hands over
/// a sorted `Vec`. That is the whole point of PRD §7.4, and it is enforced here
/// rather than trusted upstream.
///
/// # The rule
///
/// Each file's *home* slot is `hash(path) % lots`, and it takes the first free
/// lot at or after its home, wrapping. Hashing rather than ranking is
/// deliberate: a rank-ordered assignment moves every later file when one file is
/// inserted or deleted, which destroys exactly the spatial memory the product
/// is for. A hashed assignment moves nobody, and PRD §7.5's vacant lot is what
/// a deletion leaves behind instead of a reshuffle.
///
/// The wrap is why the *free* set is searched rather than the lot list: with
/// [`LOT_SLACK`] worth of headroom the search is a step or two, and it degrades
/// to a scan rather than a failure when a district is completely full.
#[must_use]
pub fn assign(files: &[(LogicalPath, u32)], lots: &[LotId]) -> Assignment {
    let mut ids: Vec<LotId> = lots.to_vec();
    ids.sort_unstable();
    ids.dedup();

    let mut ordered: Vec<(&LogicalPath, u32)> =
        files.iter().map(|(path, growth)| (path, *growth)).collect();
    ordered.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(b.0)));

    let mut free: BTreeSet<usize> = (0..ids.len()).collect();
    let mut seen: BTreeSet<&LogicalPath> = BTreeSet::new();
    let mut assignment = Assignment::default();
    for (path, _) in ordered {
        if !seen.insert(path) {
            continue;
        }
        if ids.is_empty() {
            assignment.overflow.push(Overflow {
                path: path.clone(),
                shares: None,
            });
            continue;
        }
        let home = home_slot(path, ids.len());
        if let Some(slot) = claim(&mut free, home) {
            assignment.placed.insert(path.clone(), ids[slot]);
        } else {
            // Every lot in the district is taken. The file still gets drawn —
            // dropping it is the one answer that is never acceptable — so it
            // shares its home lot with whoever got there first.
            assignment.overflow.push(Overflow {
                path: path.clone(),
                shares: Some(ids[home]),
            });
        }
    }
    assignment.vacant = free.into_iter().map(|slot| ids[slot]).collect();
    assignment
}

/// A file's home slot: `hash(path) % lots`, seeded from the logical path alone
/// (PRD §7.4).
fn home_slot(path: &LogicalPath, lots: usize) -> usize {
    if lots == 0 {
        return 0;
    }
    let modulus = u64::try_from(lots).unwrap_or(u64::MAX);
    usize::try_from(seed_for_path(path, "lot") % modulus).unwrap_or(0)
}

/// Takes the first free slot at or after `home`, wrapping to the front.
fn claim(free: &mut BTreeSet<usize>, home: usize) -> Option<usize> {
    let slot = free
        .range(home..)
        .next()
        .copied()
        .or_else(|| free.iter().next().copied())?;
    free.remove(&slot);
    Some(slot)
}

// ---------------------------------------------------------------------------
// Vacancy (PRD §7.5)
// ---------------------------------------------------------------------------

/// A lot whose building is gone (PRD §7.5).
///
/// The distinction [`Lot::occupant`] cannot make on its own: `None` there means
/// both "a file was deleted" and "nobody ever built here", and only the first
/// should grow weeds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Vacancy {
    /// The empty parcel.
    pub lot: LotId,
    /// The file that used to stand on it. Kept so the drill-down can say what
    /// was here, and so a re-added file can be recognised.
    pub former: LogicalPath,
    /// When it was vacated — the deleting commit's time, supplied by the
    /// caller. Nothing in `polis-layout` reads a clock (PRD §7.4).
    pub since: WallTime,
}

impl Vacancy {
    /// How far gone to seed, `0` at the moment of deletion and `1` after
    /// [`SEED_WINDOW_DAYS`].
    #[must_use]
    pub fn seed_progress(&self, now: WallTime) -> f32 {
        self.seed_progress_over(now, SEED_WINDOW_DAYS)
    }

    /// [`seed_progress`](Self::seed_progress) over a caller's window.
    ///
    /// A zero-length window is fully overgrown immediately, which is the useful
    /// reading rather than a division by zero.
    #[must_use]
    pub fn seed_progress_over(&self, now: WallTime, window_days: u32) -> f32 {
        if window_days == 0 {
            return 1.0;
        }
        let days = f64::from(self.since.days_until(now));
        narrow((days / f64::from(window_days)).clamp(0.0, 1.0))
    }

    /// True once the lot has fully gone to seed.
    #[must_use]
    pub fn is_overgrown(&self, now: WallTime) -> bool {
        self.seed_progress(now) >= 1.0
    }
}

/// Which lots were built on and then emptied, and when (PRD §7.5).
///
/// Lives beside [`crate::CityLayout::lots`] rather than inside [`Lot`], because
/// a vacancy is *history* about a lot and the lot itself is geometry. Keyed by
/// [`LotId`] in a `BTreeMap`, so iteration order can never reach the layout.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VacancyLedger {
    entries: BTreeMap<LotId, Vacancy>,
}

impl VacancyLedger {
    /// An empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Empties a lot and records why (PRD §7.5).
    ///
    /// The occupant is read off the lot rather than passed in, so the ledger and
    /// the geometry cannot disagree about who used to live there. Returns `None`
    /// for an unknown lot or one that was already empty.
    pub fn vacate(&mut self, lots: &mut [Lot], lot: LotId, when: WallTime) -> Option<&Vacancy> {
        let slot = lots.iter_mut().find(|candidate| candidate.id == lot)?;
        let former = slot.occupant.take()?;
        self.entries.insert(
            lot,
            Vacancy {
                lot,
                former,
                since: when,
            },
        );
        self.entries.get(&lot)
    }

    /// [`vacate`](Self::vacate) for a file whose lot the caller does not know.
    ///
    /// Scans, so prefer the [`LotId`] form when a `Building` is to hand — it
    /// carries one.
    pub fn vacate_path(
        &mut self,
        lots: &mut [Lot],
        path: &LogicalPath,
        when: WallTime,
    ) -> Option<LotId> {
        let lot = lots
            .iter()
            .find(|candidate| candidate.occupant.as_ref() == Some(path))?
            .id;
        self.vacate(lots, lot, when).map(|vacancy| vacancy.lot)
    }

    /// Builds a newly added file onto a free lot among `candidates`.
    ///
    /// Uses exactly [`assign`]'s rule over the same candidate list, so a file
    /// appended to the growth sequence settles on the lot a full re-plan would
    /// have given it — that equivalence is what PRD §7.4 means by "growth is
    /// genuinely incremental", and it holds as long as the lot set has not
    /// changed and the new file sorts last in growth order, which is what a new
    /// file does.
    ///
    /// A vacant lot is a candidate like any other: land goes back into use, and
    /// the [`Vacancy`] record is cleared when it does.
    pub fn settle(
        &mut self,
        lots: &mut [Lot],
        candidates: &[LotId],
        path: &LogicalPath,
    ) -> Option<LotId> {
        let mut ids: Vec<LotId> = candidates.to_vec();
        ids.sort_unstable();
        ids.dedup();
        if ids.is_empty() {
            return None;
        }
        let occupied: BTreeSet<LotId> = lots
            .iter()
            .filter(|lot| lot.occupant.is_some())
            .map(|lot| lot.id)
            .collect();
        let mut free: BTreeSet<usize> = ids
            .iter()
            .enumerate()
            .filter(|(_, id)| !occupied.contains(id))
            .map(|(slot, _)| slot)
            .collect();
        let slot = claim(&mut free, home_slot(path, ids.len()))?;
        let chosen = ids[slot];
        let lot = lots.iter_mut().find(|candidate| candidate.id == chosen)?;
        lot.occupant = Some(path.clone());
        self.entries.remove(&chosen);
        Some(chosen)
    }

    /// The record for one lot, if it has one.
    #[must_use]
    pub fn get(&self, lot: LotId) -> Option<&Vacancy> {
        self.entries.get(&lot)
    }

    /// Every vacancy, in lot order.
    pub fn iter(&self) -> impl Iterator<Item = &Vacancy> + '_ {
        self.entries.values()
    }

    /// Lots that have fully gone to seed by `now` (PRD §7.5) — dead code, made
    /// visible without anyone running an analysis.
    #[must_use]
    pub fn gone_to_seed(&self, now: WallTime) -> Vec<LotId> {
        self.entries
            .values()
            .filter(|vacancy| vacancy.is_overgrown(now))
            .map(|vacancy| vacancy.lot)
            .collect()
    }

    /// Forgets one lot's history — the lot reads as undeveloped again.
    pub fn forget(&mut self, lot: LotId) -> Option<Vacancy> {
        self.entries.remove(&lot)
    }

    /// How many lots are vacant.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when nothing has been vacated.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ---------------------------------------------------------------------------
// The whole-city plan
// ---------------------------------------------------------------------------

/// What [`plan`] did with every file in the tree.
///
/// The invariant worth asserting on is
/// `placed + overflow + massed + unplaced == files considered`. Every file is
/// accounted for in exactly one of them, because the one unacceptable answer to
/// "more files than lots" is a building that silently vanishes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LotReport {
    /// Blocks that were subdivided.
    pub blocks: usize,
    /// Parcels emitted.
    pub lots: usize,
    /// Files that got a lot to themselves.
    pub placed: usize,
    /// Files sharing another file's lot, because their district ran out.
    pub overflow: usize,
    /// Files deliberately given no lot: PRD §8's industrial zones are drawn as
    /// one mass, not as individual buildings.
    pub massed: usize,
    /// Files with no lot at all — only possible when the city has no blocks.
    pub unplaced: usize,
    /// Files housed by an ancestor district because their own has no block.
    pub hosted_by_ancestor: usize,
    /// Files housed by a district that is not an ancestor at all, because
    /// nothing up their chain has a block.
    pub displaced: usize,
    /// Districts with files but no block of their own.
    pub districts_without_blocks: usize,
    /// Lots nobody was assigned to. Open ground, not [`Vacancy`].
    pub unoccupied: usize,
}

impl LotReport {
    /// Every file the plan considered.
    #[must_use]
    pub fn files(&self) -> usize {
        self.placed + self.overflow + self.massed + self.unplaced
    }

    /// True when some district could not house its own files.
    #[must_use]
    pub fn is_overcrowded(&self) -> bool {
        self.overflow > 0 || self.unplaced > 0
    }
}

/// Lots for a whole city, with every file accounted for.
#[derive(Debug, Clone, Default)]
pub struct LotPlan {
    /// Parcels, indexed by [`LotId`], with occupants filled in.
    pub lots: Vec<Lot>,
    /// Lots of each block, in id order.
    pub by_block: BTreeMap<BlockId, Vec<LotId>>,
    /// Lots of each **host** district, in id order — the candidate list
    /// [`VacancyLedger::settle`] wants for a file added to that district later.
    pub by_district: BTreeMap<LogicalPath, Vec<LotId>>,
    /// Which host district each district's files were routed to. A district
    /// mapping to itself is the ordinary case.
    pub host_of: BTreeMap<LogicalPath, LogicalPath>,
    /// Files sharing a lot, or with none at all. Never dropped.
    pub overflow: Vec<Overflow>,
    /// Files given no lot on purpose (PRD §8, industrial zones), in path order.
    pub massed: Vec<LogicalPath>,
    /// What happened, in numbers.
    pub report: LotReport,
}

impl LotPlan {
    /// The lot a file stands on.
    #[must_use]
    pub fn lot_of(&self, path: &LogicalPath) -> Option<LotId> {
        self.lots
            .iter()
            .find(|lot| lot.occupant.as_ref() == Some(path))
            .map(|lot| lot.id)
    }
}

/// Subdivides every block and puts every file somewhere (PRD §7.2 step 4).
///
/// # The overflow policy, in order
///
/// A real repository *will* have a district with more files than its blocks can
/// comfortably hold, and silently dropping buildings is the worst possible
/// answer. Four steps, each one visible in [`LotReport`]:
///
/// 1. **Mass what should be massed.** PRD §8 renders `node_modules`, vendored,
///    generated and `target/` as *one* dull shape, not as individual buildings.
///    Those files ([`polis_repo::FileClass::is_massed`]) are deliberately given
///    no lot and are listed in [`LotPlan::massed`], which is what stops a
///    50 000-file dependency tree from being the problem in the first place.
/// 2. **Densify.** Each host district's target lot area is sized from its own
///    file count ([`target_area_for`]), down to [`MIN_LOT_AREA`]. A crowded
///    district subdivides finer *before* anyone is assigned, so overflow is a
///    genuinely-out-of-room condition rather than a consequence of a global
///    constant.
/// 3. **Host up the tree.** A district with files but no block of its own is
///    hosted by its nearest **ancestor** that has one — `src/auth`'s files land
///    in `src`, which is exactly where an operator looks for them. PRD §9: the
///    tree determines placement, and an ancestor is still the tree. Only when
///    nothing up the chain has a block does a file land somewhere unrelated, and
///    that is counted separately as [`LotReport::displaced`].
/// 4. **Share, never drop.** A file that still has no lot is recorded in
///    [`LotPlan::overflow`] with the lot it shares. The caller must draw it —
///    stacked, badged, or as a second building in the same parcel — and
///    [`LotReport::is_overcrowded`] says so out loud. `overflow` is never
///    silently empty because a file was discarded; the accounting identity in
///    [`LotReport::files`] is asserted by the test suite.
///
/// The only way to get [`LotReport::unplaced`] above zero is a city with no
/// blocks at all — no roads closed a loop — and that is a road-growth failure,
/// not a lot-assignment one.
#[must_use]
pub fn plan(blocks: &[Block], tree: &RepoTree) -> LotPlan {
    let blocks_by_district = group_by_district(blocks);
    let (files_by_district, massed) = split_files(tree);

    let mut out = LotPlan {
        massed,
        ..LotPlan::default()
    };
    out.report.massed = out.massed.len();
    out.report.blocks = blocks.len();

    // Step 3: route every district's files to a host district that has blocks.
    let fallback = blocks_by_district.keys().next().cloned();
    let mut hosted: DistrictFiles = BTreeMap::new();
    for (district, files) in files_by_district {
        let host = host_for(&district, &blocks_by_district);
        if host.as_ref() != Some(&district) {
            out.report.districts_without_blocks += 1;
        }
        if let Some(host) = host.or_else(|| fallback.clone()) {
            if host != district {
                if district.starts_with(&host) {
                    out.report.hosted_by_ancestor += files.len();
                } else {
                    out.report.displaced += files.len();
                }
            }
            out.host_of.insert(district, host.clone());
            hosted.entry(host).or_default().extend(files);
        } else {
            // No block anywhere in the city: no roads closed a loop. Recorded
            // rather than dropped; the count is taken off this list below.
            for (path, _) in files {
                out.overflow.push(Overflow { path, shares: None });
            }
        }
    }

    // Step 2: size each host's subdivision from the files it actually holds,
    // then subdivide block by block so lot ids run in block order.
    let targets = subdivision_targets(blocks, &blocks_by_district, &hosted);
    let mut next_id = 0_u32;
    for block in blocks {
        let Some(target) = targets.get(&block.id) else {
            continue;
        };
        let parcels = subdivide(block, *target, block_seed(block));
        let lots = number(parcels, block.id, &mut next_id);
        let ids: Vec<LotId> = lots.iter().map(|lot| lot.id).collect();
        out.by_district
            .entry(block.district.clone())
            .or_default()
            .extend(ids.iter().copied());
        out.by_block.insert(block.id, ids);
        out.lots.extend(lots);
    }
    out.report.lots = out.lots.len();

    // Step 4: assign, then record who shares.
    let mut occupants: BTreeMap<LotId, LogicalPath> = BTreeMap::new();
    for (host, files) in hosted {
        let candidates = out.by_district.get(&host).cloned().unwrap_or_default();
        let assignment = assign(&files, &candidates);
        out.report.placed += assignment.len();
        out.report.unoccupied += assignment.unoccupied().len();
        for (path, lot) in assignment.placed() {
            occupants.insert(*lot, path.clone());
        }
        out.overflow.extend_from_slice(assignment.overflow());
    }
    // Counted off the entries themselves rather than tracked alongside them, so
    // the accounting identity in `LotReport::files` cannot drift from the list
    // the caller is handed. A shared lot is overflow; no lot at all is unplaced.
    out.report.overflow = out
        .overflow
        .iter()
        .filter(|entry| entry.shares.is_some())
        .count();
    out.report.unplaced = out.overflow.len() - out.report.overflow;
    for lot in &mut out.lots {
        lot.occupant = occupants.remove(&lot.id);
    }
    debug_assert_eq!(
        out.report.files(),
        tree.files.len(),
        "a file went missing between the tree and the plan"
    );
    out
}

/// Files awaiting a lot, grouped by district: `(logical path, growth index)`,
/// which is exactly what [`assign`] takes.
type DistrictFiles = BTreeMap<LogicalPath, Vec<(LogicalPath, u32)>>;

/// Splits a tree into placeable files by district and the massed ones (PRD §8).
fn split_files(tree: &RepoTree) -> (DistrictFiles, Vec<LogicalPath>) {
    let mut by_district: DistrictFiles = BTreeMap::new();
    let mut massed = Vec::new();
    for meta in tree.files.values() {
        if meta.class.is_massed() {
            massed.push(meta.path.clone());
            continue;
        }
        by_district
            .entry(district_of(&meta.path))
            .or_default()
            .push((meta.path.clone(), meta.growth_index));
    }
    (by_district, massed)
}

/// The nearest district at or above `district` that owns a block.
///
/// Walks up the directory tree and stops at the root, so it terminates on every
/// input including the root itself.
fn host_for(
    district: &LogicalPath,
    blocks_by_district: &BTreeMap<LogicalPath, Vec<BlockId>>,
) -> Option<LogicalPath> {
    let mut candidate = district.clone();
    loop {
        if blocks_by_district.contains_key(&candidate) {
            return Some(candidate);
        }
        if candidate.is_root() {
            return None;
        }
        candidate = candidate.parent().unwrap_or_else(LogicalPath::root);
    }
}

/// Target lot area per block, for the blocks whose district hosts files.
///
/// A block in a district that hosts nothing gets no entry and is not
/// subdivided: it stays open ground. Generating thousands of parcels nobody will
/// ever stand on costs the frame budget and says nothing.
fn subdivision_targets(
    blocks: &[Block],
    blocks_by_district: &BTreeMap<LogicalPath, Vec<BlockId>>,
    hosted: &DistrictFiles,
) -> BTreeMap<BlockId, f32> {
    let area_of: BTreeMap<BlockId, f32> = blocks
        .iter()
        .map(|block| (block.id, block.boundary.area()))
        .collect();
    let mut targets = BTreeMap::new();
    for (host, files) in hosted {
        let Some(ids) = blocks_by_district.get(host) else {
            continue;
        };
        let area: f32 = ids.iter().filter_map(|id| area_of.get(id)).copied().sum();
        let target = target_area_for(area, files.len());
        for id in ids {
            targets.insert(*id, target);
        }
    }
    targets
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
    use crate::blocks::{assign_districts, extract, DistrictSites};
    use crate::determinism::fnv1a64;
    use crate::roads::{grow, scatter_attractors, suggested_extent, GrowthParams};
    use crate::terrain::TerrainField;
    use crate::{Point, RoadClass};

    fn lp(text: &str) -> LogicalPath {
        LogicalPath::new(text).expect("valid logical path")
    }

    fn rect(w: f32, h: f32) -> Polygon {
        Polygon::new(vec![
            Point::new(0.0, 0.0),
            Point::new(w, 0.0),
            Point::new(w, h),
            Point::new(0.0, h),
        ])
    }

    fn block_of(boundary: Polygon, id: u32, district: &str) -> Block {
        Block {
            id: BlockId(id),
            boundary,
            district: LogicalPath::new(district).expect("valid path"),
        }
    }

    /// A deep C, whose notch a straight cut must cross twice.
    fn concave() -> Polygon {
        Polygon::new(vec![
            Point::new(0.0, 0.0),
            Point::new(40.0, 0.0),
            Point::new(40.0, 10.0),
            Point::new(10.0, 10.0),
            Point::new(10.0, 30.0),
            Point::new(40.0, 30.0),
            Point::new(40.0, 40.0),
            Point::new(0.0, 40.0),
        ])
    }

    // -----------------------------------------------------------------------
    // The cut
    // -----------------------------------------------------------------------

    #[test]
    fn a_split_cuts_perpendicular_to_the_longest_axis() {
        // 40 wide by 4 tall: the cut must halve the 40, not the 4.
        let (low, high) = split_once(&rect(40.0, 4.0), 0.5).expect("a viable cut");
        assert_eq!(low.area() + high.area(), 160.0);
        let (llo, lhi) = low.bounds().expect("bounds");
        let (hlo, hhi) = high.bounds().expect("bounds");
        assert_eq!((llo.x, lhi.x), (0.0, 20.0));
        assert_eq!((hlo.x, hhi.x), (20.0, 40.0));
        assert_eq!((llo.y, lhi.y), (0.0, 4.0), "the short axis is untouched");
        assert_eq!(hhi.y, 4.0);
    }

    #[test]
    fn the_low_side_always_comes_back_first() {
        for offset in [0.35, 0.4, 0.5, 0.6, 0.65] {
            let (low, high) = split_once(&rect(40.0, 4.0), offset).expect("a viable cut");
            assert!(
                low.centroid().x < high.centroid().x,
                "offset {offset} put the pieces the wrong way round"
            );
        }
    }

    #[test]
    fn the_offset_is_clamped_into_the_split_band() {
        // Wild offsets must not produce a sliver; they clamp to the band.
        let wide = split_once(&rect(40.0, 4.0), 0.0).expect("clamped, not refused");
        let narrow_end = split_once(&rect(40.0, 4.0), 1.0).expect("clamped, not refused");
        assert_eq!(wide.0.area(), 160.0 * MIN_SPLIT_FRACTION);
        assert_eq!(narrow_end.1.area(), 160.0 * MIN_SPLIT_FRACTION);
        let midpoint = split_once(&rect(40.0, 4.0), f32::NAN).expect("NaN cuts at the midpoint");
        assert_eq!(midpoint.0.area(), 80.0);
    }

    #[test]
    fn a_cut_across_a_concave_notch_conserves_area() {
        let parent = concave();
        let (low, high) = split_once(&parent, 0.5).expect("a viable cut");
        assert!(
            (low.area() + high.area() - parent.area()).abs() < 0.01,
            "{} + {} != {}",
            low.area(),
            high.area(),
            parent.area()
        );
    }

    #[test]
    fn a_degenerate_polygon_is_refused_rather_than_split() {
        assert!(split_once(&Polygon::default(), 0.5).is_none());
        assert!(
            split_once(&rect(1.0, 1.0), 0.5).is_none(),
            "too small to halve"
        );
        // A line, a point, and a shape with a NaN corner.
        assert!(split_once(
            &Polygon::new(vec![Point::ORIGIN, Point::new(10.0, 0.0)]),
            0.5
        )
        .is_none());
        assert!(split_once(
            &Polygon::new(vec![Point::ORIGIN, Point::ORIGIN, Point::ORIGIN]),
            0.5
        )
        .is_none());
        assert!(split_once(
            &Polygon::new(vec![
                Point::ORIGIN,
                Point::new(f32::NAN, 0.0),
                Point::new(0.0, 40.0),
                Point::new(-40.0, 0.0),
            ]),
            0.5
        )
        .is_none());
    }

    // -----------------------------------------------------------------------
    // The recursion
    // -----------------------------------------------------------------------

    #[test]
    fn subdivision_stops_at_the_target_area() {
        let parcels = subdivide_boundary(&rect(60.0, 40.0), TARGET_LOT_AREA, 1);
        assert!(parcels.len() > 100, "{} parcels", parcels.len());
        let total: f32 = parcels.iter().map(Polygon::area).sum();
        assert!((total - 2400.0).abs() < 1.0, "area leaked: {total}");
        for parcel in &parcels {
            assert!(parcel.area() >= MIN_LOT_AREA, "{parcel:?}");
            assert!(
                parcel.area() <= TARGET_LOT_AREA + 0.001,
                "a parcel over target survived: {}",
                parcel.area()
            );
            assert!(parcel.is_valid());
        }
    }

    /// The whole termination argument, exercised on the shapes that break it.
    #[test]
    fn pathological_blocks_terminate() {
        let shapes: Vec<(&str, Polygon)> = vec![
            ("concave", concave()),
            ("sliver", rect(400.0, 0.02)),
            ("near-zero", rect(1.4, 1.4)),
            ("empty", Polygon::default()),
            (
                "line",
                Polygon::new(vec![Point::ORIGIN, Point::new(9.0, 0.0)]),
            ),
            (
                "self-touching",
                // A figure-eight boundary: the road graph produces these when a
                // snapped junction pinches a face.
                Polygon::new(vec![
                    Point::new(0.0, 0.0),
                    Point::new(20.0, 0.0),
                    Point::new(10.0, 10.0),
                    Point::new(20.0, 20.0),
                    Point::new(0.0, 20.0),
                    Point::new(10.0, 10.0),
                ]),
            ),
            (
                "non-finite",
                Polygon::new(vec![
                    Point::ORIGIN,
                    Point::new(f32::INFINITY, 0.0),
                    Point::new(0.0, 30.0),
                ]),
            ),
            (
                "spiral",
                Polygon::new(
                    (0..64)
                        .map(|i| {
                            #[allow(clippy::cast_precision_loss)] // fixture only
                            let t = i as f32 * 0.3;
                            Point::new(t * t.cos(), t * t.sin())
                        })
                        .collect(),
                ),
            ),
        ];
        for (name, shape) in shapes {
            for target in [MIN_LOT_AREA, 0.0, -1.0, f32::NAN, TARGET_LOT_AREA] {
                let parcels = subdivide_boundary(&shape, target, 9);
                assert!(
                    parcels.len() <= MAX_LOTS_PER_BLOCK,
                    "{name} at target {target} produced {}",
                    parcels.len()
                );
                for parcel in &parcels {
                    assert!(parcel.is_valid(), "{name} emitted {parcel:?}");
                    assert!(parcel.area() >= MIN_LOT_AREA, "{name}");
                }
            }
        }
    }

    #[test]
    fn subdivision_is_reproducible_and_seed_sensitive() {
        let a = subdivide_boundary(&concave(), TARGET_LOT_AREA, 11);
        let b = subdivide_boundary(&concave(), TARGET_LOT_AREA, 11);
        assert_eq!(a, b);
        let c = subdivide_boundary(&concave(), TARGET_LOT_AREA, 12);
        assert_ne!(a, c, "a different block must subdivide differently");
    }

    #[test]
    fn numbering_is_one_monotonic_sequence_over_the_city() {
        let mut next = 0_u32;
        let first = number(
            subdivide_boundary(&rect(30.0, 20.0), 20.0, 1),
            BlockId(0),
            &mut next,
        );
        let second = number(
            subdivide_boundary(&rect(30.0, 20.0), 20.0, 2),
            BlockId(1),
            &mut next,
        );
        assert_eq!(first[0].id, LotId(0));
        assert_eq!(
            second[0].id,
            LotId(u32::try_from(first.len()).expect("small"))
        );
        assert_eq!(next as usize, first.len() + second.len());
        assert!(first.iter().all(|lot| lot.block == BlockId(0)));
        assert!(second.iter().all(Lot::is_vacant));
    }

    /// [`LEAF_FILL`] is a measured constant, not a guess; if the recursion
    /// changes shape this is the test that says the district sizing is stale.
    #[test]
    fn the_lot_yield_matches_the_estimate() {
        for (w, h, files) in [
            (60.0_f32, 40.0_f32, 400_usize),
            (100.0, 30.0, 400),
            (25.0, 25.0, 80),
            (80.0, 12.0, 120),
        ] {
            let area = w * h;
            let target = target_area_for(area, files);
            assert!(
                target > MIN_LOT_AREA && target < MAX_LOT_AREA,
                "{w}x{h} for {files} files clamps at {target}; the estimate is not under test"
            );
            let lots = subdivide_boundary(&rect(w, h), target, 5).len();
            #[allow(clippy::cast_precision_loss)] // small counts
            let ratio = lots as f32 / files as f32;
            assert!(
                (1.0..=1.6).contains(&ratio),
                "{w}x{h} for {files} files yielded {lots} lots (ratio {ratio})"
            );
        }
    }

    /// The failure this constant exists to stop: a fixed target on a district
    /// with far more block than files buries the buildings in empty parcels.
    #[test]
    fn a_sparse_district_gets_large_plots_not_a_field_of_empty_ones() {
        let files = 6;
        let area = 2_400.0_f32;
        assert_eq!(target_area_for(area, files), MAX_LOT_AREA);
        let scaled = subdivide_boundary(&rect(60.0, 40.0), target_area_for(area, files), 5).len();
        let fixed = subdivide_boundary(&rect(60.0, 40.0), TARGET_LOT_AREA, 5).len();
        assert!(
            fixed > 250,
            "the fixed target really does over-subdivide: {fixed}"
        );
        assert!(
            scaled < fixed / 5,
            "scaling the target did not help: {scaled} against {fixed}"
        );
        assert!(scaled >= files, "{scaled} lots cannot house {files} files");
    }

    #[test]
    fn target_area_is_clamped_at_both_ends() {
        assert_eq!(
            target_area_for(100_000.0, 1),
            MAX_LOT_AREA,
            "a quiet corner"
        );
        assert_eq!(target_area_for(10.0, 10_000), MIN_LOT_AREA, "downtown");
        assert_eq!(
            target_area_for(500.0, 0),
            TARGET_LOT_AREA,
            "no files: default"
        );
        assert_eq!(target_area_for(f32::NAN, 5), TARGET_LOT_AREA);
        assert_eq!(target_area_for(-5.0, 5), TARGET_LOT_AREA);
    }

    // -----------------------------------------------------------------------
    // Assignment
    // -----------------------------------------------------------------------

    fn files(count: usize) -> Vec<(LogicalPath, u32)> {
        (0..count)
            .map(|i| {
                (
                    lp(&format!("src/mod{i:04}.rs")),
                    u32::try_from(i).expect("small"),
                )
            })
            .collect()
    }

    fn lot_ids(count: usize) -> Vec<LotId> {
        (0..count)
            .map(|i| LotId(u32::try_from(i).expect("small")))
            .collect()
    }

    /// Deterministic Fisher–Yates: a randomly seeded shuffle inside a
    /// determinism suite would be self-contradictory.
    fn shuffled<T: Clone>(items: &[T], seed: u64) -> Vec<T> {
        let mut out = items.to_vec();
        let mut draw = SeededRng::for_seed(seed, "test shuffle");
        for i in (1..out.len()).rev() {
            let j =
                usize::try_from(draw.below(u64::try_from(i + 1).expect("small"))).expect("small");
            out.swap(i, j);
        }
        out
    }

    /// The test that actually catches iteration-order bugs.
    #[test]
    fn assignment_is_stable_under_reordering_of_the_input() {
        let base = files(180);
        let lots = lot_ids(220);
        let reference = assign(&base, &lots);
        assert_eq!(reference.len(), 180);
        for seed in [1_u64, 2, 3, 99, 4_242] {
            let shuffled_files = shuffled(&base, seed);
            assert_ne!(
                shuffled_files, base,
                "the shuffle did nothing at seed {seed}"
            );
            let again = assign(&shuffled_files, &lots);
            assert_eq!(reference, again, "input order reached the layout at {seed}");
            // And shuffling the lot list must not move anybody either.
            let other = assign(&base, &shuffled(&lots, seed));
            assert_eq!(reference, other);
        }
    }

    #[test]
    fn a_file_lands_on_the_same_lot_however_many_neighbours_it_has() {
        let lots = lot_ids(400);
        let watched = lp("src/mod0007.rs");
        let with_a_few = assign(&files(20), &lots);
        let with_many = assign(&files(300), &lots);
        // Not the same lot in general — the district is differently packed —
        // but the *home* slot is a pure function of the path, which is what
        // makes a full re-plan of an unchanged tree reproduce itself.
        assert_eq!(
            home_slot(&watched, lots.len()),
            home_slot(&watched, lots.len())
        );
        assert!(with_a_few.lot_of(&watched).is_some());
        assert!(with_many.lot_of(&watched).is_some());
        assert_eq!(assign(&files(20), &lots), with_a_few);
    }

    #[test]
    fn every_file_is_accounted_for_when_lots_run_out() {
        let assignment = assign(&files(50), &lot_ids(20));
        assert_eq!(assignment.len(), 20, "every lot is used");
        assert_eq!(assignment.overflow().len(), 30, "and nobody is dropped");
        assert!(assignment.unoccupied().is_empty());
        for entry in assignment.overflow() {
            assert!(entry.shares.is_some(), "an overflow file still has a place");
        }

        // No lots at all: still nobody dropped.
        let none = assign(&files(7), &[]);
        assert!(none.is_empty());
        assert_eq!(none.overflow().len(), 7);
        assert!(none.overflow().iter().all(|entry| entry.shares.is_none()));
    }

    #[test]
    fn spare_lots_come_back_as_open_ground() {
        let assignment = assign(&files(10), &lot_ids(25));
        assert_eq!(assignment.len(), 10);
        assert_eq!(assignment.unoccupied().len(), 15);
        assert!(assignment.overflow().is_empty());
        // The unoccupied list is in id order, so it can be serialized.
        let mut sorted = assignment.unoccupied().to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, assignment.unoccupied());
    }

    #[test]
    fn a_duplicated_path_is_placed_once() {
        let mut duplicated = files(5);
        duplicated.push((lp("src/mod0002.rs"), 99));
        let assignment = assign(&duplicated, &lot_ids(10));
        assert_eq!(assignment.len(), 5);
        assert!(assignment.overflow().is_empty());
    }

    // -----------------------------------------------------------------------
    // Vacancy (PRD §7.5)
    // -----------------------------------------------------------------------

    fn day(n: i64) -> WallTime {
        WallTime::from_unix_seconds(n * 86_400)
    }

    #[test]
    fn a_deleted_file_leaves_a_vacant_lot_that_goes_to_seed() {
        let mut lots = number(
            subdivide_boundary(&rect(30.0, 20.0), 20.0, 1),
            BlockId(0),
            &mut 0,
        );
        let path = lp("src/gone.rs");
        lots[0].occupant = Some(path.clone());

        let mut ledger = VacancyLedger::new();
        let vacancy = ledger
            .vacate(&mut lots, LotId(0), day(100))
            .expect("the lot was occupied")
            .clone();
        assert_eq!(vacancy.former, path);
        assert!(lots[0].is_vacant(), "the lot stays; only the building goes");
        assert_eq!(ledger.len(), 1);

        assert_eq!(vacancy.seed_progress(day(100)), 0.0);
        assert_eq!(vacancy.seed_progress(day(190)), 0.5);
        assert_eq!(vacancy.seed_progress(day(280)), 1.0);
        assert_eq!(vacancy.seed_progress(day(10_000)), 1.0, "clamped");
        assert_eq!(
            vacancy.seed_progress(day(50)),
            0.0,
            "a past `now` is clamped"
        );
        assert!(!vacancy.is_overgrown(day(200)));
        assert!(vacancy.is_overgrown(day(281)));
        assert_eq!(ledger.gone_to_seed(day(281)), vec![LotId(0)]);
        assert!(ledger.gone_to_seed(day(150)).is_empty());
        assert_eq!(vacancy.seed_progress_over(day(101), 1), 1.0);
        assert_eq!(vacancy.seed_progress_over(day(101), 0), 1.0);

        // A lot cannot be vacated twice, and an unknown lot is not a panic.
        assert!(ledger.vacate(&mut lots, LotId(0), day(120)).is_none());
        assert!(ledger.vacate(&mut lots, LotId(9_999), day(120)).is_none());
    }

    #[test]
    fn a_vacant_lot_is_redeveloped_and_forgets_its_ghost() {
        let mut lots = number(
            subdivide_boundary(&rect(30.0, 20.0), 20.0, 1),
            BlockId(0),
            &mut 0,
        );
        let candidates: Vec<LotId> = lots.iter().map(|lot| lot.id).collect();
        let gone = lp("src/gone.rs");
        lots[0].occupant = Some(gone.clone());

        let mut ledger = VacancyLedger::new();
        ledger
            .vacate(&mut lots, LotId(0), day(1))
            .expect("occupied");
        assert_eq!(ledger.vacate_path(&mut lots, &gone, day(1)), None);

        // Settle enough new files that somebody lands on the freed lot.
        let mut landed = false;
        for i in 0..candidates.len() {
            let path = lp(&format!("src/new{i:03}.rs"));
            match ledger.settle(&mut lots, &candidates, &path) {
                Some(LotId(0)) => landed = true,
                Some(_) => {}
                None => break,
            }
        }
        assert!(landed, "the freed lot went back into use");
        assert!(ledger.is_empty(), "and its ghost was cleared");
        assert!(ledger.get(LotId(0)).is_none());
    }

    /// The incremental path must agree with a full re-plan, or PRD §7.4's
    /// "growth is genuinely incremental" is not true.
    #[test]
    fn settling_a_new_file_matches_a_full_reassignment() {
        let mut lots = number(
            subdivide_boundary(&rect(60.0, 40.0), 18.0, 3),
            BlockId(0),
            &mut 0,
        );
        let candidates: Vec<LotId> = lots.iter().map(|lot| lot.id).collect();
        let existing = files(30);
        let full = assign(&existing, &candidates);
        for lot in &mut lots {
            lot.occupant = full
                .placed()
                .iter()
                .find(|(_, id)| **id == lot.id)
                .map(|(path, _)| path.clone());
        }

        let newcomer = (lp("src/mod9999.rs"), 30_u32);
        let mut extended = existing.clone();
        extended.push(newcomer.clone());
        let regenerated = assign(&extended, &candidates);

        let mut ledger = VacancyLedger::new();
        let settled = ledger
            .settle(&mut lots, &candidates, &newcomer.0)
            .expect("room for one more");
        assert_eq!(
            Some(settled),
            regenerated.lot_of(&newcomer.0),
            "an incremental settle disagreed with a full regeneration"
        );
        // And nobody else moved.
        for (path, lot) in full.placed() {
            assert_eq!(regenerated.lot_of(path), Some(*lot), "{path} moved");
        }
    }

    #[test]
    fn settling_into_a_full_district_reports_no_room_rather_than_evicting() {
        let mut lots = number(
            subdivide_boundary(&rect(20.0, 12.0), 60.0, 1),
            BlockId(0),
            &mut 0,
        );
        let candidates: Vec<LotId> = lots.iter().map(|lot| lot.id).collect();
        for (index, lot) in lots.iter_mut().enumerate() {
            lot.occupant = Some(lp(&format!("src/held{index}.rs")));
        }
        let mut ledger = VacancyLedger::new();
        assert_eq!(
            ledger.settle(&mut lots, &candidates, &lp("src/late.rs")),
            None
        );
        assert_eq!(ledger.settle(&mut lots, &[], &lp("src/late.rs")), None);
        assert!(
            lots.iter().all(|lot| lot.occupant.is_some()),
            "nobody evicted"
        );
    }

    #[test]
    fn a_ledger_round_trips_through_json() {
        let mut ledger = VacancyLedger::new();
        let mut lots = vec![Lot {
            id: LotId(3),
            block: BlockId(0),
            boundary: rect(4.0, 4.0),
            occupant: Some(lp("src/x.rs")),
        }];
        ledger
            .vacate(&mut lots, LotId(3), day(7))
            .expect("occupied");
        let json = serde_json::to_string(&ledger).expect("serializes");
        let back: VacancyLedger = serde_json::from_str(&json).expect("parses");
        assert_eq!(back, ledger);
        assert_eq!(back.iter().count(), 1);
        assert_eq!(ledger.forget(LotId(3)).map(|v| v.lot), Some(LotId(3)));
        assert!(ledger.is_empty());
    }

    // -----------------------------------------------------------------------
    // The whole-city plan
    // -----------------------------------------------------------------------

    fn tree_of(entries: &[(&str, FileClass)]) -> RepoTree {
        let mut files = BTreeMap::new();
        for (index, (path, class)) in entries.iter().enumerate() {
            let path = lp(path);
            files.insert(
                path.clone(),
                FileMeta {
                    path,
                    size_bytes: 512,
                    growth_index: u32::try_from(index).expect("small"),
                    added_at: WallTime::UNIX_EPOCH,
                    last_touched: WallTime::UNIX_EPOCH,
                    class: *class,
                    language: None,
                },
            );
        }
        RepoTree {
            root: PathBuf::from("/repo"),
            files,
            worktrees: BTreeMap::new(),
            head: "0".repeat(40),
        }
    }

    fn synthetic_tree(districts: usize, per_district: usize) -> RepoTree {
        let mut entries: Vec<(String, FileClass)> = Vec::new();
        for file in 0..per_district {
            for district in 0..districts {
                entries.push((
                    format!("crate{district:03}/src/mod{file:04}.rs"),
                    FileClass::Ordinary,
                ));
            }
        }
        let borrowed: Vec<(&str, FileClass)> = entries
            .iter()
            .map(|(path, class)| (path.as_str(), *class))
            .collect();
        tree_of(&borrowed)
    }

    /// Blocks from a real grown road graph, assigned to real districts.
    fn city_blocks(tree: &RepoTree) -> Vec<Block> {
        let extent = suggested_extent(tree);
        let terrain = TerrainField::generate(fnv1a64(b"lots test fixture"), extent);
        let attractors = scatter_attractors(tree, extent);
        let graph = grow(
            &terrain,
            &attractors,
            GrowthParams::default(),
            RoadClass::Street,
        );
        let mut blocks = extract(&graph);
        let sites = DistrictSites::from_scatter(tree, &attractors);
        assign_districts(&mut blocks, &sites);
        blocks
    }

    #[test]
    fn every_file_in_a_real_repository_is_accounted_for() {
        let tree = synthetic_tree(6, 9);
        let blocks = city_blocks(&tree);
        let plan = plan(&blocks, &tree);
        assert_eq!(
            plan.report.files(),
            tree.files.len(),
            "a file went missing: {:?}",
            plan.report
        );
        assert_eq!(plan.report.unplaced, 0);
        assert!(plan.report.placed > 0);
        assert_eq!(plan.lots.len(), plan.report.lots);
        // Ids match indices, and occupants are unique.
        let mut occupied: BTreeSet<&LogicalPath> = BTreeSet::new();
        for (index, lot) in plan.lots.iter().enumerate() {
            assert_eq!(lot.id, LotId(u32::try_from(index).expect("small")));
            if let Some(path) = &lot.occupant {
                assert!(occupied.insert(path), "{path} is on two lots");
            }
        }
        assert_eq!(occupied.len(), plan.report.placed);
    }

    #[test]
    fn industrial_files_are_massed_rather_than_given_lots() {
        let tree = tree_of(&[
            ("src/main.rs", FileClass::Monument),
            ("src/lib.rs", FileClass::Ordinary),
            ("node_modules/a/index.js", FileClass::Industrial),
            ("node_modules/b/index.js", FileClass::Industrial),
            ("node_modules/c/index.js", FileClass::Industrial),
        ]);
        let blocks = vec![block_of(rect(40.0, 40.0), 0, "src")];
        let plan = plan(&blocks, &tree);
        assert_eq!(plan.report.massed, 3);
        assert_eq!(plan.massed.len(), 3);
        assert_eq!(plan.report.placed, 2);
        assert_eq!(plan.report.files(), 5);
        assert!(plan.lots.iter().all(|lot| lot
            .occupant
            .as_ref()
            .is_none_or(|p| !p.as_str().starts_with("node_modules"))));
    }

    /// PRD §9: the tree determines placement, so a district with no block of its
    /// own goes to its **ancestor**, not to whichever district is nearest.
    #[test]
    fn a_district_with_no_block_is_hosted_by_its_ancestor() {
        let tree = tree_of(&[
            ("src/lib.rs", FileClass::Ordinary),
            ("src/auth/token.rs", FileClass::Ordinary),
            ("src/auth/session.rs", FileClass::Ordinary),
        ]);
        let blocks = vec![block_of(rect(40.0, 40.0), 0, "src")];
        let plan = plan(&blocks, &tree);
        assert_eq!(plan.host_of[&lp("src/auth")], lp("src"));
        assert_eq!(plan.host_of[&lp("src")], lp("src"));
        assert_eq!(plan.report.hosted_by_ancestor, 2);
        assert_eq!(plan.report.displaced, 0);
        assert_eq!(plan.report.districts_without_blocks, 1);
        assert_eq!(plan.report.placed, 3);
    }

    #[test]
    fn a_district_with_no_ancestor_block_is_displaced_and_counted() {
        let tree = tree_of(&[
            ("docs/readme.md", FileClass::Ordinary),
            ("src/lib.rs", FileClass::Ordinary),
        ]);
        // Only `src` has a block, and `docs` is not below it.
        let blocks = vec![block_of(rect(40.0, 40.0), 0, "src")];
        let plan = plan(&blocks, &tree);
        assert_eq!(plan.host_of[&lp("docs")], lp("src"));
        assert_eq!(plan.report.displaced, 1);
        assert_eq!(plan.report.placed, 2);
        assert_eq!(plan.report.files(), 2);
    }

    #[test]
    fn a_crowded_district_densifies_before_it_overflows() {
        // 400 files into one 40x40 block. Densification alone cannot house them
        // — 1600 units of area at a 2.0 floor is 800 lots at best, and the
        // recursion's real yield is lower — so this is the case the policy is
        // written for.
        let entries: Vec<(String, FileClass)> = (0..400)
            .map(|i| (format!("src/mod{i:04}.rs"), FileClass::Ordinary))
            .collect();
        let borrowed: Vec<(&str, FileClass)> = entries
            .iter()
            .map(|(path, class)| (path.as_str(), *class))
            .collect();
        let tree = tree_of(&borrowed);
        let blocks = vec![block_of(rect(40.0, 40.0), 0, "src")];
        let plan = plan(&blocks, &tree);

        assert!(
            plan.report.lots > 200,
            "densified: {} lots",
            plan.report.lots
        );
        assert_eq!(plan.report.files(), 400);
        assert_eq!(plan.report.placed + plan.report.overflow, 400);
        assert_eq!(plan.report.unplaced, 0);
        for entry in &plan.overflow {
            assert!(entry.shares.is_some(), "nobody is dropped");
        }
    }

    #[test]
    fn a_city_with_no_blocks_reports_every_file_unplaced_rather_than_losing_them() {
        let tree = synthetic_tree(2, 3);
        let plan = plan(&[], &tree);
        assert_eq!(plan.report.unplaced, tree.files.len());
        assert_eq!(plan.report.files(), tree.files.len());
        assert_eq!(plan.overflow.len(), tree.files.len());
        assert!(plan.overflow.iter().all(|entry| entry.shares.is_none()));
        assert!(plan.report.is_overcrowded());
        assert!(plan.lots.is_empty());
    }

    #[test]
    fn a_block_whose_district_holds_no_file_is_left_as_open_ground() {
        let tree = tree_of(&[("src/lib.rs", FileClass::Ordinary)]);
        let blocks = vec![
            block_of(rect(40.0, 40.0), 0, "src"),
            block_of(rect(40.0, 40.0), 1, "vendor"),
        ];
        let plan = plan(&blocks, &tree);
        assert!(plan.by_block.contains_key(&BlockId(0)));
        assert!(
            !plan.by_block.contains_key(&BlockId(1)),
            "an empty district should not spend the lot budget"
        );
    }

    // -----------------------------------------------------------------------
    // Determinism
    // -----------------------------------------------------------------------

    fn digest(plan: &LotPlan) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        let mut eat = |bytes: &[u8]| {
            for byte in bytes {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        };
        for lot in &plan.lots {
            eat(&lot.id.0.to_le_bytes());
            eat(&lot.block.0.to_le_bytes());
            eat(lot
                .occupant
                .as_ref()
                .map_or("", LogicalPath::as_str)
                .as_bytes());
            for vertex in &lot.boundary.vertices {
                eat(&crate::determinism::quantize(vertex.x).to_le_bytes());
                eat(&crate::determinism::quantize(vertex.y).to_le_bytes());
            }
        }
        for entry in &plan.overflow {
            eat(entry.path.as_str().as_bytes());
        }
        hash
    }

    fn reference_digest() -> u64 {
        let tree = synthetic_tree(5, 8);
        let blocks = city_blocks(&tree);
        digest(&plan(&blocks, &tree))
    }

    /// [`crate::determinism`] rule 7 and ADR-0029: a literal, so a change
    /// anywhere upstream that moves the city says so instead of moving it
    /// quietly.
    ///
    /// The digest also holds across `--release`, which is the practical
    /// evidence that nothing here is being contracted or re-associated by the
    /// optimiser (rules 3 and 5). Verified by running this test under both
    /// profiles; the value below is the same in each.
    ///
    /// # If this fails
    ///
    /// Decide whether the change to lot plan was intended. If it was, every
    /// golden layout file in the repository is invalidated **on purpose** —
    /// update this literal and regenerate them together. If it was not, the
    /// diff that caused it is the bug.
    #[test]
    fn the_lot_digest_is_pinned() {
        {
            assert_eq!(
                format!("{:016x}", reference_digest()),
                "a7cc97d4d339d9b7",
                "the lot plan moved"
            );
        }
    }

    #[test]
    fn two_runs_in_one_process_are_identical() {
        assert_eq!(reference_digest(), reference_digest());
    }

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

    fn child_digest() -> u64 {
        let exe = std::env::current_exe().expect("test binary path");
        let output = Command::new(exe)
            .args([
                "--exact",
                "lots::tests::print_reference_digest",
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
            .find_map(|line| line.strip_prefix("POLIS_LOTS_DIGEST="))
            .unwrap_or_else(|| panic!("child printed no digest:\n{stdout}"));
        u64::from_str_radix(line.trim(), 16).expect("hex digest")
    }

    /// The density the operator actually sees, on a repository-sized fixture.
    ///
    /// This is a product test wearing a numbers costume. Too few lots and files
    /// share parcels; too many and the map is a survey plan with a building
    /// here and there. Both failures are silent — nothing panics, nothing is
    /// dropped — so the band is the only thing that catches them.
    #[test]
    fn a_five_thousand_file_repo_is_housed_at_a_legible_density() {
        use std::time::Instant;

        let tree = synthetic_tree(50, 100);
        assert_eq!(tree.files.len(), 5_000);
        let blocks = city_blocks(&tree);

        let start = Instant::now();
        let plan = plan(&blocks, &tree);
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;

        assert_eq!(plan.report.files(), 5_000, "{:?}", plan.report);
        assert_eq!(plan.report.placed, 5_000, "{:?}", plan.report);
        assert_eq!(plan.report.overflow, 0);
        assert_eq!(plan.report.unplaced, 0);

        #[allow(clippy::cast_precision_loss)] // thousands, not quadrillions
        let lots_per_file = plan.report.lots as f64 / 5_000.0;
        assert!(
            (1.0..2.5).contains(&lots_per_file),
            "{} lots for 5000 files ({lots_per_file} each) — {:?}",
            plan.report.lots,
            plan.report
        );

        // PRD §13.1 budgets 3 s for the whole cold start; subdivision and
        // assignment are a small part of that and the bound is loose enough to
        // survive a loaded CI box while still catching an accidental O(n^2).
        assert!(
            elapsed_ms < 2_000.0,
            "lots took {elapsed_ms:.0} ms for 5000 files"
        );
    }

    /// The child half of [`two_fresh_processes_are_identical`].
    #[test]
    #[ignore = "child process of two_fresh_processes_are_identical"]
    fn print_reference_digest() {
        println!("POLIS_LOTS_DIGEST={:016x}", reference_digest());
    }
}
