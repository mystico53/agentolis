//! Step 5 — buildings (PRD §7.2, §7.3).
//!
//! > **Buildings** are lots inset by a setback, with a small random rotation
//! > (±4°).
//!
//! # Attributes
//!
//! * **Footprint area** proportional to `sqrt(file_size_bytes)`, clamped to
//!   `[min_lot, block_area * 0.6]`.
//! * **Height** proportional to uncommitted diff lines. The city rises as agents
//!   work and settles when you merge; the tallest thing on the map is the
//!   biggest unreviewed pile, which directly serves "where do I need to look".
//! * **Silhouette variety** carries most of the organic reading and costs
//!   nothing: vary roof form by a hash of the path.
//!
//! # Height's input comes from the transcript, not from re-diffing
//!
//! It already exists three ways: `structuredPatch` gives exact ±counts per edit,
//! `toolUseResult.toolStats` gives a per-subagent roll-up, and `cost-state`
//! gives a per-session total. `polis_repo::git::diff_line_counts` is the
//! *fallback*, needed because `structuredPatch` is missing for subagent edits
//! (ADR-0004, ADR-0042).
//!
//! So [`height_for_diff_lines`] takes a line count as an **input**. Nothing here
//! reads a working tree, and nothing here reads a clock.
//!
//! # PRD §17 open question 4 — the slow-decay ghost
//!
//! > Height from uncommitted diff means the city flattens on merge — satisfying,
//! > but does it destroy the "recently active" reading? Possibly needs a
//! > slow-decay ghost.
//!
//! It does destroy it, and the answer taken here is **yes, a ghost — but not in
//! the layout**. The height curve is [`height_for`], which takes the committed
//! line count *and* a ghost term in the same unit:
//!
//! ```text
//! height_for(diff_lines, ghost_lines)      the curve
//! height_for_diff_lines(d) == height_for(d, 0.0)
//! ```
//!
//! [`ghost_lines`] produces the ghost from "how big was the pile, and how long
//! ago did it settle". Two consequences, both deliberate:
//!
//! 1. **The ghost is a function of wall-clock time, so it may never be stored in
//!    [`crate::CityLayout`].** PRD §7.4 forbids the clock reaching the layout,
//!    and PRD §16 compares serialized layouts byte for byte across machines — a
//!    height that decays with the date would fail that comparison every day. So
//!    [`crate::city`] stores `height_for(diff_lines, 0.0)`, and the renderer
//!    adds the ghost per frame, through this same curve so there is exactly one
//!    height function in the product.
//! 2. **The API does not change when the ghost is switched on.** Every caller
//!    that wants the flat-on-merge reading passes `0.0`; a caller that wants the
//!    "recently active" reading passes [`ghost_lines`]. Adding the decay later
//!    is a call-site edit, not a signature change, which is what PRD §17 leaves
//!    open.
//!
//! The decay itself is a rational fall-off rather than `exp(-t/τ)`:
//! [`crate::determinism`] rule 4 bans transcendentals on values that reach the
//! layout, and while the ghost is render-only today, a curve that *could* be
//! stored without breaking the golden files is strictly better than one that
//! could not.
//!
//! # Landmarks (PRD §8)
//!
//! Two of PRD §8's five landmark rows are decided here, from
//! [`polis_repo::FileClass`]:
//!
//! * **Monument** — "tall, distinct silhouette, always labelled at every zoom".
//!   A height floor of [`MONUMENT_HEIGHT`] and a pinned [`RoofForm::Stepped`]
//!   silhouette, so the anchor is recognisable from the zoom where the whole
//!   city fits on screen and individual roofs do not resolve.
//! * **Industrial** — "rendered as a single mass, not individual buildings".
//!   [`place_with`] returns `None` for them, on purpose: an industrial file gets
//!   no building at all, and [`crate::city::IndustrialMass`] draws the district
//!   as one dull shape. `lots::plan` already refuses them a lot, so this is the
//!   second half of the same decision rather than a new one.
//!
//! # Every draw is seeded from the path
//!
//! Rotation and roof form both come from
//! [`SeededRng::for_path`](crate::determinism::SeededRng::for_path) with
//! **different purpose tags**, so that a later change to roof selection cannot
//! rotate every building in the city (PRD §7.4).

use polis_events::{LogicalPath, WallTime};
use polis_repo::FileClass;

use crate::determinism::{det_sin_cos, narrow, SeededRng};
use crate::{Building, Lot, Point, Polygon, RoofForm};

// ---------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------

/// Setback from the lot boundary, in city-space units. Layout-visible, so it is
/// a constant rather than a parameter.
pub const SETBACK: f32 = 0.4;

/// Maximum building rotation, in degrees. PRD §7.2 step 5 says ±4°.
pub const MAX_ROTATION_DEGREES: f32 = 4.0;

/// City-space area per `sqrt(byte)` (PRD §7.3).
///
/// Sized against [`crate::lots::TARGET_LOT_AREA`] **and then corrected by
/// looking at a rendered city**, which is the only way this number can be
/// chosen. At the first value tried (0.05) an 84-file repository put 429 units²
/// of building inside 2 632 units² of block — sixteen per cent block coverage,
/// where a real urban block is somewhere between a third and two thirds built.
/// The map read as scattered specks on open ground rather than as streets with
/// buildings along them.
///
/// At this value a 4 KiB source file has `sqrt(4096) · 0.10 = 6.4` units², which
/// is most of a [`crate::lots::TARGET_LOT_AREA`] parcel once the setback is
/// taken, and a 40 KiB file fills its lot outright. The size signal survives at
/// the small end — where the difference between a stub and a module is worth
/// seeing — and saturates at the large end, where the lot is the constraint
/// anyway.
pub const FOOTPRINT_SCALE: f32 = 0.10;

/// Fraction of the enclosing block a single building may cover (PRD §7.3).
pub const BLOCK_AREA_FRACTION: f32 = 0.6;

/// Smallest footprint worth drawing, in city-space units².
///
/// Below this a building is sub-pixel at every zoom PRD §12 defines and the lot
/// reads better as open ground.
pub const MIN_FOOTPRINT_AREA: f32 = 0.25;

/// Height of a building with no uncommitted change (PRD §7.3).
///
/// Not zero: a flat city on a clean tree would erase the skyline PRD §8 makes
/// the wayfinding layer.
pub const BASE_HEIGHT: f32 = 1.0;

/// City-space height per uncommitted diff line, below the knee.
///
/// > **Height** ∝ uncommitted diff lines. (PRD §7.3)
///
/// Literally proportional below [`HEIGHT_KNEE_LINES`], which is the regime every
/// edit an operator is actually watching lives in.
pub const HEIGHT_PER_LINE: f32 = 0.05;

/// Where the height curve stops being linear, in diff lines.
///
/// A forty-line edit is a normal unit of agent work and should be plainly
/// visible against a clean building; a four-thousand-line generated-file churn
/// should not be a hundred times taller than it, or nothing else on the map is
/// legible while it exists. Above the knee the curve is `sqrt(KNEE · lines)`,
/// which is the same square root PRD §7.3 already applies to file size, and for
/// the same reason.
pub const HEIGHT_KNEE_LINES: f32 = 40.0;

/// Hard ceiling on building height, in city-space units.
///
/// A sanity clamp, not a design parameter: it is reached at about 39 700
/// uncommitted lines, above which two piles can tie. That is a limitation and it
/// is stated rather than hidden — a diff that large is a vendored-tree commit,
/// not a review queue.
pub const MAX_HEIGHT: f32 = 64.0;

/// Resting height of a PRD §8 monument — "tall, distinct silhouette".
///
/// # The tension this number resolves
///
/// PRD §7.3 says "the tallest thing on the map is the biggest unreviewed pile —
/// which directly serves *where do I need to look*". PRD §8 says a monument is
/// "tall". Both cannot be literally true, and the first is the product thesis
/// while the second is a wayfinding aid.
///
/// So the monument's height is a **plinth added to the pile**, not a floor under
/// it: `MONUMENT_HEIGHT - BASE_HEIGHT` of extra, applied to whatever the diff
/// says. Three consequences, all of them intended, and
/// `a_pile_out_tops_a_resting_monument` pins the crossover:
///
/// * A monument at rest stands two and a half times an ordinary building at
///   rest, so it reads as an anchor on a clean tree.
/// * **Any pile over about thirty lines out-tops a resting monument**, so the
///   §7.3 reading survives contact with real work.
/// * Between two files with the same pile, the monument is taller — which is
///   what makes it an anchor rather than noise.
///
/// The rest of "distinct silhouette" is carried by [`roof_for_class`] and by the
/// label, which is where PRD §10's "shape encodes what, colour encodes how it
/// went" says identity belongs. Height is a *state* channel here; loading
/// identity into it is the conflation §10 warns about, and the plinth is
/// deliberately the smallest amount of it that still reads.
pub const MONUMENT_HEIGHT: f32 = 2.5;

/// Hours after a pile settles at which [`ghost_lines`] has halved it.
pub const GHOST_HALF_LIFE_HOURS: f32 = 6.0;

/// Days of no commit before PRD §8's overgrowth begins.
///
/// Written as `polis_repo::tree::OVERGROWTH_DAYS` would be if it were a `const`
/// expression here; `overgrowth_onset_matches_polis_repo` asserts they agree, so
/// the two cannot drift into showing different files as dead.
pub const OVERGROWTH_ONSET_DAYS: u32 = 90;

/// Days of no commit at which overgrowth is complete.
///
/// The onset plus two more quarters. A step function at 90 days would make a
/// file that is 89 days old and one that is 91 days old look categorically
/// different, which is not what the underlying evidence supports.
pub const OVERGROWTH_FULL_DAYS: u32 = 270;

// ---------------------------------------------------------------------------
// What a building is made of
// ---------------------------------------------------------------------------

/// Everything about a file that changes its building (PRD §7.3, §8).
///
/// `Copy + Eq` on purpose: nothing here is a clock reading, so a spec can be
/// compared to decide whether a rebuild is needed at all. The slow-decay ghost
/// (PRD §17 Q4) is deliberately **not** a field — it is a function of *now*, and
/// a struct that mixes stored facts with clock-derived ones ends up serialized.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BuildingSpec {
    /// Size on disk. Footprint area is proportional to its square root.
    pub size_bytes: u64,
    /// Uncommitted diff lines, taken as an input (ADR-0042).
    pub diff_lines: u32,
    /// PRD §8's landmark class.
    pub class: FileClass,
}

impl BuildingSpec {
    /// A spec for an ordinary file with a clean working tree.
    #[must_use]
    pub const fn new(size_bytes: u64) -> Self {
        Self {
            size_bytes,
            diff_lines: 0,
            class: FileClass::Ordinary,
        }
    }

    /// The same spec with an uncommitted pile on it.
    #[must_use]
    pub const fn with_diff_lines(mut self, diff_lines: u32) -> Self {
        self.diff_lines = diff_lines;
        self
    }

    /// The same spec with a landmark class.
    #[must_use]
    pub const fn with_class(mut self, class: FileClass) -> Self {
        self.class = class;
        self
    }

    /// True when PRD §8 draws this file as part of a mass rather than as a
    /// building of its own.
    #[must_use]
    pub fn is_massed(&self) -> bool {
        self.class.is_massed()
    }
}

// ---------------------------------------------------------------------------
// Placement
// ---------------------------------------------------------------------------

/// Places a building on a lot.
///
/// Every draw is seeded from `path`, never from a shared stream (PRD §7.4).
/// `block_area` is the enclosing block's area, which is what the PRD §7.3 clamp
/// `[min_lot, block_area * 0.6]` is relative to.
///
/// Returns `None` when the lot cannot hold a building after the setback — a
/// normal outcome for a sliver parcel, and the lot then reads as vacant.
#[must_use]
pub fn place(lot: &Lot, path: &LogicalPath, size_bytes: u64, block_area: f32) -> Option<Building> {
    place_with(lot, path, BuildingSpec::new(size_bytes), block_area)
}

/// [`place`] with the height and landmark class filled in (PRD §7.3, §8).
///
/// Returns `None` for an industrial file: PRD §8 renders those as *one* dull
/// mass, so they get no building at all. That is the same decision
/// [`crate::lots::plan`] makes when it refuses them a lot, arrived at from the
/// other end.
#[must_use]
pub fn place_with(
    lot: &Lot,
    path: &LogicalPath,
    spec: BuildingSpec,
    block_area: f32,
) -> Option<Building> {
    if spec.is_massed() {
        return None;
    }
    let envelope = inset(&lot.boundary, SETBACK)?;
    let envelope_area = envelope.area();
    if !envelope_area.is_finite() || envelope_area <= MIN_FOOTPRINT_AREA {
        return None;
    }

    let wanted = footprint_area(spec.size_bytes, crate::lots::MIN_LOT_AREA, block_area);
    let scale = fill_scale(wanted, envelope_area);
    let rotation = rotation_for(path);
    let centre = envelope.centroid();
    let footprint = transform_about(&envelope, centre, scale, rotation);
    if !footprint.is_valid() {
        return None;
    }

    Some(Building {
        path: path.clone(),
        lot: lot.id,
        footprint,
        height: height_for_class(spec.diff_lines, 0.0, spec.class),
        roof: roof_for_class(path, spec.class),
        rotation,
    })
}

/// The uniform scale that turns an envelope of `have` area into one of `want`.
///
/// Clamped to `1.0`: a building never grows past its lot, whatever the file
/// size says. A file bigger than its parcel is a *full* parcel, not an
/// overlapping one.
#[must_use]
pub fn fill_scale(want: f32, have: f32) -> f32 {
    if !have.is_finite() || have <= 0.0 || !want.is_finite() || want <= 0.0 {
        return 1.0;
    }
    let ratio = f64::from(want) / f64::from(have);
    narrow(ratio.sqrt()).clamp(0.0, 1.0)
}

/// Footprint area for a file, before the lot is consulted (PRD §7.3).
///
/// Proportional to `sqrt(size_bytes)`, clamped to `[min_lot, block_area * 0.6]`.
/// A 50 MB generated file and a 500-byte module must not differ by five orders
/// of magnitude on screen, which is what the square root and the clamp are for.
///
/// Total: a non-finite or non-positive `block_area` drops the ceiling, and a
/// `min_lot` above the ceiling loses to it — a building may never exceed
/// [`BLOCK_AREA_FRACTION`] of its block, whatever a floor says.
#[must_use]
pub fn footprint_area(size_bytes: u64, min_lot: f32, block_area: f32) -> f32 {
    let ceiling = if block_area.is_finite() && block_area > 0.0 {
        narrow(f64::from(block_area) * f64::from(BLOCK_AREA_FRACTION))
    } else {
        f32::MAX
    };
    let floor = if min_lot.is_finite() && min_lot > 0.0 {
        min_lot
    } else {
        MIN_FOOTPRINT_AREA
    };
    let floor = floor.min(ceiling);

    // `u64 -> f64` is lossy above 2^53. A file that large has not been read into
    // memory by anything in this process, and the square root of the rounded
    // value is identical on every IEEE-754 target, which is what matters here.
    #[allow(clippy::cast_precision_loss)]
    let bytes = size_bytes as f64;
    let raw = narrow(f64::from(FOOTPRINT_SCALE) * bytes.sqrt());
    raw.clamp(floor, ceiling)
}

// ---------------------------------------------------------------------------
// Height (PRD §7.3, PRD §17 Q4)
// ---------------------------------------------------------------------------

/// Height for a given uncommitted diff size (PRD §7.3).
///
/// Exactly `height_for(diff_lines, 0.0)` — the flat-on-merge reading the PRD
/// asks for. See the module docs for the slow-decay ghost PRD §17 Q4 leaves
/// open and where it is applied.
#[must_use]
pub fn height_for_diff_lines(diff_lines: u32) -> f32 {
    height_for(diff_lines, 0.0)
}

/// The height curve (PRD §7.3), with PRD §17 Q4's ghost term.
///
/// `ghost_lines` is a *residue* in the same unit as `diff_lines`: what the pile
/// used to be, faded by how long ago it settled (see [`ghost_lines`]). Passing
/// `0.0` gives the PRD's literal behaviour.
///
/// The curve is
///
/// ```text
/// lines      = diff_lines + ghost
/// compressed = lines                       when lines <= KNEE
///            = sqrt(KNEE * lines)          above it
/// height     = BASE + PER_LINE * compressed
/// ```
///
/// **Literally proportional below the knee**, which is what PRD §7.3 asks for
/// and is the regime every edit an operator is watching lives in: the difference
/// between a five-line and a forty-line change is read directly off the height.
/// Above it the same square root PRD §7.3 already applies to file size takes
/// over, so one enormous generated-file churn does not flatten the rest of the
/// skyline. The two halves agree exactly at `lines == KNEE`, and the whole curve
/// is strictly increasing up to [`MAX_HEIGHT`], so the tallest thing on the map
/// really is the biggest unreviewed pile.
///
/// No `exp`, no `ln`, no `powf` — this value is serialized into
/// [`crate::CityLayout`] and compared byte for byte across two operating systems
/// (PRD §16), and transcendental functions are not required to be correctly
/// rounded ([`crate::determinism`], rule 4). `sqrt` **is** covered: IEEE-754
/// requires it to be correctly rounded, which is why it is the one root the
/// crate is allowed to take.
#[must_use]
pub fn height_for(diff_lines: u32, ghost_lines: f32) -> f32 {
    let ghost = if ghost_lines.is_finite() && ghost_lines > 0.0 {
        f64::from(ghost_lines)
    } else {
        0.0
    };
    let lines = f64::from(diff_lines) + ghost;
    let knee = f64::from(HEIGHT_KNEE_LINES);
    let compressed = if lines <= knee {
        lines
    } else {
        (knee * lines).sqrt()
    };
    let height = f64::from(BASE_HEIGHT) + f64::from(HEIGHT_PER_LINE) * compressed;
    narrow(height).clamp(BASE_HEIGHT, MAX_HEIGHT)
}

/// [`height_for`] with PRD §8's monument plinth added.
///
/// A monument stands on `MONUMENT_HEIGHT - BASE_HEIGHT` of plinth, **added** to
/// whatever its diff says rather than substituted for it. See
/// [`MONUMENT_HEIGHT`] for why that is a plinth and not a floor.
///
/// [`FileClass::CivicSquare`] gets no special height, and that is a decision
/// rather than an omission. PRD §8 wants "a recognisable open space at the
/// historic centre", and the obvious reading — clamp the height of root-level
/// config so it never towers — would hide a four-hundred-line uncommitted change
/// to a CI workflow or a lockfile, which is exactly a pile an operator needs to
/// see. The open space is a *district*-level rendering decision, not a clamp on
/// a building.
#[must_use]
pub fn height_for_class(diff_lines: u32, ghost_lines: f32, class: FileClass) -> f32 {
    let base = height_for(diff_lines, ghost_lines);
    match class {
        FileClass::Monument => (base + (MONUMENT_HEIGHT - BASE_HEIGHT)).min(MAX_HEIGHT),
        FileClass::Ordinary | FileClass::Industrial | FileClass::CivicSquare => base,
    }
}

/// The slow-decay ghost of a settled pile, in diff lines (PRD §17 Q4).
///
/// `lines` is how big the pile was when it settled — the last non-zero diff
/// count seen for the file — and `settled` is when that happened. The result
/// feeds [`height_for`]'s second argument.
///
/// The fall-off is `lines / (1 + hours / HALF_LIFE)` rather than an exponential:
/// it halves at [`GHOST_HALF_LIFE_HOURS`], is monotone, reaches zero
/// asymptotically rather than by clamping, and contains no transcendental
/// function — so a future decision to *store* the ghost would not break PRD
/// §16's byte comparison (see the module docs).
///
/// Total: a `now` before `settled` — a clock that went backwards, which
/// ADR-0014 says to expect — yields the undecayed value rather than a negative
/// one or a panic.
#[must_use]
pub fn ghost_lines(lines: u32, settled: WallTime, now: WallTime) -> f32 {
    if lines == 0 {
        return 0.0;
    }
    let elapsed = now.duration_since(settled).unwrap_or_default();
    let hours = elapsed.as_secs_f64() / 3600.0;
    let half = f64::from(GHOST_HALF_LIFE_HOURS).max(f64::EPSILON);
    narrow(f64::from(lines) / (1.0 + hours / half))
}

// ---------------------------------------------------------------------------
// Silhouette (PRD §7.3, §8)
// ---------------------------------------------------------------------------

/// Roof form from a hash of the path (PRD §7.3).
///
/// Purpose tag `"roof"`, so it is an independent stream from the rotation draw.
#[must_use]
pub fn roof_for(path: &LogicalPath) -> RoofForm {
    let mut rng = SeededRng::for_path(path, "roof");
    rng.choose(&RoofForm::ALL)
        .copied()
        .unwrap_or(RoofForm::Flat)
}

/// [`roof_for`], with PRD §8's monument silhouette pinned.
///
/// > Tall, distinct silhouette, always labelled at every zoom. These are the
/// > orientation anchors — the first thing the eye finds when zoomed out.
///
/// Monuments are deliberately **uniform** rather than varied: at the zoom where
/// the whole city fits on screen an individual roof does not resolve, and a
/// consistent stepped profile is what lets the eye pick the anchors out of the
/// skyline. Variety is what the other 99% of buildings are for.
#[must_use]
pub fn roof_for_class(path: &LogicalPath, class: FileClass) -> RoofForm {
    match class {
        FileClass::Monument => RoofForm::Stepped,
        FileClass::Ordinary | FileClass::Industrial | FileClass::CivicSquare => roof_for(path),
    }
}

/// Rotation in radians, within ±[`MAX_ROTATION_DEGREES`] (PRD §7.2 step 5).
///
/// Purpose tag `"rotation"`.
#[must_use]
pub fn rotation_for(path: &LogicalPath) -> f32 {
    let mut rng = SeededRng::for_path(path, "rotation");
    let limit = f64::from(MAX_ROTATION_DEGREES);
    let degrees = rng.range_f64(-limit, limit);
    narrow(degrees * DEGREES_TO_RADIANS)
}

/// `π / 180`, as one named constant rather than three call sites.
const DEGREES_TO_RADIANS: f64 = std::f64::consts::PI / 180.0;

// ---------------------------------------------------------------------------
// Decay (PRD §7.5, §8)
// ---------------------------------------------------------------------------

/// How overgrown a building is, `0.0` to `1.0` (PRD §7.5, §8).
///
/// > Files untouched for a long window grow **overgrowth**. […] Desaturated,
/// > softened outline, encroaching vegetation texture.
///
/// Zero until [`OVERGROWTH_ONSET_DAYS`], then a linear ramp to one at
/// [`OVERGROWTH_FULL_DAYS`]. A ramp rather than
/// `polis_repo::tree::is_overgrown`'s boolean, because the boolean is the
/// *classification* and this is the *rendering*: a file at 89 days and one at 91
/// days differ by two days of evidence and should not differ categorically on
/// screen.
///
/// `now` is a parameter, never a clock read: this is a rendering input, and a
/// layout that changed with the date would break PRD §7.4 (the same rule
/// `polis_repo::tree::is_overgrown` follows).
#[must_use]
pub fn overgrowth(last_touched: WallTime, now: WallTime) -> f32 {
    overgrowth_over(
        last_touched,
        now,
        OVERGROWTH_ONSET_DAYS,
        OVERGROWTH_FULL_DAYS,
    )
}

/// [`overgrowth`] over a caller's window.
///
/// A `full` at or below `onset` makes the ramp a step at `onset`, which is the
/// useful reading rather than a division by zero.
#[must_use]
pub fn overgrowth_over(last_touched: WallTime, now: WallTime, onset: u32, full: u32) -> f32 {
    let days = last_touched.days_until(now);
    if days < onset {
        return 0.0;
    }
    if full <= onset {
        return 1.0;
    }
    let progress = f64::from(days - onset) / f64::from(full - onset);
    narrow(progress.clamp(0.0, 1.0))
}

// ---------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------

/// A polygon inset by `distance`, or `None` when nothing is left (PRD §7.2).
///
/// > **Buildings** are lots inset by a setback.
///
/// Straight-line edge offsetting: each edge's supporting line is pushed
/// `distance` towards the interior and consecutive offset lines are intersected.
/// That is exact for a convex parcel and correct for the mildly concave ones a
/// half-plane subdivision produces.
///
/// It is also the standard way to produce a self-intersecting polygon on a
/// reflex vertex, so the result is **validated** — positive area, no larger than
/// the original, every vertex finite — and a failure falls back to a uniform
/// shrink about the centroid by `1 - distance / (2·area / perimeter)`. The
/// fallback is exact for a regular polygon and never self-intersects, which is
/// the property that matters: a building that folds through itself is a
/// triangulation crash in `lyon` three crates downstream, not a visual blemish.
///
/// The result is wound counter-clockwise whatever the input was, so the whole
/// crate hands `lyon` one orientation.
#[must_use]
pub fn inset(polygon: &Polygon, distance: f32) -> Option<Polygon> {
    if !polygon.is_valid() {
        return None;
    }
    let mut source = polygon.clone();
    if !source.is_ccw() {
        source.reverse();
    }
    if !distance.is_finite() || distance <= 0.0 {
        return Some(source);
    }

    let area_before = f64::from(source.area());
    if let Some(candidate) = offset_edges(&source, f64::from(distance)) {
        if offset_survived(&source, &candidate, area_before) {
            return Some(candidate);
        }
    }
    shrink_about_centroid(&source, f64::from(distance))
}

/// Whether a straight-line offset is still the same shape, smaller.
///
/// The test that matters is **edge direction**, not winding. Inset a square far
/// enough and the four offset lines cross into a square rotated by half a turn —
/// which is still counter-clockwise, still convex, still has positive area, and
/// is completely wrong. A rotation by half a turn reverses every edge, so
/// requiring each offset edge to still point the same way as the edge it came
/// from catches it, and catches the reflex-vertex fold for the same reason.
fn offset_survived(source: &Polygon, candidate: &Polygon, area_before: f64) -> bool {
    if !candidate.is_valid() || candidate.vertices.len() != source.vertices.len() {
        return false;
    }
    let area = f64::from(candidate.area());
    if !area.is_finite() || area <= 0.0 || area > area_before || !candidate.is_ccw() {
        return false;
    }
    for (before, after) in source.edges().zip(candidate.edges()) {
        let old = before.1 - before.0;
        let new = after.1 - after.0;
        let along = f64::from(old.x) * f64::from(new.x) + f64::from(old.y) * f64::from(new.y);
        if !along.is_finite() || along <= 0.0 {
            return false;
        }
    }
    source.contains(candidate.centroid())
}

/// The straight-line offset, before validation. `None` on a degenerate edge run.
fn offset_edges(polygon: &Polygon, distance: f64) -> Option<Polygon> {
    let n = polygon.vertices.len();
    if n < 3 {
        return None;
    }
    // Per edge `i` (from vertex `i` to vertex `i+1`): a point on the inward
    // offset line, and the edge direction. For a counter-clockwise polygon the
    // interior is to the left, so the inward normal of `(dx, dy)` is `(-dy, dx)`.
    let mut lines: Vec<(f64, f64, f64, f64)> = Vec::with_capacity(n);
    for i in 0..n {
        let a = polygon.vertices[i];
        let b = polygon.vertices[(i + 1) % n];
        let dx = f64::from(b.x) - f64::from(a.x);
        let dy = f64::from(b.y) - f64::from(a.y);
        let length = (dx * dx + dy * dy).sqrt();
        if !length.is_finite() || length <= 0.0 {
            return None;
        }
        let (ux, uy) = (dx / length, dy / length);
        let (nx, ny) = (-uy, ux);
        lines.push((
            f64::from(a.x) + nx * distance,
            f64::from(a.y) + ny * distance,
            ux,
            uy,
        ));
    }

    let mut vertices = Vec::with_capacity(n);
    for i in 0..n {
        // Vertex `i` is where the offset of edge `i-1` meets the offset of edge
        // `i`, which is the corner both of them share.
        let (px, py, ux, uy) = lines[(i + n - 1) % n];
        let (qx, qy, vx, vy) = lines[i];
        let denominator = ux * vy - uy * vx;
        let point = if denominator.abs() < PARALLEL_EPSILON {
            // Collinear or near-collinear edges: the two offset lines coincide,
            // so the corner simply slides along with them.
            Point::new(narrow(qx), narrow(qy))
        } else {
            let t = ((qx - px) * vy - (qy - py) * vx) / denominator;
            Point::new(narrow(px + ux * t), narrow(py + uy * t))
        };
        if !point.is_finite() {
            return None;
        }
        vertices.push(point);
    }
    Some(Polygon::new(vertices))
}

/// How near-parallel two edges may be before their offset intersection is
/// abandoned. `sin` of about 0.006°.
const PARALLEL_EPSILON: f64 = 1.0e-7;

/// A uniform shrink about the centroid that removes `distance` of average
/// boundary clearance. `None` when nothing survives.
fn shrink_about_centroid(polygon: &Polygon, distance: f64) -> Option<Polygon> {
    let area = f64::from(polygon.area());
    let perimeter = perimeter(polygon);
    if !area.is_finite() || area <= 0.0 || !perimeter.is_finite() || perimeter <= 0.0 {
        return None;
    }
    // `2A/P` is the inradius of a regular polygon and a good stand-in for the
    // average centroid-to-edge distance of an irregular one.
    let inradius = 2.0 * area / perimeter;
    if inradius <= distance {
        return None;
    }
    let scale = narrow((inradius - distance) / inradius);
    let shrunk = transform_about(polygon, polygon.centroid(), scale, 0.0);
    if shrunk.is_valid() && shrunk.area() > 0.0 {
        Some(shrunk)
    } else {
        None
    }
}

/// Total edge length, including the closing edge.
#[must_use]
pub fn perimeter(polygon: &Polygon) -> f64 {
    let mut total = 0.0_f64;
    for (a, b) in polygon.edges() {
        let dx = f64::from(b.x) - f64::from(a.x);
        let dy = f64::from(b.y) - f64::from(a.y);
        total += (dx * dx + dy * dy).sqrt();
    }
    total
}

/// Scales then rotates a polygon about a point (PRD §7.2 step 5).
///
/// One function rather than two, so the two transforms are applied in one
/// documented order with one rounding per coordinate — composing two separate
/// passes would round twice and put the golden files at the mercy of the order
/// a caller happened to pick.
///
/// The rotation goes through [`det_sin_cos`], not `f32::sin_cos`: a building's
/// rotation is serialized and compared across two operating systems, and `sin`
/// is not required by IEEE-754 to be correctly rounded
/// ([`crate::determinism`], rule 4).
#[must_use]
pub fn transform_about(polygon: &Polygon, about: Point, scale: f32, rotation: f32) -> Polygon {
    let (sin, cos) = det_sin_cos(f64::from(rotation));
    let s = f64::from(scale);
    let (cx, cy) = (f64::from(about.x), f64::from(about.y));
    let vertices = polygon
        .vertices
        .iter()
        .map(|v| {
            let dx = (f64::from(v.x) - cx) * s;
            let dy = (f64::from(v.y) - cy) * s;
            Point::new(
                narrow(cx + (dx * cos - dy * sin)),
                narrow(cy + (dx * sin + dy * cos)),
            )
        })
        .collect();
    Polygon::new(vertices)
}

#[cfg(test)]
mod tests {
    // Determinism assertions here are exact and bit-level on purpose (PRD §7.4,
    // §16); `float_cmp` exists to catch approximate equality written as `==`,
    // which is the opposite of what these tests are for.
    #![allow(clippy::float_cmp)]

    use super::*;
    use crate::{BlockId, LotId};

    fn lp(s: &str) -> LogicalPath {
        LogicalPath::new(s).expect("test path")
    }

    fn square(size: f32) -> Polygon {
        Polygon::new(vec![
            Point::new(0.0, 0.0),
            Point::new(size, 0.0),
            Point::new(size, size),
            Point::new(0.0, size),
        ])
    }

    /// `Duration::from_hours` is unstable on the pinned toolchain (1.98), and
    /// clippy's `duration_suboptimal_units` suggestion is therefore unusable.
    /// A named helper is clearer than the literal it replaces anyway.
    fn hours(n: u64) -> std::time::Duration {
        std::time::Duration::from_secs(n * 3_600)
    }

    /// See [`hours`].
    fn days(n: u64) -> std::time::Duration {
        hours(n * 24)
    }

    fn lot(boundary: Polygon) -> Lot {
        Lot {
            id: LotId(7),
            block: BlockId(3),
            boundary,
            occupant: None,
        }
    }

    // -----------------------------------------------------------------------
    // Footprint (PRD §7.3)
    // -----------------------------------------------------------------------

    #[test]
    fn footprint_area_follows_the_square_root_of_size() {
        // Four times the bytes is twice the footprint, which is the whole point
        // of the square root: a 50 MB generated file and a 500-byte module must
        // not differ by five orders of magnitude on screen.
        let small = footprint_area(1_024, 0.0, f32::MAX);
        let large = footprint_area(4_096, 0.0, f32::MAX);
        assert!((large / small - 2.0).abs() < 1e-4, "{small} -> {large}");
    }

    #[test]
    fn footprint_area_is_clamped_at_both_ends() {
        // A one-byte file still gets the floor.
        assert_eq!(footprint_area(1, 3.0, 1_000.0), 3.0);
        // A 50 MB file is capped at 60% of its block.
        assert_eq!(footprint_area(50_000_000, 1.0, 10.0), 6.0);
        // The block ceiling beats a floor that exceeds it: a building may never
        // cover more than 60% of its block, whatever a caller's floor says.
        assert_eq!(footprint_area(1, 100.0, 10.0), 6.0);
    }

    #[test]
    fn footprint_area_survives_a_degenerate_block() {
        for block in [0.0_f32, -1.0, f32::NAN, f32::INFINITY] {
            let area = footprint_area(4_096, 2.0, block);
            assert!(area.is_finite() && area > 0.0, "block_area = {block}");
        }
        assert!(footprint_area(0, 2.0, 100.0).is_finite());
    }

    // -----------------------------------------------------------------------
    // Height (PRD §7.3, PRD §17 Q4)
    // -----------------------------------------------------------------------

    #[test]
    fn height_rises_with_the_pile_and_never_ties() {
        let mut previous = height_for_diff_lines(0);
        assert_eq!(previous, BASE_HEIGHT, "a clean file is still a building");
        for lines in [1_u32, 5, 20, 40, 41, 200, 1_000, 10_000, 20_000] {
            let height = height_for_diff_lines(lines);
            assert!(
                height > previous,
                "{lines} lines is not taller than the step before it: \
                 {previous} -> {height}. PRD §7.3: the tallest thing on the map \
                 is the biggest unreviewed pile."
            );
            assert!(height <= MAX_HEIGHT);
            previous = height;
        }
    }

    #[test]
    fn the_knee_keeps_a_huge_churn_from_flattening_the_skyline() {
        // A 40-line edit is a normal unit of work and must be plainly visible
        // next to base height; a 4 000-line churn must not be a hundred times
        // taller than it, or nothing else is legible while it exists.
        let base = height_for_diff_lines(0);
        let normal = height_for_diff_lines(40);
        let huge = height_for_diff_lines(4_000);
        assert!(normal - base > 1.0, "a normal edit is visible: {normal}");
        // 7x, against the 100x an uncompressed curve would give.
        assert!(
            huge / normal < 10.0,
            "a huge churn is {}x a normal one; above about 10x nothing else on \
             the map is legible while one exists",
            huge / normal
        );
    }

    /// PRD §17 open question 4. The ghost must slot into the *same* curve, or
    /// the product has two height functions that disagree.
    #[test]
    fn the_ghost_is_the_same_curve_with_a_second_term() {
        assert_eq!(height_for_diff_lines(40), height_for(40, 0.0));
        // Merging flattens the building...
        let working = height_for_diff_lines(120);
        let merged = height_for_diff_lines(0);
        assert!(merged < working);
        // ...and the ghost is what keeps "recently active" readable, decaying to
        // nothing rather than being switched off.
        let settled = WallTime::from_unix_seconds(1_700_000_000);
        let fresh = ghost_lines(120, settled, settled);
        let later = ghost_lines(120, settled, settled.saturating_add(hours(6)));
        let much_later = ghost_lines(120, settled, settled.saturating_add(hours(240)));
        assert_eq!(
            fresh, 120.0,
            "an instant after the merge, nothing has faded"
        );
        assert!(
            (later - 60.0).abs() < 1e-3,
            "one half-life is half the pile: {later}"
        );
        assert!(much_later < 4.0, "the ghost fades away: {much_later}");
        assert!(height_for(0, fresh) > merged);
        assert!(height_for(0, much_later) < height_for(0, later));
    }

    #[test]
    fn a_backwards_clock_does_not_produce_a_negative_ghost() {
        let settled = WallTime::from_unix_seconds(1_700_000_000);
        let earlier = WallTime::from_unix_seconds(1_600_000_000);
        assert_eq!(ghost_lines(50, settled, earlier), 50.0);
        assert_eq!(ghost_lines(0, settled, settled), 0.0);
        assert!(height_for(10, f32::NAN).is_finite());
        assert!(height_for(10, -5.0).is_finite());
    }

    #[test]
    fn landmark_classes_get_their_plinth_and_nothing_else() {
        // PRD §8: a monument is an orientation anchor before it is a file.
        assert_eq!(
            height_for_class(0, 0.0, FileClass::Monument),
            MONUMENT_HEIGHT
        );
        assert!(height_for_class(10_000, 0.0, FileClass::Monument) > MONUMENT_HEIGHT);
        // The civic square deliberately gets no clamp: a 400-line uncommitted
        // change to root config is a pile an operator needs to see.
        assert_eq!(
            height_for_class(400, 0.0, FileClass::CivicSquare),
            height_for_diff_lines(400)
        );
        assert_eq!(
            height_for_class(37, 0.0, FileClass::Ordinary),
            height_for_diff_lines(37)
        );
    }

    /// PRD §7.3 asks for height "∝ uncommitted diff lines". Below the knee that
    /// is literal, which is the regime an operator actually reads.
    #[test]
    fn height_is_literally_proportional_below_the_knee() {
        let base = height_for_diff_lines(0);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let knee = HEIGHT_KNEE_LINES as u32;
        for lines in 1..=knee {
            #[allow(clippy::cast_precision_loss)]
            let expected = base + HEIGHT_PER_LINE * lines as f32;
            let actual = height_for_diff_lines(lines);
            assert!(
                (actual - expected).abs() < 1e-5,
                "{lines} lines: {actual} vs {expected}"
            );
        }
        // The two halves of the curve agree at the knee, and it keeps climbing.
        assert!(height_for_diff_lines(knee + 1) > height_for_diff_lines(knee));
    }

    /// The resolution of PRD §7.3 against PRD §8, pinned.
    ///
    /// > The tallest thing on the map is the biggest unreviewed pile.
    ///
    /// A monument at rest is an anchor; a real pile out-tops it.
    #[test]
    fn a_pile_out_tops_a_resting_monument() {
        let resting = height_for_class(0, 0.0, FileClass::Monument);
        assert_eq!(resting, MONUMENT_HEIGHT);
        assert!(
            resting > height_for_class(0, 0.0, FileClass::Ordinary) * 2.0,
            "a monument does not read as an anchor at rest"
        );

        // The crossover, stated rather than hoped for.
        let crossover = (1..200)
            .find(|lines| height_for_diff_lines(*lines) > resting)
            .expect("some pile out-tops a monument");
        assert_eq!(
            crossover, 31,
            "the pile/monument crossover moved; PRD §7.3's reading depends on it \
             being small enough that ordinary work wins"
        );

        // And between equals, the monument is still the taller one.
        assert!(
            height_for_class(400, 0.0, FileClass::Monument)
                > height_for_class(400, 0.0, FileClass::Ordinary)
        );
    }

    // -----------------------------------------------------------------------
    // Silhouette and rotation (PRD §7.2 step 5, §7.3, §7.4)
    // -----------------------------------------------------------------------

    /// PRD §7.4. If this ever fails, every golden layout file is invalidated on
    /// purpose (ADR-0029).
    #[test]
    fn roof_and_rotation_are_pinned_to_literal_values() {
        assert_eq!(roof_for(&lp("src/main.rs")), RoofForm::Flat);
        assert_eq!(roof_for(&lp("src/lib.rs")), RoofForm::Pitched);
        assert_eq!(roof_for(&lp("README.md")), RoofForm::Flat);
        assert_eq!(roof_for(&lp("polis-layout/src/city.rs")), RoofForm::Flat);

        assert_eq!(rotation_for(&lp("src/main.rs")).to_bits(), 0x3d10_891c);
        assert_eq!(rotation_for(&lp("src/lib.rs")).to_bits(), 0xbc37_41d5);
        assert_eq!(rotation_for(&lp("README.md")).to_bits(), 0x3d3f_4b5b);
        assert_eq!(
            rotation_for(&lp("polis-layout/src/city.rs")).to_bits(),
            0x3cb0_e384
        );
    }

    #[test]
    fn rotation_stays_inside_four_degrees() {
        let limit = MAX_ROTATION_DEGREES.to_radians();
        let mut seen_positive = false;
        let mut seen_negative = false;
        for i in 0..500 {
            let path = lp(&format!("src/module{i}/file{i}.rs"));
            let r = rotation_for(&path);
            assert!(r.abs() <= limit + 1e-6, "{path:?} rotated {r}");
            seen_positive |= r > 0.0;
            seen_negative |= r < 0.0;
        }
        assert!(seen_positive && seen_negative, "rotation must go both ways");
    }

    #[test]
    fn every_roof_form_is_reachable_and_the_mix_is_not_degenerate() {
        let mut counts = [0_u32; 3];
        for i in 0..900 {
            let path = lp(&format!("src/dir{}/file{i}.rs", i % 17));
            let index = RoofForm::ALL
                .iter()
                .position(|f| *f == roof_for(&path))
                .expect("a shipped form");
            counts[index] += 1;
        }
        for (i, count) in counts.iter().enumerate() {
            assert!(
                *count > 180,
                "roof form {:?} appears {count} times in 900; silhouette variety \
                 carries most of the organic reading (PRD §7.3)",
                RoofForm::ALL[i]
            );
        }
    }

    /// Rotation and roof must be independent streams, or a change to roof
    /// selection rotates every building in the city (PRD §7.4).
    #[test]
    fn roof_and_rotation_are_independent_streams() {
        // Two paths whose roofs agree must not have correlated rotations. The
        // cheap structural check: the purpose tags differ, so the seeds differ.
        let path = lp("src/auth/session.rs");
        let roof_seed = crate::determinism::seed_for_path(&path, "roof");
        let rotation_seed = crate::determinism::seed_for_path(&path, "rotation");
        assert_ne!(roof_seed, rotation_seed);
    }

    #[test]
    fn monuments_get_one_recognisable_silhouette() {
        // Whatever the hash says, a monument is stepped: at city zoom the roof
        // does not resolve and the profile is the wayfinding signal (PRD §8).
        for name in ["src/main.rs", "src/lib.rs", "polis-layout/src/city.rs"] {
            let path = lp(name);
            assert_eq!(
                roof_for_class(&path, FileClass::Monument),
                RoofForm::Stepped
            );
            assert_eq!(roof_for_class(&path, FileClass::Ordinary), roof_for(&path));
        }
    }

    // -----------------------------------------------------------------------
    // Inset and placement (PRD §7.2 step 5)
    // -----------------------------------------------------------------------

    #[test]
    fn a_square_insets_to_a_smaller_concentric_square() {
        let inner = inset(&square(10.0), 1.0).expect("a 10x10 lot survives a 1 setback");
        assert!((inner.area() - 64.0).abs() < 1e-3, "{}", inner.area());
        let c = inner.centroid();
        assert!((c.x - 5.0).abs() < 1e-3 && (c.y - 5.0).abs() < 1e-3);
        assert!(inner.is_ccw(), "one winding for the whole crate");
    }

    #[test]
    fn a_clockwise_lot_insets_the_same_way() {
        let mut cw = square(10.0);
        cw.reverse();
        let a = inset(&square(10.0), 1.0).expect("ccw");
        let b = inset(&cw, 1.0).expect("cw");
        assert!((a.area() - b.area()).abs() < 1e-3);
        assert!(b.is_ccw());
    }

    #[test]
    fn a_sliver_lot_has_no_building() {
        assert!(inset(&square(0.5), SETBACK).is_none());
        assert!(inset(&Polygon::default(), SETBACK).is_none());
        assert!(inset(
            &Polygon::new(vec![Point::ORIGIN, Point::new(1.0, 0.0)]),
            0.1
        )
        .is_none());
        assert!(place(&lot(square(0.5)), &lp("a.rs"), 1_000, 100.0).is_none());
    }

    /// The reflex-vertex case that makes naive edge offsetting fold a polygon
    /// through itself. A fold is a `lyon` triangulation crash three crates
    /// downstream, so it must degrade to a valid shape instead.
    #[test]
    fn a_concave_lot_never_folds_through_itself() {
        let l = Polygon::new(vec![
            Point::new(0.0, 0.0),
            Point::new(10.0, 0.0),
            Point::new(10.0, 3.0),
            Point::new(3.0, 3.0),
            Point::new(3.0, 10.0),
            Point::new(0.0, 10.0),
        ]);
        let inner = inset(&l, 0.5).expect("an L-shaped lot is buildable");
        assert!(inner.is_valid());
        assert!(inner.is_ccw());
        assert!(inner.area() > 0.0 && inner.area() < l.area());
        for v in &inner.vertices {
            assert!(v.is_finite());
        }

        // A setback deep enough to consume the arms yields no building rather
        // than an inside-out one.
        let deep = inset(&l, 2.0);
        if let Some(deep) = deep {
            assert!(deep.is_valid() && deep.area() > 0.0 && deep.area() < l.area());
        }
    }

    #[test]
    fn a_large_file_fills_its_lot_and_a_small_one_sits_inside_it() {
        let parcel = lot(square(6.0));
        let envelope = inset(&parcel.boundary, SETBACK).expect("buildable");
        let big = place(&parcel, &lp("src/big.rs"), 4_000_000, 400.0).expect("a building");
        let small = place(&parcel, &lp("src/small.rs"), 900, 400.0).expect("a building");
        assert!(big.footprint.area() > small.footprint.area());
        assert!(
            big.footprint.area() <= envelope.area() + 1e-3,
            "a building never grows past its lot"
        );
        assert!(
            small.footprint.area() < envelope.area(),
            "a small file leaves open ground"
        );
    }

    #[test]
    fn a_building_carries_its_lot_and_its_path() {
        let parcel = lot(square(6.0));
        let path = lp("src/auth/session.rs");
        let b = place(&parcel, &path, 8_000, 400.0).expect("a building");
        assert_eq!(b.lot, parcel.id);
        assert_eq!(b.path, path);
        assert_eq!(b.roof, roof_for(&path));
        assert_eq!(b.rotation, rotation_for(&path));
        assert_eq!(b.height, BASE_HEIGHT, "a clean tree is a flat city");
        assert!(b.footprint.is_valid());
    }

    /// PRD §8: `node_modules`, vendored, generated and `target/` are drawn as
    /// one dull mass, not as individual buildings. `lots::plan` refuses them a
    /// lot; this is the same decision from the other end.
    #[test]
    fn an_industrial_file_gets_no_building_at_all() {
        let parcel = lot(square(8.0));
        let spec = BuildingSpec::new(4_000).with_class(FileClass::Industrial);
        assert!(place_with(&parcel, &lp("node_modules/left-pad/index.js"), spec, 400.0).is_none());
        assert!(spec.is_massed());

        let ordinary = BuildingSpec::new(4_000);
        assert!(place_with(&parcel, &lp("src/a.rs"), ordinary, 400.0).is_some());
        assert!(!ordinary.is_massed());
    }

    #[test]
    fn a_monument_is_tall_and_stepped() {
        let parcel = lot(square(8.0));
        let path = lp("src/main.rs");
        let spec = BuildingSpec::new(4_000).with_class(FileClass::Monument);
        let b = place_with(&parcel, &path, spec, 400.0).expect("a building");
        assert_eq!(b.roof, RoofForm::Stepped);
        assert!(b.height >= MONUMENT_HEIGHT);
    }

    /// The property PRD §7.4 exists for: same inputs, same building, bit for
    /// bit, however many times it is asked.
    #[test]
    fn placement_is_bit_identical_across_repeated_calls() {
        let parcel = lot(square(7.0));
        let path = lp("src/auth/tokens.rs");
        let spec = BuildingSpec::new(12_345).with_diff_lines(88);
        let first = place_with(&parcel, &path, spec, 250.0).expect("a building");
        for _ in 0..16 {
            let again = place_with(&parcel, &path, spec, 250.0).expect("a building");
            assert_eq!(first, again);
        }
    }

    #[test]
    fn transform_about_is_a_scale_then_a_rotation_about_one_point() {
        let p = square(10.0);
        let centre = p.centroid();
        let half = transform_about(&p, centre, 0.5, 0.0);
        assert!((half.area() - 25.0).abs() < 1e-3);
        assert!((half.centroid().x - 5.0).abs() < 1e-3);

        // A rotation preserves area and the point it turns about.
        let turned = transform_about(&p, centre, 1.0, 0.5);
        assert!((turned.area() - 100.0).abs() < 1e-2);
        assert!((turned.centroid().x - 5.0).abs() < 1e-3);
        assert!((turned.centroid().y - 5.0).abs() < 1e-3);
        assert_ne!(turned.vertices[0], p.vertices[0]);
    }

    #[test]
    fn fill_scale_never_grows_a_building_past_its_lot() {
        assert_eq!(fill_scale(100.0, 25.0), 1.0);
        assert!((fill_scale(25.0, 100.0) - 0.5).abs() < 1e-6);
        assert_eq!(fill_scale(10.0, 0.0), 1.0);
        assert_eq!(fill_scale(f32::NAN, 10.0), 1.0);
    }

    #[test]
    fn perimeter_is_the_closed_boundary_length() {
        assert!((perimeter(&square(10.0)) - 40.0).abs() < 1e-6);
        assert_eq!(perimeter(&Polygon::default()), 0.0);
    }

    // -----------------------------------------------------------------------
    // Decay (PRD §7.5, §8)
    // -----------------------------------------------------------------------

    #[test]
    fn overgrowth_ramps_after_the_onset_rather_than_switching_on() {
        let touched = WallTime::from_unix_seconds(1_600_000_000);
        let at = |n: u64| touched.saturating_add(days(n));

        assert_eq!(overgrowth(touched, at(0)), 0.0);
        assert_eq!(overgrowth(touched, at(89)), 0.0);
        assert_eq!(overgrowth(touched, at(90)), 0.0, "the ramp starts at zero");
        let mid = overgrowth(touched, at(180));
        assert!((mid - 0.5).abs() < 0.01, "{mid}");
        assert_eq!(overgrowth(touched, at(270)), 1.0);
        assert_eq!(overgrowth(touched, at(3_650)), 1.0);
    }

    #[test]
    fn a_degenerate_overgrowth_window_is_a_step_not_a_division_by_zero() {
        let touched = WallTime::from_unix_seconds(1_600_000_000);
        let later = touched.saturating_add(days(200));
        assert_eq!(overgrowth_over(touched, later, 90, 90), 1.0);
        assert_eq!(overgrowth_over(touched, later, 90, 10), 1.0);
        assert_eq!(overgrowth_over(touched, later, 500, 900), 0.0);
    }

    /// Two thresholds for the same product concept in two crates is how a file
    /// ends up classified dead in one layer and alive in another.
    #[test]
    fn overgrowth_onset_matches_polis_repo() {
        assert_eq!(OVERGROWTH_ONSET_DAYS, polis_repo::tree::OVERGROWTH_DAYS);
        let touched = WallTime::from_unix_seconds(1_600_000_000);
        let boundary = touched.saturating_add(days(90));
        assert!(polis_repo::tree::is_overgrown(touched, boundary));
        assert_eq!(overgrowth(touched, boundary), 0.0);
    }
}
