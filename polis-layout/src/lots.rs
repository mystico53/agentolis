//! Stage 4 — lots, and PRD §7.5's vacancy record.
//!
//! > **Lots** by recursive subdivision of each block along its longest axis
//! > until lot area falls under target. Irregular blocks give irregular lots for
//! > free.
//!
//! Once an axis is chosen it is **reused for the children** until the strip
//! stops being the long one. That is what turns a block into a row of deep,
//! narrow plots facing the street rather than a quad-tree of squares, and it is
//! the difference between a plan that reads as a town and one that reads as
//! graph paper.
//!
//! # The plot is the size of the file that stands on it
//!
//! > **Footprint area** ∝ `sqrt(file_size_bytes)` (PRD §7.3)
//!
//! A footprint can only be as big as the ground under it, so PRD §7.3's driver
//! is read **here**, not in [`crate::buildings`]. Every block is cut by
//! `subdivide_weighted`, which carries the block's own files down the
//! recursion as a weight sequence — `sqrt(bytes)` normalised against the block's
//! mean and clamped to [`WEIGHT_SPREAD`] either way — splitting the sequence
//! where the two halves weigh most nearly the same and cutting the ground at the
//! area fraction that split asks for. The recursion bottoms out at exactly one
//! item per parcel.
//!
//! Equal-area subdivision is what made every building in a block the same size,
//! and that single fact is what the fresh-eyes review named as the reason every
//! crop of the M1 render read as gravel: measured on the 5 000-file fixture, the
//! median block's buildings spanned **1.64×** from p10 to p90 and the whole
//! city's spanned 6.1×. Weighted, the same fixture gives **4.2×** and 18.5×.
//!
//! # The age of the ground, as density
//!
//! > Files added in the repo's first year form the old town — dense, tangled,
//! > irregular. Files added last month sit on the periphery and look more
//! > planned. (PRD §7.1)
//!
//! Two things are graded by `urbanity` — a blend of the block's own growth
//! order and its distance from the civic square (see [`RADIAL_WEIGHT`]):
//!
//! * **plot size**, through the parcel cap, which is the design bake-off
//!   judge's fifth required change (vary the grain "by district age *and* by
//!   district file count, not by growth-sequence fraction alone");
//! * **how much of a block is surveyed at all**, through
//!   [`LOT_SLACK_CORE`]/[`LOT_SLACK_RIM`]. The old town is built out, one plot
//!   per file; the rim keeps roughly a third of its ground as vacant paddocks.
//!
//! That second term is the one the eye reads. Measured at 5 000 files, coverage
//! in the innermost third of the city against the outermost went from
//! **28.9 % / 32.7 %** — a gradient pointing the *wrong way*, which is why the
//! render read as one uniform texture from centre to rim — to **37.4 % / 28.5 %**,
//! with the city total unchanged at about 32 %.
//!
//! # Viability, and why no building ever stands in a road
//!
//! A parcel is **viable** when a point inside it clears the block boundary —
//! which *is* the road centre line — by more than the road half-width. Files are
//! seated on viable parcels only, frontage first, so houses line the streets and
//! the middle of a block stays open. Unoccupied parcels stay vacant and read as
//! yards, which is PRD §7.5's vacant lots for free.
//!
//! When a block has more files than viable parcels the roomiest are **split
//! again** rather than stacked: two narrower houses on one frontage is what a
//! crowded quarter actually does, and every resulting parcel is re-tested. The
//! prototype stacked instead, and 1.6 % of its buildings ended up standing in
//! the road corridor. Here that number is structurally zero — a parcel with no
//! road-clear interior point is never seated on.
//!
//! # Deletion and decay (PRD §7.5)
//!
//! > Deleted files leave **vacant lots** that go to seed over time.
//!
//! [`Lot::occupant`] alone cannot express that — `None` is equally "nobody ever
//! built here". [`VacancyLedger`] carries the difference: which lot, whose
//! building it was, and when it went. [`Vacancy::seed_progress`] turns that into
//! the `0..1` the renderer needs, from a `now` the **caller** supplies; nothing
//! in `polis-layout` may read a clock (PRD §7.4).

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

use std::collections::{BTreeMap, BTreeSet};

use polis_events::{LogicalPath, WallTime};
use serde::{Deserialize, Serialize};

use crate::accrete::Settlement;
use crate::blocks::BlockPlan;
use crate::determinism::{narrow, seed_for_path, SeededRng};
use crate::geom::{
    area, centroid, dist, dist_to_boundary, extent_along, interior_point_avoiding, longest_axis,
    perp, split_ring, Pt,
};
use crate::{Lot, LotId};

/// Largest a parcel on the oldest ground may be, in units of the city-wide grain.
///
/// Parcels are sized from the **block's own** file count rather than from one
/// city-wide grain, so a block with four files gets four plots and is built on
/// rather than surveyed into forty parcels of which thirty-six stay vacant. That
/// single change is most of the difference between a plan whose ground is 80 %
/// empty and one that reads as a town. This cap is what stops the same rule
/// producing a single absurd parcel where a block is large and nearly unoccupied.
pub const MAX_PARCEL_GRAINS_CORE: f64 = 2.20;

/// Largest a parcel on the newest ground may be, in units of the city-wide grain.
///
/// Three times [`MAX_PARCEL_GRAINS_CORE`], and that ratio is the whole point:
/// the judge's fifth required change is to widen the size hierarchy, "vary `sep`
/// by district age *and* by district file count, not by growth-sequence fraction
/// alone". A single cap for the whole city is what produced parcels of the same
/// median area in the tightest quarter and on the last ground broken, which is
/// the uniform grain the eye reads as manufactured.
pub const MAX_PARCEL_GRAINS_RIM: f64 = 7.00;

/// How many parcels the oldest ground surveys per file.
///
/// **One.** The old town is built out: every surveyed plot in the founding
/// quarters carries a house, and the open ground there is the block's own
/// courtyard behind the street wall rather than a plot nobody took. That is
/// what makes the core read as dense.
pub const LOT_SLACK_CORE: f64 = 1.00;

/// How many parcels the newest ground surveys per file.
///
/// **Higher** than [`LOT_SLACK_CORE`], and the direction is the whole density
/// gradient: the last ground broken is surveyed at about two plots per file, so
/// half of it stays open while the core is solid. Measured before this ramp existed, coverage ran
/// 28.9 % in the innermost third of the city against 32.7 % in the outermost —
/// a gradient pointing the *wrong way*, which is why the render read as one
/// uniform texture from centre to rim.
///
/// The old objection to rim slack — that many small vacant parcels read as a
/// subdivision plat rather than as a town edge — is answered by
/// [`VACANT_WEIGHT`]: a vacant plot is surveyed a quarter larger than its
/// neighbours, so the rim's emptiness arrives as a few big paddocks and not as
/// a scatter of unsold specks.
pub const LOT_SLACK_RIM: f64 = 2.05;

/// What the mean block radius is multiplied by to stand in for the city's edge.
///
/// A disc of radius `R` filled evenly has a mean radius of `2R/3`, so one and a
/// half puts the mean back at the edge. Any constant would do — it only sets
/// where [`CORE_RADIUS`] and [`RIM_RADIUS`] sit — and this one keeps them
/// readable as fractions of the city.
pub const REACH_SCALE: f64 = 1.5;

/// Fraction of the city's reach at which the radial term starts to bite.
pub const CORE_RADIUS: f64 = 0.24;

/// Fraction of the city's reach at which the radial term is fully suburban.
pub const RIM_RADIUS: f64 = 0.86;

/// How far a file's plot may sit from its block's mean plot, either way.
///
/// > **Footprint area** ∝ `sqrt(file_size_bytes)` (PRD §7.3)
///
/// The PRD's driver is read here rather than in [`crate::buildings`], because a
/// footprint can only be as big as the ground under it: with every plot in a
/// block the same size, a file three orders of magnitude larger than its
/// neighbour still gets the same building, and that is exactly what the visual
/// review measured — "one size class ... why every crop reads as gravel". The
/// weight sequence handed to `subdivide_weighted` is `sqrt(bytes)` normalised
/// against the block's own mean, so plot area, and with it footprint area,
/// follows the square root of the file size directly.
///
/// The band is what stops one 250 kB file eating a whole block and one 300-byte
/// stub getting a plot no building can stand on. `3.2` either way is a **10×
/// spread within a single block**, which is the review's own target and about
/// what a real street of burgage plots and merchant houses spans.
pub const WEIGHT_SPREAD: f64 = 4.6;

/// A vacant plot's weight, against the block's mean.
///
/// Above one on purpose — see [`LOT_SLACK_RIM`].
pub const VACANT_WEIGHT: f64 = 1.45;

/// Largest share of its block one plot may take, once a block has three.
///
/// [`WEIGHT_SPREAD`] alone lets one large file in a three-file block claim two
/// thirds of the ground, and a footprint filling two thirds of a block is a
/// block with a hole in it rather than a building on a street. Two plots may
/// still halve a block between them — there is nowhere else for the ground to
/// go — so the cap only applies from three.
pub const MAX_PLOT_SHARE: f64 = 0.45;

/// How far the weight-balanced cut may wander from the ratio it was asked for.
///
/// Small: this jitter is decoration, and the ratio is information. At ±7 % the
/// street line stays irregular without the plot widths stopping being readable
/// as file sizes.
pub const WEIGHT_JITTER: f64 = 0.05;

/// Closest to an end, as a fraction of the block's own area, a weighted cut may
/// fall.
pub const MIN_CUT_FRACTION: f64 = 0.06;

/// Bisection steps used to place a weighted cut at a wanted area fraction.
///
/// Fixed, never convergence-tested, for the reason
/// [`crate::buildings::FIT_STEPS`] gives: a loop that stops on a tolerance
/// stops after a different number of steps on a different target (PRD §7.4).
///
/// Twenty-four halvings take the bracket to about `6·10⁻⁸` of the block's own
/// width, which is five orders of magnitude below
/// [`crate::determinism::QUANTUM`] — and this loop is the whole added cost of
/// weighting the survey, so the steps that buy nothing are worth not spending:
/// at forty it put the single-file incremental step 8 ms over PRD §13.1's
/// budget on its own.
pub const CUT_STEPS: u32 = 24;

/// How much of the density gradient is read off the map rather than off
/// `git log`.
///
/// PRD §7.1 makes the growth order the age signal and this stage reads it
/// first (`block_age`). But the accretion only *partly* turns growth order
/// into distance from the civic square: measured on the 5 000-file fixture,
/// median block area rose just 1.37× from the innermost octile to the
/// outermost, so an age-only ramp produced a density gradient the eye could not
/// find. The radial term is what makes PRD §7.1's structure visible in the
/// places where the accretion did not manage to make age and radius the same
/// thing; where it did, the two terms agree and the blend changes nothing.
pub const RADIAL_WEIGHT: f64 = 0.62;

/// A parcel is viable when its interior clears the road by this multiple of the
/// road half-width.
pub const VIABLE_CLEARANCE: f64 = 1.15;

/// Subdivision stops when a strip is narrower than this multiple of the road
/// half-width. Below it, no building can stand without touching the road.
pub const MIN_STRIP: f64 = 3.4;

/// Smallest plot the weighted survey may ask for, in squares of [`MIN_STRIP`].
///
/// A plot narrower than a buildable strip in either direction is a plot the
/// subdivision will refuse to cut, so asking for one costs the block a parcel —
/// see `floor_plot_shares`. Just over one square, because a plot exactly one
/// strip on a side has no room for the garden its block owes the street.
pub const MIN_PLOT_AREA_STRIPS: f64 = 1.60;

/// Deepest recursion in `subdivide`.
pub const MAX_SUBDIVISION_DEPTH: u32 = 11;

/// How far the split fraction may wander from the middle.
pub const SPLIT_JITTER: f64 = 0.26;

/// Closest to an end, as a fraction of the span, a split may fall.
pub const MAX_EDGE: f64 = 0.42;

/// How many times a crowded block may split a parcel again to seat a surplus
/// file before it gives up and shares.
pub const MAX_DENSIFY_ROUNDS: usize = 8;

/// Days from deletion to fully gone to seed (PRD §7.5).
pub const SEED_WINDOW_DAYS: u32 = 180;

/// District size, in files, at which the parcel cap is left alone.
pub const DISTRICT_REF_FILES: f64 = 16.0;

/// Narrowest the district-size term may make a parcel cap.
pub const DISTRICT_FINEST: f64 = 0.62;

/// Widest the district-size term may make a parcel cap.
pub const DISTRICT_COARSEST: f64 = 1.55;

/// Age of a block, `0` for the founding files and `1` for the newest (PRD §7.1).
///
/// > **`git log` is the growth order.** Replay it in commit order. Files added
/// > in the repo's first year form the old town […] Files added last month sit
/// > on the periphery. (PRD §7.1)
///
/// The **median growth index of the block's own files**, which is that sentence
/// read literally. `files` arrives sorted by `(growth_index, path)`, so the
/// median is a lookup.
///
/// The obvious alternative — `BlockPlan::birth`, the growth step of the oldest
/// plot in the block — is measured and rejected: it is a *minimum* over a face
/// that usually touches several plots, so almost every block in the city
/// inherits the age of the earliest ground anywhere near it. At 5 000 files its
/// median came out at 0.011, 0.032 and 0.233 across the three block-size
/// terciles, which is no gradient at all. It survives only as the fallback for a
/// block whose files are all untracked and therefore have no growth index.
fn block_age(files: &[u32], s: &Settlement, last_growth: u32, birth: u32, last_birth: u32) -> f64 {
    let tracked: Vec<u32> = files
        .iter()
        .map(|fi| s.files[*fi as usize].growth_index)
        .filter(|g| *g != u32::MAX)
        .collect();
    if tracked.is_empty() {
        return ground_age(birth, last_birth);
    }
    let median = tracked[tracked.len() / 2];
    (f64::from(median) / f64::from(last_growth.max(1))).clamp(0.0, 1.0)
}

/// Age of the ground under a block, from the growth step of its oldest plot.
///
/// The fallback for a block with no tracked file in it; see `block_age`.
fn ground_age(birth: u32, last: u32) -> f64 {
    if birth == u32::MAX {
        return 1.0;
    }
    (f64::from(birth) / f64::from(last.max(1))).clamp(0.0, 1.0)
}

/// Linear ramp from a core value to a rim value over [`ground_age`].
fn by_age(core: f64, rim: f64, t: f64) -> f64 {
    core + (rim - core) * t.clamp(0.0, 1.0)
}

/// How suburban a block is: `0` in the founding core, `1` on the last ground
/// broken (PRD §7.1).
///
/// A blend of the growth order and the distance from the civic square; see
/// [`RADIAL_WEIGHT`] for why both terms are needed and what was measured
/// without the second one.
fn urbanity(age: f64, radius: f64) -> f64 {
    // Smoothstep over [`CORE_RADIUS`]..[`RIM_RADIUS`], not the raw fraction: a
    // linear term spends most of its range on the middle of the city, where a
    // block is neither core nor rim, and leaves the two ends — the only places
    // the eye compares — barely apart. No transcendental (PRD §7.4).
    let u = ((radius.clamp(0.0, 1.0) - CORE_RADIUS) / (RIM_RADIUS - CORE_RADIUS)).clamp(0.0, 1.0);
    let out = u * u * (3.0 - 2.0 * u);
    ((1.0 - RADIAL_WEIGHT) * age.clamp(0.0, 1.0) + RADIAL_WEIGHT * out).clamp(0.0, 1.0)
}

/// Ceiling on how many parcels one block may be cut into.
///
/// The item list is what drives `subdivide_weighted`'s depth, and a block that
/// is enormous relative to the grain would otherwise ask for thousands of
/// parcels and spend the whole layout budget surveying ground nobody will build
/// on. Well above any real block: the 5 000-file fixture's largest asks for 34.
pub const MAX_PARCELS_PER_BLOCK: usize = 256;

/// Hold every item to [`MAX_PLOT_SHARE`] of the block, once there are three.
///
/// Applied to the finished item list — files *and* vacant plots — because a
/// paddock two thirds the size of its block is as much a hole as a building
/// that size. One pass: capping an item lowers the total, so the shares that
/// come out are at or just under the bound, never over it.
fn cap_plot_shares(items: &mut [f64]) {
    if items.len() < 3 {
        return;
    }
    let total: f64 = items.iter().sum();
    if !(total.is_finite() && total > 0.0) {
        return;
    }
    let cap = total * MAX_PLOT_SHARE;
    for w in items.iter_mut() {
        if *w > cap {
            *w = cap;
        }
    }
}

/// Hold every item to a plot a building can actually stand on.
///
/// [`WEIGHT_SPREAD`] bounds a plot against its **block's** mean, which says
/// nothing about absolute size: in a small block the smallest weight can ask for
/// a plot narrower than the buildable strip, the subdivision then refuses the
/// cut that would create it, and the block comes out with fewer parcels than
/// files — which is how a plot proportional to `sqrt(bytes)` turns into a file
/// with no lot of its own. Measured on the 1 200-file fixture, this floor took
/// overflow from six files back to one.
///
/// Two passes: raising a weight raises the total, so the first pass lands the
/// raised items just under the floor and the second closes most of the gap. It
/// is deliberately not iterated to convergence — a fixed step count is what
/// keeps this reproducible (PRD §7.4) — and where the block simply has no room,
/// every weight ends at the floor, which is the equal split this stage used to
/// do everywhere.
fn floor_plot_shares(items: &mut [f64], block_area: f64, min_plot: f64) {
    if items.is_empty() || !(block_area.is_finite() && block_area > 0.0) {
        return;
    }
    for _ in 0..2 {
        let total: f64 = items.iter().sum();
        if !(total.is_finite() && total > 0.0) {
            return;
        }
        let floor = total * (min_plot / block_area);
        if floor <= 0.0 {
            return;
        }
        let mut moved = false;
        for w in items.iter_mut() {
            if *w < floor {
                *w = floor;
                moved = true;
            }
        }
        if !moved {
            return;
        }
    }
}

/// A block's file weights: `sqrt(bytes)` against the block's own mean, clamped.
///
/// See [`WEIGHT_SPREAD`]. The mean is summed in the caller's fixed order and
/// `sqrt` is exactly rounded on every target, so this is reproducible to the
/// bit (PRD §7.4).
fn block_weights(files: &[u32], s: &Settlement) -> Vec<f64> {
    let raw: Vec<f64> = files
        .iter()
        .map(|fi| (s.files[*fi as usize].size_bytes.max(1) as f64).sqrt())
        .collect();
    if raw.is_empty() {
        return raw;
    }
    let sum: f64 = raw.iter().sum();
    let mean = sum / raw.len() as f64;
    if !(mean.is_finite() && mean > 0.0) {
        return vec![1.0; raw.len()];
    }
    raw.iter()
        .map(|w| (w / mean).clamp(1.0 / WEIGHT_SPREAD, WEIGHT_SPREAD))
        .collect()
}

/// How much the size of a district widens or narrows its parcels.
///
/// A package of two hundred files is a quarter in its own right and is surveyed
/// finely; a package of three is an outbuilding on a large plot. The judge asked
/// for the grain to vary "by district age *and* by district file count"; this is
/// the second half, and it is a fourth root so that a district forty times the
/// size of another gets parcels two and a half times finer rather than forty.
fn district_grain(files: usize) -> f64 {
    let ratio = DISTRICT_REF_FILES / (files.max(1) as f64);
    // `sqrt(sqrt(x))` rather than `powf(0.25)`: `sqrt` is exactly rounded on
    // every target, `powf` is not (PRD §7.4).
    ratio
        .sqrt()
        .sqrt()
        .clamp(DISTRICT_FINEST, DISTRICT_COARSEST)
}

// ---------------------------------------------------------------------------
// Subdivision
// ---------------------------------------------------------------------------

/// Recursive subdivision along the longest axis (PRD §7.2 step 4).
///
/// `want` is how many parcels this ring should end up as; `target` the area to
/// stop at; `min_w` the narrowest strip a building can stand on. The split
/// fraction is jittered but **clamped so both halves stay wider than one
/// buildable strip** — guarding only the parent's extent is what leaves the thin
/// end-pieces no building can use.
///
/// # A cut that would make an unbuildable parcel is not made
///
/// `keep` decides whether a piece can carry a building at all. When either child
/// fails it, the cut is **abandoned and the parent is emitted whole** — which is
/// the merge the design bake-off's judge asked for ("merge unbuildable lots into
/// a neighbour polygon before seating rather than falling back"), done at the
/// only moment when the neighbour is still known for free: a piece's neighbour
/// is its sibling, and un-cutting is the union of the two.
///
/// Doing it here rather than after the fact is what makes it cheap and exact. A
/// merge performed later would have to union two arbitrary rings and could
/// produce a non-convex parcel; refusing the cut cannot, because the parent was
/// already a parcel.
// Nine arguments, and every one of them is a different axis of the same
// recursion: what to cut, how small to stop, how many pieces are wanted, which
// way the last cut ran, the seed, the depth, the narrowest buildable strip, what
// counts as buildable, and where to put the answer. A struct here would be a
// struct with nine fields and one use.
#[allow(clippy::too_many_arguments)]
pub(crate) fn subdivide(
    ring: &[Pt],
    target: f64,
    want: u32,
    axis: Option<Pt>,
    seed: u64,
    depth: u32,
    min_w: f64,
    keep: &dyn Fn(&[Pt]) -> bool,
    out: &mut Vec<Vec<Pt>>,
) {
    let a = area(ring);
    if depth >= MAX_SUBDIVISION_DEPTH
        || ring.len() < 3
        || (a <= target && want <= 1)
        || a < target * 0.24
    {
        if ring.len() >= 3 {
            out.push(ring.to_vec());
        }
        return;
    }
    let principal = longest_axis(ring);
    let ax = match axis {
        Some(prev) => {
            let (lo, hi) = extent_along(ring, prev);
            let (plo, phi) = extent_along(ring, perp(prev));
            // Keep slicing the same way while the strip is still fat enough.
            if (hi - lo) > (phi - plo) * 0.78 {
                prev
            } else {
                principal
            }
        }
        None => principal,
    };
    let (lo, hi) = extent_along(ring, ax);
    let span = hi - lo;
    // `min_w / MAX_EDGE`, not `2 * min_w`: the jitter clamp below cannot push a
    // boundary closer to the edge than `MAX_EDGE` of the span, so a span only
    // just over twice `min_w` would still leave a half below it.
    if span < min_w / MAX_EDGE {
        out.push(ring.to_vec());
        return;
    }
    let mut rng = SeededRng::for_seed(seed, "lot.split");
    let edge = (min_w / span).clamp(0.0, MAX_EDGE);
    let t = (0.5 + (rng.next_f64() - 0.5) * SPLIT_JITTER).clamp(edge, 1.0 - edge);
    let c = lo + span * t;
    let (neg, pos) = split_ring(ring, ax, c);
    let mut pieces: Vec<Vec<Pt>> = Vec::new();
    pieces.extend(neg);
    pieces.extend(pos);
    pieces.retain(|p| p.len() >= 3 && area(p) > 1e-9);
    if pieces.len() < 2 {
        out.push(ring.to_vec());
        return;
    }
    // The merge: a cut that would leave a piece no building can stand on is not
    // made at all, so the ground stays with the sibling that can use it.
    if !pieces.iter().all(|p| keep(p)) {
        out.push(ring.to_vec());
        return;
    }
    let total: f64 = pieces.iter().map(|p| area(p)).sum();
    // A deterministic order for the children's seeds: a property of where the
    // pieces are, never of the order `split_ring` happened to emit them.
    let mut keyed: Vec<((i64, i64), usize)> = pieces
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let c = centroid(p);
            (((c[1] * 1e6) as i64, (c[0] * 1e6) as i64), i)
        })
        .collect();
    keyed.sort_unstable();
    for (k, (_, i)) in keyed.iter().enumerate() {
        let p = &pieces[*i];
        let frac = if total > 0.0 { area(p) / total } else { 0.5 };
        let w = ((f64::from(want) * frac).round() as u32).max(1);
        subdivide(
            p,
            target,
            w,
            Some(ax),
            seed.wrapping_mul(0x9E37_79B9_7F4A_7C15)
                .wrapping_add(k as u64 + 1),
            depth + 1,
            min_w,
            keep,
            out,
        );
    }
}

/// The `keep` predicate that lets every cut through: subdivision by area alone.
pub(crate) fn any_parcel(_ring: &[Pt]) -> bool {
    true
}

// ---------------------------------------------------------------------------
// Weighted subdivision — the plot is the size of the file that stands on it
// ---------------------------------------------------------------------------

/// The two sides of one cut, each of which may be more than one piece when the
/// ring is non-convex.
type Halves = (Vec<Vec<Pt>>, Vec<Vec<Pt>>);

/// Area of the part of `ring` on the negative side of `dot(ax, x) = c`.
///
/// Sutherland–Hodgman is exact for a convex ring and is most of what this
/// pipeline produces; a pruned block can be genuinely non-convex, and there the
/// general re-walk is used. `convex` is computed once per cut rather than once
/// per bisection step.
fn negative_area(ring: &[Pt], convex: bool, ax: Pt, c: f64) -> f64 {
    if convex {
        clipped_area(ring, ax, c)
    } else {
        split_ring(ring, ax, c).0.iter().map(|p| area(p)).sum()
    }
}

/// The area of `clip_halfplane(ring, n, c)` without building the ring.
///
/// The same Sutherland–Hodgman walk, with the shoelace accumulated as the
/// vertices are produced instead of collected into a `Vec`. This is the inner
/// loop of the whole stage — [`CUT_STEPS`] evaluations per cut, one cut per
/// parcel — and the allocation was most of its cost.
///
/// Deterministic by the same argument as everything else here: a fixed
/// expression evaluated in a fixed order over `+ - * /`, all correctly rounded
/// (PRD §7.4). It is not bit-identical to `area(&clip_halfplane(..))`, which
/// drops vertices within `1e-9` of each other before summing; the difference is
/// far below [`crate::determinism::QUANTUM`] and it only ever steers a
/// bisection.
fn clipped_area(ring: &[Pt], n: Pt, c: f64) -> f64 {
    let m = ring.len();
    if m < 3 {
        return 0.0;
    }
    let mut first: Option<Pt> = None;
    let mut prev: Option<Pt> = None;
    let mut twice = 0.0f64;
    for i in 0..m {
        let a = ring[i];
        let b = ring[(i + 1) % m];
        let da = crate::geom::dot(n, a) - c;
        let db = crate::geom::dot(n, b) - c;
        let a_in = da <= 0.0;
        let b_in = db <= 0.0;
        let mut emit = |p: Pt| {
            if let Some(q) = prev {
                twice += q[0] * p[1] - p[0] * q[1];
            } else {
                first = Some(p);
            }
            prev = Some(p);
        };
        if a_in {
            emit(a);
        }
        if a_in != b_in {
            let t = da / (da - db);
            if t.is_finite() {
                emit(crate::geom::lerp(a, b, t.clamp(0.0, 1.0)));
            }
        }
    }
    if let (Some(f), Some(p)) = (first, prev) {
        twice += p[0] * f[1] - f[0] * p[1];
    }
    twice.abs() * 0.5
}

/// Where to cut so that the negative side takes `want` of the ring's area.
///
/// The clipped area is monotone non-decreasing in `c` for **any** simple ring,
/// convex or not, so this bisection is exact rather than approximate. `c` is
/// confined to `[low, high]`, which is how "no parcel is narrower than a
/// buildable strip" becomes a property of the cut rather than a filter applied
/// after it: a fraction that cannot be reached inside the band lands on the
/// nearest end of it, and the parcel is merely off its wanted size.
fn cut_at_fraction(ring: &[Pt], convex: bool, ax: Pt, low: f64, high: f64, want: f64) -> f64 {
    let total = area(ring);
    if total <= 0.0 || high <= low {
        return f64::midpoint(low, high);
    }
    let target = total * want;
    let mut lo = low;
    let mut hi = high;
    for _ in 0..CUT_STEPS {
        let mid = f64::midpoint(lo, hi);
        if negative_area(ring, convex, ax, mid) < target {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    hi
}

/// How many of `n` items each piece takes, proportional to its area.
///
/// A cut across a non-convex block can leave two or more pieces on one side.
/// The items that side was dealt are split between them by area, through
/// cumulative rounding so the counts sum to `n` exactly and no item is lost. A
/// piece dealt nothing is still emitted — it is ground, and ground is
/// conserved; it simply becomes a vacant parcel (PRD §7.5).
fn deal(pieces: &[Vec<Pt>], n: usize) -> Vec<usize> {
    let areas: Vec<f64> = pieces.iter().map(|p| area(p)).collect();
    let total: f64 = areas.iter().sum();
    if total <= 0.0 || pieces.is_empty() {
        return vec![0; pieces.len()];
    }
    let mut out = Vec::with_capacity(pieces.len());
    let mut cumulative = 0.0;
    let mut placed = 0usize;
    for (i, a) in areas.iter().enumerate() {
        cumulative += *a;
        let end = if i + 1 == areas.len() {
            n
        } else {
            ((n as f64) * cumulative / total)
                .round()
                .clamp(0.0, n as f64) as usize
        };
        let end = end.max(placed).min(n);
        out.push(end - placed);
        placed = end;
    }
    out
}

/// Canonical order for the pieces of one cut: lowest quantised centroid first.
///
/// A property of where a piece is, never of the order `split_ring` happened to
/// emit it (PRD §7.4).
fn in_place_order(pieces: &mut [Vec<Pt>]) {
    pieces.sort_by_key(|p| {
        let c = centroid(p);
        ((c[1] * 1e6) as i64, (c[0] * 1e6) as i64)
    });
}

/// Recursive subdivision that gives each item the share of the ground its
/// weight asks for (PRD §7.2 step 4, PRD §7.3).
///
/// The unweighted [`subdivide`] cuts a block into pieces of roughly equal area
/// and then hands them out, which is why every building in a block came out the
/// same size: a block of eleven files became eleven equal plots whatever those
/// files were. Here the block's **weight sequence** is carried down with the
/// ring — the sequence is split at the index that best balances it, and the
/// ring is cut at the *area fraction* that split asks for — so the recursion
/// bottoms out at exactly one item per parcel with the parcel's area
/// proportional to its item's weight.
///
/// Everything that made [`subdivide`] produce a town rather than graph paper is
/// kept:
///
/// * the axis is **reused for the children** while the strip is still the long
///   one, which is what turns a block into a row of deep narrow plots facing
///   the street rather than a quad-tree of squares;
/// * a cut that would leave a piece no building can stand on is **not made**,
///   and the parent is emitted whole — the merge done at the one moment the
///   neighbour is free to know ([`subdivide`]'s own documentation);
/// * every child's seed is derived from its place, not from emission order.
///
/// `weights` must be non-empty for the ring to be split at all; a ring with one
/// item is a finished parcel.
// Eight arguments, each a different axis of the same recursion — see
// `subdivide`, which carries the same set for the same reason.
#[allow(clippy::too_many_arguments)]
pub(crate) fn subdivide_weighted(
    ring: &[Pt],
    weights: &[f64],
    axis: Option<Pt>,
    seed: u64,
    depth: u32,
    min_w: f64,
    keep: &dyn Fn(&[Pt]) -> bool,
    out: &mut Vec<Vec<Pt>>,
) {
    if ring.len() < 3 {
        return;
    }
    if weights.len() <= 1 || depth >= MAX_SUBDIVISION_DEPTH {
        out.push(ring.to_vec());
        return;
    }
    let principal = longest_axis(ring);
    let ax = match axis {
        Some(prev) => {
            let (lo, hi) = extent_along(ring, prev);
            let (plo, phi) = extent_along(ring, perp(prev));
            if (hi - lo) > (phi - plo) * 0.78 {
                prev
            } else {
                principal
            }
        }
        None => principal,
    };
    let (lo, hi) = extent_along(ring, ax);
    if hi - lo < min_w * 2.0 {
        out.push(ring.to_vec());
        return;
    }

    // Split the sequence where the two halves weigh most nearly the same, and
    // cut the ground in the ratio that split asks for.
    let total: f64 = weights.iter().sum();
    if !(total.is_finite() && total > 0.0) {
        out.push(ring.to_vec());
        return;
    }
    let half = total * 0.5;
    let mut running = 0.0;
    let mut best = (f64::INFINITY, 1usize, 0.0f64);
    for k in 1..weights.len() {
        running += weights[k - 1];
        let gap = (running - half).abs();
        if gap < best.0 {
            best = (gap, k, running);
        }
    }
    let (_, k, left) = best;
    let mut rng = SeededRng::for_seed(seed, "lot.weighted");
    let want = ((left / total) * (1.0 + (rng.next_f64() - 0.5) * 2.0 * WEIGHT_JITTER))
        .clamp(MIN_CUT_FRACTION, 1.0 - MIN_CUT_FRACTION);
    let convex = crate::geom::is_convex(ring);
    let live = |v: Vec<Vec<Pt>>| -> Vec<Vec<Pt>> {
        v.into_iter()
            .filter(|p| p.len() >= 3 && area(p) > 1e-9)
            .collect()
    };
    // The wanted ratio first; an even split second. A cut that would leave a
    // piece no building can stand on is **not made**, and the ground then stays
    // whole with two files on it — so before giving up on the cut entirely, try
    // the split most likely to leave both children buildable. A file with a
    // plot of the wrong size beats two files sharing one plot, which is a file
    // drawn nowhere.
    let mut halves: Option<Halves> = None;
    let attempts: &[f64] = if (want - 0.5).abs() < 0.03 {
        &[0.5]
    } else {
        &[want, 0.5]
    };
    for &fraction in attempts {
        let c = cut_at_fraction(ring, convex, ax, lo + min_w, hi - min_w, fraction);
        let (neg, pos) = split_ring(ring, ax, c);
        let neg = live(neg);
        let pos = live(pos);
        if neg.is_empty() || pos.is_empty() {
            continue;
        }
        if !neg.iter().chain(pos.iter()).all(|p| keep(p)) {
            continue;
        }
        halves = Some((neg, pos));
        break;
    }
    let Some((mut neg, mut pos)) = halves else {
        // The merge: ground stays with the sibling that can use it.
        out.push(ring.to_vec());
        return;
    };
    in_place_order(&mut neg);
    in_place_order(&mut pos);

    let mut child = 0u64;
    for (pieces, items) in [(neg, &weights[..k]), (pos, &weights[k..])] {
        let counts = deal(&pieces, items.len());
        let mut taken = 0usize;
        for (piece, take) in pieces.iter().zip(counts.iter()) {
            let slice = &items[taken..taken + *take];
            taken += *take;
            child += 1;
            if slice.is_empty() {
                out.push(piece.clone());
                continue;
            }
            subdivide_weighted(
                piece,
                slice,
                Some(ax),
                seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(child),
                depth + 1,
                min_w,
                keep,
                out,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Parcelling
// ---------------------------------------------------------------------------

/// One surveyed parcel, in the pipeline's internal `f64` form.
#[derive(Debug, Clone)]
pub(crate) struct Parcel {
    /// The ring, counter-clockwise.
    pub(crate) ring: Vec<Pt>,
    /// Index into the block list.
    pub(crate) block: u32,
    /// The file standing here, if any.
    pub(crate) occupant: Option<u32>,
}

/// What the parcelling did with every file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LotReport {
    /// Blocks that were subdivided.
    pub blocks: usize,
    /// Parcels emitted.
    pub lots: usize,
    /// Files that got a parcel to themselves.
    pub placed: usize,
    /// Files that had to share a block's roomiest parcel after densifying.
    pub overflow: usize,
    /// Files deliberately given no lot: PRD §8's industrial zones are drawn as
    /// one mass, not as individual buildings.
    pub massed: usize,
    /// Files with no parcel at all — only possible when the city has no blocks.
    pub unplaced: usize,
    /// Parcels nobody was assigned to: yards and gardens (PRD §7.5).
    pub unoccupied: usize,
}

impl LotReport {
    /// Every file the parcelling considered.
    #[must_use]
    pub fn files(&self) -> usize {
        self.placed + self.overflow + self.massed + self.unplaced
    }

    /// True when some block could not house its own files one to a parcel.
    #[must_use]
    pub fn is_overcrowded(&self) -> bool {
        self.overflow > 0 || self.unplaced > 0
    }
}

/// A file that could not get a parcel to itself.
///
/// Never a dropped building: an overflow entry is a file the caller still has to
/// draw. It is only ever produced when a block has fewer road-clear parcels than
/// files even after densifying.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Overflow {
    /// The file with nowhere of its own.
    pub path: LogicalPath,
    /// The lot it shares, or `None` when the block has none at all.
    pub shares: Option<LotId>,
}

/// The whole city's parcels.
#[derive(Debug, Clone, Default)]
pub(crate) struct Parcelling {
    /// Parcels, in emission order; the index is the [`LotId`].
    pub(crate) parcels: Vec<Parcel>,
    /// Files that had to share.
    pub(crate) overflow: Vec<Overflow>,
    /// Files deliberately unhoused (PRD §8), in path order.
    pub(crate) massed: Vec<LogicalPath>,
    /// What happened, in numbers.
    pub(crate) report: LotReport,
}

/// Survey every block and seat every file (PRD §7.2 step 4).
///
/// `road_half` is the corridor a building must keep out of; it is measured
/// against the block ring, which *is* the road centre line.
///
/// `cuts` carries the subdivisions of the previous growth step; see
/// [`crate::memo::CutCache`]. Pass a fresh one and nothing is reused.
pub(crate) fn parcel_city(
    blocks: &[BlockPlan],
    s: &Settlement,
    civic: Option<u32>,
    road_half: f64,
    cuts: &mut crate::memo::CutCache,
) -> Parcelling {
    cuts.trim(blocks.len());
    let mut out = Parcelling::default();
    out.report.blocks = blocks.len();
    if blocks.is_empty() {
        out.report.unplaced = s.files.iter().filter(|f| !f.industrial).count();
        out.report.massed = s.files.iter().filter(|f| f.industrial).count();
        out.massed = s
            .files
            .iter()
            .filter(|f| f.industrial)
            .map(|f| f.path.clone())
            .collect();
        out.massed.sort();
        return out;
    }

    // The parcel grain is a fixed fraction of the total ground per file, so a
    // 90-file village and a 5 000-file city have the same texture: a block is
    // always divided into a handful of parcels, and the ones nobody occupies
    // read as yards rather than as one enormous empty lot.
    let areas: Vec<f64> = blocks.iter().map(BlockPlan::area).collect();
    let mut sorted = areas.clone();
    sorted.sort_by(f64::total_cmp);
    let median = if sorted.is_empty() {
        1.0
    } else {
        sorted[sorted.len() / 2]
    };
    let total: f64 = areas.iter().sum();
    let housed = s.files.iter().filter(|f| !f.industrial).count().max(1);
    let grain = (total / housed as f64).clamp(median * 0.03, median * 0.60);
    let min_w = road_half * MIN_STRIP;
    let min_clear = road_half * VIABLE_CLEARANCE;
    // The radial half of the density ramp ([`RADIAL_WEIGHT`]), as a fraction of
    // *this* city's reach rather than as an absolute length: the civic square
    // sits at the origin, which is the same frame `crate::city::measure`
    // reports core and rim coverage in.
    //
    // # Why the mean and not the outermost block
    //
    // PRD §7.7 — never move the ground while the operator is looking at it —
    // and the M1 gate measures it: one added file may move at most a tenth of
    // the city's buildings. Normalising against `max(radius)` failed that
    // outright, at **252 of 551 buildings moved**, and the mechanism is worth
    // recording because it is a trap any city-wide statistic falls into. One new
    // plot can land outside every existing block; the maximum then jumps by a
    // couple of per cent; every block's radius *ratio* shifts with it; and the
    // parcel counts derived from that ratio flip together, so a single added
    // file re-surveys the whole city.
    //
    // The mean over blocks moves by O(1/n) instead of by a jump, which is the
    // property this needs. [`REACH_SCALE`] puts it back on the scale of the
    // city's edge.
    let reach = if blocks.is_empty() {
        1.0
    } else {
        let sum: f64 = blocks
            .iter()
            .map(|b| dist(centroid(&b.ring), [0.0, 0.0]))
            .sum();
        (sum / blocks.len() as f64 * REACH_SCALE).max(1e-9)
    };

    // The growth order, as a denominator: the last file added is age 1.
    let last_growth = s
        .files
        .iter()
        .map(|f| f.growth_index)
        .filter(|g| *g != u32::MAX)
        .max()
        .unwrap_or(0)
        .max(1);
    let last_birth = s.plots.iter().map(|p| p.birth).max().unwrap_or(0).max(1);
    // Files per district, for the second half of the grain rule. Keyed on the
    // territory node, and built from the plots so a district that spilled onto a
    // neighbour's ground is still counted whole.
    let mut district_files: BTreeMap<u32, usize> = BTreeMap::new();
    for plot in &s.plots {
        let n = plot
            .files
            .iter()
            .filter(|fi| !s.files[**fi as usize].industrial)
            .count();
        *district_files.entry(plot.district).or_insert(0) += n;
    }

    for (bi, block) in blocks.iter().enumerate() {
        let block_id = u32::try_from(bi).expect("block count fits in u32");
        let mut files: Vec<u32> = Vec::new();
        for &pi in &block.plots {
            for &fi in &s.plots[pi as usize].files {
                if s.files[fi as usize].industrial {
                    continue;
                }
                files.push(fi);
            }
        }
        // PRD §8's civic square is open ground, not parcels — **when it is
        // actually empty**. `civic_square` picks the block holding the reserved
        // plot, and an ordinary plot can land in that same block; skipping it
        // unconditionally then deletes that plot's files from the map. They were
        // counted as `unhoused` and drawn nowhere, which is the one outcome this
        // stage's accounting exists to make impossible. A square with two houses
        // on one side is a smaller loss than two missing buildings.
        if Some(block_id) == civic && files.is_empty() {
            continue;
        }
        // Growth order within the block, so the oldest file takes the first lot.
        files.sort_by(|a, b| {
            let fa = &s.files[*a as usize];
            let fb = &s.files[*b as usize];
            (fa.growth_index, fa.path.as_str()).cmp(&(fb.growth_index, fb.path.as_str()))
        });

        let a = block.area();
        if a <= 0.0 {
            out.report.unplaced += files.len();
            continue;
        }
        // Enough parcels for this block's own files, plus the slack PRD §7.5
        // wants as vacant ground, and never a parcel above the cap.
        // `round`, not `ceil`: a block with one file must get **one** parcel,
        // not two. Rounding up turns every single-file block in the city into a
        // house next to an empty plot, which halves the occupancy and with it
        // the built share of the ground.
        //
        // Both terms are graded by the age of this block's own ground and by the
        // size of the district that owns it — the judge's fifth required change —
        // and then blended with how far out the block sits ([`RADIAL_WEIGHT`]).
        let t = urbanity(
            block_age(&files, s, last_growth, block.birth, last_birth),
            dist(centroid(&block.ring), [0.0, 0.0]) / reach,
        );
        let dfiles = block
            .district
            .and_then(|d| district_files.get(&d).copied())
            .unwrap_or(files.len());
        let max_parcel = grain
            * by_age(MAX_PARCEL_GRAINS_CORE, MAX_PARCEL_GRAINS_RIM, t)
            * district_grain(dfiles);
        // One item per parcel, in the order the parcels will be cut: this
        // block's own files by `sqrt(bytes)` (PRD §7.3), then the vacant ground
        // PRD §7.5 wants, then whatever more is needed to keep every parcel
        // under the cap.
        let weights = block_weights(&files, s);
        let mut items = weights.clone();
        // `floor`, not `round`: rounding hands a vacant plot to a core block
        // that asked for four fifths of one, and the whole point of
        // [`LOT_SLACK_CORE`] is that the old town has none. Flooring makes the
        // ramp's bottom end mean exactly what it says.
        let spare = ((files.len() as f64) * (by_age(LOT_SLACK_CORE, LOT_SLACK_RIM, t) - 1.0))
            .floor()
            .max(0.0) as usize;
        let by_cap = (a / max_parcel).ceil().max(1.0) as usize;
        let wanted = (items.len() + spare)
            .max(by_cap)
            .min(MAX_PARCELS_PER_BLOCK)
            .max(items.len());
        items.resize(wanted, VACANT_WEIGHT);
        if items.is_empty() {
            items.push(1.0);
        }
        // A plot must be big enough to stand a house on before it is allowed
        // to be proportional to anything.
        floor_plot_shares(&mut items, a, min_w * min_w * MIN_PLOT_AREA_STRIPS);
        cap_plot_shares(&mut items);
        let seed = files
            .first()
            .map_or(bi as u64, |f| s.files[*f as usize].path.layout_seed())
            ^ (bi as u64).wrapping_mul(31);
        // A parcel is buildable when a point inside it clears the road corridor.
        // Passing it into the subdivision is what turns "reject the sliver" into
        // "never cut the sliver off in the first place".
        let buildable = |r: &[Pt]| {
            interior_point_avoiding(r, Some(&block.ring), road_half)
                .is_some_and(|(_, clear)| clear >= min_clear)
        };
        // The cut is a pure function of exactly these arguments, so a block the
        // growth step did not touch keeps the subdivision it already had —
        // recomputed only when something it reads has actually changed. See
        // [`crate::memo::CutCache`].
        let mut rings = cuts.cut(
            &block.ring,
            &items,
            seed,
            [min_w, road_half, min_clear],
            || {
                let mut rings: Vec<Vec<Pt>> = Vec::new();
                subdivide_weighted(
                    &block.ring,
                    &items,
                    None,
                    seed,
                    0,
                    min_w,
                    &buildable,
                    &mut rings,
                );
                rings
            },
        );
        rings.retain(|r| area(r) > 1e-9);
        if rings.is_empty() {
            rings.push(block.ring.clone());
        }

        seat_block(
            &mut out, block, block_id, rings, &files, &weights, s, road_half, min_clear, min_w,
            seed,
        );
    }

    rehouse_overflow_next_door(&mut out, blocks, s);

    // Every file is accounted for: a plot that fell in no block at all (the
    // face walk dropped its cell) leaves its files unhoused, and that is
    // counted rather than silently lost.
    let mut seated = vec![false; s.files.len()];
    for p in &out.parcels {
        if let Some(f) = p.occupant {
            seated[f as usize] = true;
        }
    }
    for o in &out.overflow {
        if let Some(i) = s.files.iter().position(|f| f.path == o.path) {
            seated[i] = true;
        }
    }
    for f in &s.files {
        if f.industrial {
            out.massed.push(f.path.clone());
        }
    }
    out.report.unplaced += seated
        .iter()
        .zip(s.files.iter())
        .filter(|(ok, f)| !**ok && !f.industrial)
        .count();
    out.massed.sort();
    out.report.massed = out.massed.len();
    out.report.lots = out.parcels.len();
    out.report.unoccupied = out.parcels.iter().filter(|p| p.occupant.is_none()).count();
    out
}

/// Give every file the block had no room for a vacant parcel next door.
///
/// # The failure this exists to remove
///
/// Two files on one parcel means one of them has no building, and a file with
/// no building is a file that has silently left the map — the worst outcome in
/// this pipeline. It was measured at 11 files of 7 014 on Django and 21 of 6 138
/// on `CPython`, and instrumenting it gave one cause, exactly:
///
/// ```text
/// DIAG block=1540 plots=1 caps=[(5, 43)] files=43 viable=12 area=1.0274 verts=3
/// ```
///
/// One plot, whose capacity is five, carrying **43** files, in a triangular face
/// of area 1.03 against a median block of 4.81. `django/utils` is 43 files and
/// [`crate::regions`] gave it a single plot: its partition divides plot
/// *capacity* down the tree, but every district also has a floor of one plot,
/// and Django is 2 076 directories over 2 472 plots — so the floor, not the
/// balance, is what most leaf directories get. The files then all land on that
/// one plot, and the face it sits in has room for twelve buildings at the local
/// grain. Twelve fit. The subdivision is not at fault and neither is the
/// densify loop: both were instrumented first and both had run to exhaustion,
/// having produced every parcel the ground can hold.
///
/// # Why next door, and why this is not a papered-over crack
///
/// The ground the file needs exists a few metres away: Django's map has 3 260
/// vacant parcels. So the surplus walks outward over the road graph and takes
/// the nearest vacant parcel, preferring one in its own district, then one in
/// its own top-level package, then any. That is the relaxation ladder PRD §7.2
/// already applies to plots, applied one level down to parcels, and it is
/// strictly better than the alternative it replaces — a real parcel of its own
/// on a neighbour's ground beats no building at all.
///
/// It does not hide the shortfall: `shared_faces` still counts the districts
/// whose ground could not hold them, and a file rehoused here is a file whose
/// building stands outside its own district, which the report prints.
///
/// Deterministic: the overflow list is built in block order then file order, the
/// walk is a breadth-first search over blocks in index order, and ties inside a
/// ring go to the lowest parcel index (PRD §7.4).
fn rehouse_overflow_next_door(out: &mut Parcelling, blocks: &[BlockPlan], s: &Settlement) {
    if out.overflow.is_empty() {
        return;
    }
    // Which blocks share a road edge.
    let mut sides: BTreeMap<usize, Vec<u32>> = BTreeMap::new();
    for (i, b) in blocks.iter().enumerate() {
        for &h in &b.half_edges {
            sides
                .entry(h / 2)
                .or_default()
                .push(u32::try_from(i).expect("block count fits in u32"));
        }
    }
    let mut adj: Vec<Vec<u32>> = vec![Vec::new(); blocks.len()];
    for list in sides.values() {
        if list.len() == 2 {
            adj[list[0] as usize].push(list[1]);
            adj[list[1] as usize].push(list[0]);
        }
    }
    for list in &mut adj {
        list.sort_unstable();
        list.dedup();
    }
    // Vacant parcels per block, lowest index first.
    let mut vacant: Vec<Vec<usize>> = vec![Vec::new(); blocks.len()];
    for (i, p) in out.parcels.iter().enumerate() {
        if p.occupant.is_none() && (p.block as usize) < blocks.len() {
            vacant[p.block as usize].push(i);
        }
    }
    let packages = crate::districts::package_of(&s.territory);
    let file_of: BTreeMap<&LogicalPath, u32> = s
        .files
        .iter()
        .enumerate()
        .map(|(i, f)| (&f.path, u32::try_from(i).expect("file count fits in u32")))
        .collect();

    let pending = std::mem::take(&mut out.overflow);
    let mut still: Vec<Overflow> = Vec::new();
    for entry in pending {
        let Some(&fi) = file_of.get(&entry.path) else {
            still.push(entry);
            continue;
        };
        let from = entry
            .shares
            .and_then(|l| out.parcels.get(l.0 as usize))
            .map(|p| p.block);
        let Some(from) = from.filter(|b| (*b as usize) < blocks.len()) else {
            still.push(entry);
            continue;
        };
        let want = s.district_of_file(fi);
        let package = |d: Option<u32>| d.and_then(|d| packages.get(&d).copied());
        let mine = package(Some(want));
        // Breadth-first over the road graph's faces, taking the best parcel in
        // the nearest ring that has one: same district, then same package, then
        // anything. Bounded, so a city with no vacant ground cannot walk it all.
        let mut seen: BTreeSet<u32> = BTreeSet::new();
        let mut ring: Vec<u32> = vec![from];
        seen.insert(from);
        let mut taken: Option<usize> = None;
        for _ in 0..REHOUSE_RINGS {
            let mut best: Option<(u8, usize)> = None;
            for &b in &ring {
                let Some(&pi) = vacant[b as usize].first() else {
                    continue;
                };
                let d = blocks[b as usize].district;
                let rank = if d == Some(want) {
                    0
                } else if package(d) == mine && mine.is_some() {
                    1
                } else {
                    2
                };
                if best.is_none_or(|(r, p)| (rank, pi) < (r, p)) {
                    best = Some((rank, pi));
                }
            }
            if let Some((_, pi)) = best {
                taken = Some(pi);
                break;
            }
            let mut next: Vec<u32> = Vec::new();
            for &b in &ring {
                for &n in &adj[b as usize] {
                    if seen.insert(n) {
                        next.push(n);
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            next.sort_unstable();
            ring = next;
        }
        if let Some(pi) = taken {
            let block = out.parcels[pi].block as usize;
            vacant[block].retain(|&x| x != pi);
            out.parcels[pi].occupant = Some(fi);
            out.report.placed += 1;
            out.report.overflow -= 1;
        } else {
            still.push(entry);
        }
    }
    out.overflow = still;
}

/// How far a homeless file may walk for a parcel, in rings of blocks.
///
/// Four. Far enough to cross a crowded quarter, near enough that the building
/// is still recognisably next to where it belongs; and it bounds the search on a
/// city with no vacant ground at all.
const REHOUSE_RINGS: usize = 4;

/// Seat one block's files on its parcels.
#[allow(clippy::too_many_arguments)]
fn seat_block(
    out: &mut Parcelling,
    block: &BlockPlan,
    block_id: u32,
    rings: Vec<Vec<Pt>>,
    files: &[u32],
    weights: &[f64],
    s: &Settlement,
    road_half: f64,
    min_clear: f64,
    min_w: f64,
    seed: u64,
) {
    // A parcel is viable when a point inside it clears the road corridor.
    let probe = |ring: &[Pt]| interior_point_avoiding(ring, Some(&block.ring), road_half);
    let buildable = |r: &[Pt]| probe(r).is_some_and(|(_, clear)| clear >= min_clear);
    let mut viable: Vec<(Vec<Pt>, Pt, f64)> = Vec::new();
    let mut spare: Vec<Vec<Pt>> = Vec::new();
    for r in rings {
        match probe(&r) {
            Some((p, clear)) if clear >= min_clear => viable.push((r, p, clear)),
            _ => spare.push(r),
        }
    }

    // Densify: a crowded block splits its roomiest parcels again rather than
    // stacking two files on one, which is where road-crossing buildings came
    // from in the prototype.
    //
    // **A parcel that will not split does not stop the loop**, it is only
    // struck off. Giving up on the first refusal is what left a block short of
    // parcels while an untouched neighbour would have halved cleanly, and the
    // files that fell off the end became `overflow` — a file drawn nowhere,
    // which is the worst outcome this stage has. Measured on the 1 200-file
    // fixture, breaking out gave 4 overflow files and striking off gives 0.
    //
    // **The budget is set by the demand, not by a constant.** It used to be
    // `MAX_DENSIFY_ROUNDS * 3` — twenty-four — which is a bound on *work* while
    // the work required is set by how many files the face has to seat. Measured
    // once the district partition moved onto the plot adjacency graph and faces
    // stopped being carved out of a territory polygon: one face of
    // `django/utils`, a triangle of area 1.03 against a median block of 4.81,
    // had to seat 43 files. Twenty-four rounds cannot produce 43 parcels from
    // four, so eleven files ran out of *budget* rather than out of room and were
    // drawn sharing a neighbour's parcel — the failure mode this stage exists to
    // prevent. CPython's `PCbuild` was the same shape at 21 files.
    //
    // The bound below is a real one rather than a hope. Each round either splits
    // a parcel, which strictly increases `viable` and so can happen at most
    // `files.len()` times before the loop condition ends it, or strikes one off
    // into `refused`, which is never retried and is bounded by the number of
    // parcels that have ever existed. Two rounds per file plus the old constant
    // covers both halves; `largest_untried` returning `None` is what actually
    // ends the loop in practice.
    let mut rounds = 0;
    let budget = files.len() * 2 + MAX_DENSIFY_ROUNDS * 3;
    let mut refused: BTreeSet<(i64, i64)> = BTreeSet::new();
    let key = |ring: &[Pt]| -> (i64, i64) {
        let c = centroid(ring);
        ((c[1] * 1e6) as i64, (c[0] * 1e6) as i64)
    };
    while viable.len() < files.len() && rounds < budget {
        rounds += 1;
        let Some(idx) = largest_untried(&viable, &refused, &key) else {
            break;
        };
        let (ring, _, _) = viable[idx].clone();
        let mut pieces: Vec<Vec<Pt>> = Vec::new();
        subdivide(
            &ring,
            area(&ring) * 0.45,
            2,
            None,
            seed.wrapping_add(rounds as u64 * 0x9E37_79B9),
            0,
            min_w,
            &buildable,
            &mut pieces,
        );
        pieces.retain(|p| area(p) > 1e-9);
        let mut fresh: Vec<(Vec<Pt>, Pt, f64)> = Vec::new();
        let mut rejected: Vec<Vec<Pt>> = Vec::new();
        for p in pieces {
            match probe(&p) {
                Some((q, clear)) if clear >= min_clear => fresh.push((p, q, clear)),
                _ => rejected.push(p),
            }
        }
        if fresh.len() < 2 {
            // Splitting this one made things worse: keep it whole, and try the
            // next roomiest instead of abandoning the block.
            refused.insert(key(&ring));
            continue;
        }
        viable.swap_remove(idx);
        viable.extend(fresh);
        spare.extend(rejected);
    }

    // Street frontage first: a parcel on the block edge is built on before one
    // buried in the middle, so houses line the roads and the interior of a block
    // stays open. That is what a town does, and it is what makes a road look
    // like a street rather than a boundary.
    let grain = area(&block.ring).max(1e-9).sqrt();
    viable.sort_by_key(|(_, q, _)| {
        let front = (dist_to_boundary(&block.ring, *q) / grain * 64.0).round() as i64;
        let c = centroid(&[*q]);
        (front, (c[1] * 1e5) as i64, (c[0] * 1e5) as i64)
    });
    spare.sort_by_key(|r| {
        let c = centroid(r);
        ((c[1] * 1e5) as i64, (c[0] * 1e5) as i64)
    });

    let base = out.parcels.len();
    for (ring, _, _) in &viable {
        out.parcels.push(Parcel {
            ring: ring.clone(),
            block: block_id,
            occupant: None,
        });
    }
    let n_viable = viable.len();
    for ring in &spare {
        out.parcels.push(Parcel {
            ring: ring.clone(),
            block: block_id,
            occupant: None,
        });
    }

    if n_viable == 0 {
        // Nothing here can hold a building at all. No file is lost: each one is
        // recorded as overflow on the roomiest parcel, and the caller draws it.
        let widest = (base..out.parcels.len())
            .max_by(|a, b| area(&out.parcels[*a].ring).total_cmp(&area(&out.parcels[*b].ring)));
        for fi in files {
            out.overflow.push(Overflow {
                path: s.files[*fi as usize].path.clone(),
                shares: widest.map(|w| LotId(u32::try_from(w).expect("fits"))),
            });
            out.report.overflow += 1;
        }
        return;
    }

    // Roomiest first, for the surplus.
    let mut by_room: Vec<usize> = (0..n_viable).collect();
    by_room.sort_by(|&x, &y| viable[y].2.total_cmp(&viable[x].2).then_with(|| x.cmp(&y)));

    // # Which file lives on which of the street-facing plots
    //
    // The *set* of occupied parcels is still the frontage-first prefix, so
    // houses line the roads and the middle of the block stays open. Which file
    // takes which of them is decided by **matching size rank to plot rank**:
    // `subdivide_weighted` has already cut this block into plots whose areas
    // follow `sqrt(bytes)`, and the frontage sort reorders them, so without
    // re-pairing the two the biggest plot and the biggest file are only
    // accidentally the same parcel and PRD §7.3's driver is thrown away between
    // the two stages.
    //
    // Both ranks are broken to the item's own index, so this is a pure function
    // of the block's files and their sizes (PRD §7.4).
    let seats = files.len().min(n_viable);
    let mut by_plot: Vec<usize> = (0..seats).collect();
    by_plot.sort_by(|&x, &y| {
        area(&out.parcels[base + x].ring)
            .total_cmp(&area(&out.parcels[base + y].ring))
            .then_with(|| x.cmp(&y))
    });
    let mut by_size: Vec<usize> = (0..seats).collect();
    by_size.sort_by(|&x, &y| {
        weights
            .get(x)
            .copied()
            .unwrap_or(1.0)
            .total_cmp(&weights.get(y).copied().unwrap_or(1.0))
            .then_with(|| x.cmp(&y))
    });
    let mut seat_of = vec![0usize; seats];
    for (plot, file) in by_plot.iter().zip(by_size.iter()) {
        seat_of[*file] = *plot;
    }

    for (k, fi) in files.iter().enumerate() {
        if k < n_viable {
            out.parcels[base + seat_of[k]].occupant = Some(*fi);
            out.report.placed += 1;
            continue;
        }
        // A surplus file: halve the roomiest parcel and take one half, so it
        // gets a lot of its own rather than a share of a neighbour's. Two
        // narrower houses on one frontage is what a crowded quarter does, and it
        // keeps one occupant per lot all the way to the renderer.
        //
        // **Every** viable parcel is tried, roomiest first, not just the one
        // whose turn it is. A parcel that has already been halved twice can be
        // too narrow to halve again while its neighbour is untouched, and giving
        // up on the first refusal is what put a file on a shared lot after
        // twelve files were added to one block in a row — the incremental path,
        // where the surplus arrives one at a time and always lands on the same
        // frontage.
        // Three tiers over the hosts, each looser than the last, and the last
        // one accepts anything — because the alternative for this file is not a
        // better parcel, it is no lot of its own at all.
        //
        // 0. Cut no strip narrower than a building needs, and require **both**
        //    halves to be road-clear.
        // 1. Allow a narrower strip; the newcomer's half may be poor, but an
        //    occupied host still keeps a half it can stand its own house on.
        // 2. Anything that splits at all.
        //
        // The single tier this replaces took whichever piece came out of the
        // split first, and measured that cost one building in 4 565 — a file on
        // a half with almost no clearance, whose footprint the road guard then
        // shrank to 1.4·10⁻⁵. A bad half costs a small building; it never costs
        // a building in the carriageway, because `buildings::place_in_parcel`
        // checks that unconditionally afterwards.
        let start = (k - n_viable) % n_viable;
        let mut seated = false;
        'passes: for tier in 0..3u32 {
            let strict = tier == 0;
            let careful = tier < 2;
            for step in 0..n_viable {
                let host = base + by_room[(start + step) % n_viable];
                let mut pieces: Vec<Vec<Pt>> = Vec::new();
                subdivide(
                    &out.parcels[host].ring,
                    area(&out.parcels[host].ring) * 0.45,
                    2,
                    None,
                    seed.wrapping_add(k as u64)
                        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                        .wrapping_add(step as u64),
                    0,
                    // The strict pass will not cut a strip narrower than a
                    // building needs; the fallback pass may, because the
                    // alternative there is a file with no lot of its own.
                    if strict { min_w } else { min_w * 0.5 },
                    &any_parcel,
                    &mut pieces,
                );
                pieces.retain(|p| area(p) > 1e-9);
                if pieces.len() < 2 {
                    continue;
                }
                // The roomier half goes to the newcomer, the other stays with
                // the host — but the **host is already occupied**, so its half
                // has to carry a house too. Checking only the half handed over
                // is what left occupied parcels that no building could stand
                // on: the host's ring is replaced here, after its viability was
                // tested, and `crate::buildings` then had nowhere to put its
                // building. Measured, that was three unbuilt files of 1 104
                // before this stage gave the *roomier* half away and six after.
                //
                // So the strict pass requires both halves, and takes the
                // assignment that gives the newcomer the roomier one whenever
                // both work.
                let mut order: Vec<usize> = (0..pieces.len()).collect();
                order.sort_by(|&x, &y| {
                    let cx = probe(&pieces[x]).map_or(0.0, |(_, c)| c);
                    let cy = probe(&pieces[y]).map_or(0.0, |(_, c)| c);
                    cy.total_cmp(&cx).then_with(|| x.cmp(&y))
                });
                let host_occupied = out.parcels[host].occupant.is_some();
                let viable_pair = |g: usize, k: usize| -> bool {
                    buildable(&pieces[g]) && (!host_occupied || buildable(&pieces[k]))
                };
                // The host keeps a buildable half in **both** passes: the
                // fallback exists so a newcomer is not left without a lot, not
                // so an existing building can be demolished for it.
                let keeps_its_house = |k: usize| !host_occupied || buildable(&pieces[k]);
                let (gi, ki) = if viable_pair(order[0], order[1]) {
                    (order[0], order[1])
                } else if viable_pair(order[1], order[0]) {
                    (order[1], order[0])
                } else if !strict && keeps_its_house(order[1]) {
                    (order[0], order[1])
                } else if !strict && keeps_its_house(order[0]) {
                    (order[1], order[0])
                } else if careful {
                    continue;
                } else {
                    (order[0], order[1])
                };
                let give = pieces[gi].clone();
                let keep = pieces[ki].clone();
                order.retain(|i| *i != gi && *i != ki);
                out.parcels[host].ring = keep;
                out.parcels.push(Parcel {
                    ring: give,
                    block: block_id,
                    occupant: Some(*fi),
                });
                // A split of a non-convex parcel can leave a third piece. It is
                // ground, and ground is conserved: it becomes a vacant lot
                // rather than being dropped on the floor.
                for extra in &order {
                    out.parcels.push(Parcel {
                        ring: pieces[*extra].clone(),
                        block: block_id,
                        occupant: None,
                    });
                }
                out.report.placed += 1;
                seated = true;
                break 'passes;
            }
        }
        if !seated {
            let host = base + by_room[start];
            out.overflow.push(Overflow {
                path: s.files[*fi as usize].path.clone(),
                shares: Some(LotId(u32::try_from(host).expect("fits"))),
            });
            out.report.overflow += 1;
        }
    }
}

/// Index of the roomiest viable parcel that has not already refused to split.
///
/// `refused` is keyed on the quantised centroid, which is a property of where
/// the parcel is rather than of its index — indices move when `swap_remove`
/// runs (PRD §7.4).
fn largest_untried(
    viable: &[(Vec<Pt>, Pt, f64)],
    refused: &BTreeSet<(i64, i64)>,
    key: &dyn Fn(&[Pt]) -> (i64, i64),
) -> Option<usize> {
    (0..viable.len())
        .filter(|i| !refused.contains(&key(&viable[*i].0)))
        .max_by(|a, b| {
            area(&viable[*a].0)
                .total_cmp(&area(&viable[*b].0))
                .then_with(|| b.cmp(a))
        })
}

// ---------------------------------------------------------------------------
// Vacancy (PRD §7.5)
// ---------------------------------------------------------------------------

/// A lot whose building is gone (PRD §7.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Vacancy {
    /// The empty parcel.
    pub lot: LotId,
    /// The file that used to stand on it. Kept so the drill-down can say what
    /// was here, and so a re-added file can be recognised.
    pub former: LogicalPath,
    /// When it was vacated — the deleting commit's time, supplied by the caller.
    pub since: WallTime,
}

impl Vacancy {
    /// How far gone to seed: `0` at deletion, `1` after [`SEED_WINDOW_DAYS`].
    #[must_use]
    pub fn seed_progress(&self, now: WallTime) -> f32 {
        self.seed_progress_over(now, SEED_WINDOW_DAYS)
    }

    /// [`seed_progress`](Self::seed_progress) over a caller's window.
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
    /// A file's home slot is `hash(path) % lots` and it takes the first free lot
    /// at or after it, wrapping. Hashing rather than ranking is deliberate: a
    /// rank-ordered assignment moves every later file when one is inserted,
    /// which destroys exactly the spatial memory the product is for.
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

    /// Lots that have fully gone to seed by `now` (PRD §7.5).
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

/// A file's home slot: `hash(path) % lots`, from the logical path alone.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn lp(s: &str) -> LogicalPath {
        LogicalPath::new(s).expect("a valid test path")
    }

    fn square(s: f64) -> Vec<Pt> {
        vec![[0.0, 0.0], [s, 0.0], [s, s], [0.0, s]]
    }

    #[test]
    fn subdivision_conserves_area_and_hits_the_target() {
        let block = square(8.0);
        let mut out = Vec::new();
        subdivide(&block, 4.0, 16, None, 0xABCD, 0, 0.2, &any_parcel, &mut out);
        assert!(out.len() >= 8, "only {} parcels", out.len());
        let total: f64 = out.iter().map(|r| area(r)).sum();
        assert!(
            (total - 64.0).abs() < 1e-6,
            "parcels sum to {total}, block is 64"
        );
        for r in &out {
            assert!(
                area(r) <= 4.0 * 1.6,
                "a parcel of {} is far over target",
                area(r)
            );
        }
    }

    #[test]
    fn no_parcel_is_narrower_than_a_buildable_strip() {
        let block = square(8.0);
        let mut out = Vec::new();
        let min_w = 0.5;
        subdivide(
            &block,
            0.6,
            200,
            None,
            0x1234,
            0,
            min_w,
            &any_parcel,
            &mut out,
        );
        for r in &out {
            let ax = longest_axis(r);
            let (a0, a1) = extent_along(r, perp(ax));
            assert!(
                a1 - a0 >= min_w * 0.95,
                "a parcel is {} wide, below the {min_w} strip",
                a1 - a0
            );
        }
    }

    #[test]
    fn subdivision_is_deterministic() {
        let block = square(6.0);
        let mut a = Vec::new();
        let mut b = Vec::new();
        subdivide(&block, 1.0, 12, None, 0x5555, 0, 0.2, &any_parcel, &mut a);
        subdivide(&block, 1.0, 12, None, 0x5555, 0, 0.2, &any_parcel, &mut b);
        assert_eq!(a, b);
    }

    #[test]
    fn strips_run_the_same_way_rather_than_forming_a_quadtree() {
        // A long block should come out as a row of strips across its length, not
        // a checkerboard: that is the "deep narrow plots facing the street"
        // property PRD §7.2 step 4 is after.
        let block: Vec<Pt> = vec![[0.0, 0.0], [12.0, 0.0], [12.0, 2.0], [0.0, 2.0]];
        let mut out = Vec::new();
        subdivide(&block, 3.0, 8, None, 0x99, 0, 0.2, &any_parcel, &mut out);
        assert!(out.len() >= 4);
        for r in &out {
            let (x0, x1) = extent_along(r, [1.0, 0.0]);
            let (y0, y1) = extent_along(r, [0.0, 1.0]);
            assert!(
                (y1 - y0) > 1.9,
                "a parcel is {} deep: the block was cut the wrong way",
                y1 - y0
            );
            assert!(x1 - x0 < 6.0);
        }
    }

    #[test]
    fn a_vacancy_goes_to_seed_over_the_window() {
        let v = Vacancy {
            lot: LotId(3),
            former: lp("src/gone.rs"),
            since: WallTime::from_unix_seconds(0),
        };
        let day = 24 * 3600;
        assert!(v.seed_progress(WallTime::from_unix_seconds(0)) < 0.01);
        let half = WallTime::from_unix_seconds(day * i64::from(SEED_WINDOW_DAYS) / 2);
        assert!((v.seed_progress(half) - 0.5).abs() < 0.02);
        let full = WallTime::from_unix_seconds(day * i64::from(SEED_WINDOW_DAYS) + day);
        assert!(v.is_overgrown(full));
    }

    #[test]
    fn the_ledger_frees_and_reuses_a_lot() {
        let mut lots: Vec<Lot> = (0..4)
            .map(|i| Lot {
                id: LotId(i),
                block: crate::BlockId(0),
                boundary: crate::Polygon::default(),
                occupant: None,
            })
            .collect();
        let mut ledger = VacancyLedger::new();
        let ids: Vec<LotId> = lots.iter().map(|l| l.id).collect();
        let a = ledger
            .settle(&mut lots, &ids, &lp("src/a.rs"))
            .expect("a lot");
        assert!(lots
            .iter()
            .any(|l| l.occupant.as_ref() == Some(&lp("src/a.rs"))));
        assert!(ledger.is_empty());
        ledger.vacate(&mut lots, a, WallTime::from_unix_seconds(10));
        assert_eq!(ledger.len(), 1);
        let b = ledger
            .settle(&mut lots, &ids, &lp("src/b.rs"))
            .expect("a lot");
        // Re-settling on the freed lot clears its vacancy record.
        if b == a {
            assert!(ledger.get(a).is_none());
        }
    }

    #[test]
    fn settling_is_stable_when_a_file_is_added() {
        let make = || -> Vec<Lot> {
            (0..8)
                .map(|i| Lot {
                    id: LotId(i),
                    block: crate::BlockId(0),
                    boundary: crate::Polygon::default(),
                    occupant: None,
                })
                .collect()
        };
        let ids: Vec<LotId> = (0..8).map(LotId).collect();
        let mut first = make();
        let mut ledger = VacancyLedger::new();
        let mut before = BTreeMap::new();
        for name in ["a.rs", "b.rs", "c.rs"] {
            let p = lp(name);
            before.insert(p.clone(), ledger.settle(&mut first, &ids, &p));
        }
        let mut second = make();
        let mut ledger2 = VacancyLedger::new();
        let mut after = BTreeMap::new();
        for name in ["a.rs", "b.rs", "c.rs", "d.rs"] {
            let p = lp(name);
            let got = ledger2.settle(&mut second, &ids, &p);
            after.insert(p, got);
        }
        for (path, lot) in &before {
            assert_eq!(
                after.get(path),
                Some(lot),
                "{} moved when a file was added",
                path.as_str()
            );
        }
    }

    #[test]
    fn the_oldest_ground_is_surveyed_finer_than_the_newest() {
        // PRD §7.1's age structure, as plot size. The judge's fifth required
        // change: vary the grain by district age *and* by district file count,
        // "not by growth-sequence fraction alone".
        let core = by_age(MAX_PARCEL_GRAINS_CORE, MAX_PARCEL_GRAINS_RIM, 0.0);
        let rim = by_age(MAX_PARCEL_GRAINS_CORE, MAX_PARCEL_GRAINS_RIM, 1.0);
        assert_eq!(core, MAX_PARCEL_GRAINS_CORE);
        assert_eq!(rim, MAX_PARCEL_GRAINS_RIM);
        assert!(rim > core * 2.5, "{rim} against {core}");
        let mut previous = 0.0;
        for i in 0..=20 {
            let t = f64::from(i) / 20.0;
            let v = by_age(MAX_PARCEL_GRAINS_CORE, MAX_PARCEL_GRAINS_RIM, t);
            assert!(v >= previous, "the age ramp is not monotone at {t}");
            previous = v;
        }
        // Occupancy runs the same way: the old town is built out and the rim
        // keeps ground open, which is where the density gradient comes from.
        const { assert!(LOT_SLACK_RIM > LOT_SLACK_CORE) };
        const { assert!(LOT_SLACK_CORE >= 1.0) };
        // And a vacant plot is bigger than its neighbours, so the rim's
        // emptiness arrives as paddocks rather than as unsold specks.
        const { assert!(VACANT_WEIGHT > 1.0) };
    }

    #[test]
    fn urbanity_reads_the_growth_order_and_the_map() {
        assert_eq!(urbanity(0.0, 0.0), 0.0);
        assert_eq!(urbanity(1.0, 1.0), 1.0);
        // Neither term alone can claim the whole range: an old block far out is
        // partly suburban, a new block in the middle is partly urban.
        let far_but_old = urbanity(0.0, 1.0);
        let near_but_new = urbanity(1.0, 0.0);
        assert!(far_but_old > 0.0 && far_but_old < 1.0, "{far_but_old}");
        assert!(near_but_new > 0.0 && near_but_new < 1.0, "{near_but_new}");
        assert!((far_but_old + near_but_new - 1.0).abs() < 1e-12);
        // Monotone in both.
        let mut previous = -1.0;
        for i in 0..=20 {
            let v = urbanity(0.5, f64::from(i) / 20.0);
            assert!(v >= previous, "urbanity fell with radius");
            previous = v;
        }
    }

    #[test]
    fn a_plot_is_the_size_of_the_file_that_stands_on_it() {
        // PRD §7.3's driver, read here because a footprint can only be as big
        // as the ground under it. The visual review's target is a ~10x spread
        // within one block.
        let block = square(20.0);
        let items = vec![1.0, 3.0, 1.0, 3.0, 1.0, 3.0, 1.0, 3.0];
        let mut out = Vec::new();
        subdivide_weighted(&block, &items, None, 0x5127, 0, 0.2, &any_parcel, &mut out);
        assert_eq!(out.len(), items.len(), "one parcel per item");
        let total: f64 = out.iter().map(|r| area(r)).sum();
        assert!((total - 400.0).abs() < 1e-6, "ground was lost: {total}");
        let mut areas: Vec<f64> = out.iter().map(|r| area(r)).collect();
        areas.sort_by(f64::total_cmp);
        let ratio = areas[areas.len() - 1] / areas[0];
        assert!(ratio > 2.0, "the weights bought only a {ratio:.2}x spread");
        // The four heavy items take about three times the four light ones.
        let heavy: f64 = areas[4..].iter().sum();
        let light: f64 = areas[..4].iter().sum();
        assert!(
            (heavy / light - 3.0).abs() < 0.6,
            "heavy {heavy} against light {light}"
        );
    }

    #[test]
    fn the_weighted_survey_is_deterministic_and_conserves_the_ground() {
        let block: Vec<Pt> = vec![[0.0, 0.0], [9.0, 0.4], [8.4, 6.1], [0.6, 5.2]];
        let items: Vec<f64> = (0..11).map(|i| 0.5 + f64::from(i % 5) * 0.6).collect();
        let mut a = Vec::new();
        let mut b = Vec::new();
        subdivide_weighted(
            &block,
            &items,
            None,
            0x00C0_FFEE,
            0,
            0.15,
            &any_parcel,
            &mut a,
        );
        subdivide_weighted(
            &block,
            &items,
            None,
            0x00C0_FFEE,
            0,
            0.15,
            &any_parcel,
            &mut b,
        );
        assert_eq!(a, b);
        let total: f64 = a.iter().map(|r| area(r)).sum();
        assert!((total - area(&block)).abs() < 1e-6, "ground was lost");
        assert_eq!(a.len(), items.len());
    }

    #[test]
    fn a_weighted_cut_that_would_orphan_a_parcel_is_not_made() {
        // The same merge `subdivide` performs: refusing the cut is what keeps
        // the ground with the sibling that can use it.
        let block = square(8.0);
        let items = vec![1.0; 12];
        let mut merged = Vec::new();
        subdivide_weighted(
            &block,
            &items,
            None,
            0xABCD,
            0,
            0.2,
            &|_| false,
            &mut merged,
        );
        assert_eq!(merged.len(), 1, "a cut was made past a failing predicate");
        assert!((area(&merged[0]) - 64.0).abs() < 1e-9);

        let mut coarse = Vec::new();
        subdivide_weighted(
            &block,
            &items,
            None,
            0xABCD,
            0,
            0.2,
            &|r: &[Pt]| area(r) >= 8.0,
            &mut coarse,
        );
        let total: f64 = coarse.iter().map(|r| area(r)).sum();
        assert!((total - 64.0).abs() < 1e-6, "ground was lost: {total}");
        for r in &coarse {
            assert!(area(r) >= 8.0, "an orphan of {} survived", area(r));
        }
    }

    #[test]
    fn no_weighted_parcel_is_narrower_than_a_buildable_strip() {
        let block = square(8.0);
        let items = vec![1.0; 200];
        let min_w = 0.5;
        let mut out = Vec::new();
        subdivide_weighted(
            &block,
            &items,
            None,
            0x1234,
            0,
            min_w,
            &any_parcel,
            &mut out,
        );
        for r in &out {
            let ax = longest_axis(r);
            let (a0, a1) = extent_along(r, perp(ax));
            assert!(
                a1 - a0 >= min_w * 0.95,
                "a parcel is {} wide, below the {min_w} strip",
                a1 - a0
            );
        }
    }

    #[test]
    fn dealing_items_between_pieces_loses_none() {
        let pieces = vec![square(1.0), square(2.0), square(3.0)];
        for n in 0..20usize {
            let counts = deal(&pieces, n);
            assert_eq!(
                counts.iter().sum::<usize>(),
                n,
                "{n} items became {counts:?}"
            );
        }
        // Proportional to area: the 9-unit piece takes most of them.
        let counts = deal(&pieces, 14);
        assert!(counts[2] > counts[1] && counts[1] > counts[0], "{counts:?}");
    }

    #[test]
    fn a_bigger_district_is_surveyed_finer() {
        assert!(district_grain(400) < district_grain(DISTRICT_REF_FILES as usize));
        assert!(district_grain(2) > district_grain(DISTRICT_REF_FILES as usize));
        assert_eq!(district_grain(1_000_000), DISTRICT_FINEST);
        assert_eq!(district_grain(0), DISTRICT_COARSEST);
        // A fourth root: forty times the files is two and a half times the
        // grain, not forty.
        let ratio = district_grain(4) / district_grain(160);
        assert!(ratio > 2.0 && ratio < 3.0, "{ratio}");
    }

    #[test]
    fn the_growth_order_is_the_age_and_not_the_oldest_plot_near_by() {
        // `BlockPlan::birth` is a minimum over the face, so nearly every block
        // inherits the age of the earliest ground anywhere near it. The block's
        // own files are what PRD §7.1 actually names.
        assert_eq!(ground_age(0, 100), 0.0);
        assert_eq!(ground_age(100, 100), 1.0);
        assert_eq!(ground_age(u32::MAX, 100), 1.0);
        assert!((ground_age(25, 100) - 0.25).abs() < 1e-12);
    }

    #[test]
    fn a_cut_that_would_orphan_a_parcel_is_not_made() {
        // The judge: "merge unbuildable lots into a neighbour polygon before
        // seating". Refusing the cut is that merge, done while the neighbour is
        // still known — the sibling piece.
        let block = square(8.0);
        let mut greedy = Vec::new();
        subdivide(
            &block,
            4.0,
            16,
            None,
            0xABCD,
            0,
            0.2,
            &any_parcel,
            &mut greedy,
        );
        assert!(greedy.len() > 1);

        // A predicate nothing can satisfy: the block must come back whole, and
        // the ground is conserved either way.
        let mut merged = Vec::new();
        subdivide(
            &block,
            4.0,
            16,
            None,
            0xABCD,
            0,
            0.2,
            &|_| false,
            &mut merged,
        );
        assert_eq!(merged.len(), 1, "a cut was made past a failing predicate");
        assert!((area(&merged[0]) - 64.0).abs() < 1e-9);

        // A predicate that only rejects small pieces stops the recursion early
        // and still tiles the block exactly.
        let mut coarse = Vec::new();
        subdivide(
            &block,
            0.5,
            128,
            None,
            0xABCD,
            0,
            0.2,
            &|r: &[Pt]| area(r) >= 4.0,
            &mut coarse,
        );
        let total: f64 = coarse.iter().map(|r| area(r)).sum();
        assert!((total - 64.0).abs() < 1e-6, "ground was lost: {total}");
        for r in &coarse {
            assert!(area(r) >= 4.0, "an orphan of {} survived", area(r));
        }
    }

    #[test]
    fn a_vacant_lot_is_told_apart_from_ground_nobody_ever_built_on() {
        // PRD §7.5: a deleted file leaves a lot that goes to seed. `occupant:
        // None` alone cannot say that — the ledger is what carries the
        // difference through to the renderer.
        let mut lots: Vec<Lot> = (0..2)
            .map(|i| Lot {
                id: LotId(i),
                block: crate::BlockId(0),
                boundary: crate::Polygon::default(),
                occupant: None,
            })
            .collect();
        let mut ledger = VacancyLedger::new();
        let built = ledger
            .settle(&mut lots, &[LotId(0)], &lp("src/gone.rs"))
            .expect("a lot");
        let when = WallTime::from_unix_seconds(0);
        ledger.vacate(&mut lots, built, when);

        // Both lots now read as unoccupied.
        assert!(lots.iter().all(Lot::is_vacant));
        // Only one of them has a history, and it has a progress the renderer can
        // draw.
        assert!(ledger.get(LotId(0)).is_some());
        assert!(ledger.get(LotId(1)).is_none());
        let half = WallTime::from_unix_seconds(24 * 3600 * i64::from(SEED_WINDOW_DAYS) / 2);
        let progress = ledger.get(LotId(0)).expect("a vacancy").seed_progress(half);
        assert!(progress > 0.4 && progress < 0.6, "{progress}");
        assert_eq!(
            ledger.get(LotId(0)).expect("a vacancy").former,
            lp("src/gone.rs")
        );
    }
}
