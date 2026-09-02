//! Stage 1 — the road *substrate*.
//!
//! > **The road network is the boundary network of the settled ground.** A road
//! > is the line where one parcel's territory stops and the next one's begins.
//!
//! That is a Voronoi diagram of the plot positions, and it is planar, connected
//! and full of closed faces **by construction** rather than by tuning a snap
//! radius. Every cell contributes one independent cycle; there is no parameter
//! setting at which it degenerates into a tree, which is exactly the failure
//! mode PRD §7.2 warns about and the first M1 attempt hit.
//!
//! # Half-plane clipping, not a triangulation
//!
//! A cell is built by starting from a fixed frame and clipping by the
//! perpendicular bisector with every neighbour inside a query radius, widening
//! the radius until it provably covers the cell (`2 · rmax ≤ radius`). No
//! degenerate predicates, no insertion-order sensitivity, and the neighbour set
//! is exact rather than heuristic.
//!
//! **This is the code that must stay `f64`** (ADR-0053). See [`crate::geom`].
//!
//! # The edge of town is where the ground stops, not a drawn limit
//!
//! An earlier port of this module bounded every cell by clipping it to a convex
//! *city limit* polygon — a wobbled nineteen-gon's hull — and computed the
//! diagram inside a fan of wedges cut out of it. Both are gone, and the reason
//! is that they were the whole of the "pie chart on a coin": the limit made the
//! silhouette convex (solidity 0.9975, where a circle is 1.00) and the wedges
//! made four to nine district boundaries exactly straight lines from the middle
//! of the city to its edge.
//!
//! The boundary is now what the prototype in `docs/design/accretion` used and
//! what the accretion actually produces: a ring of **phantom** plots laid on a
//! lattice wherever the ground is empty but within `[PHANTOM_NEAR, PHANTOM_FAR]`
//! separations of the settlement. Phantom cells are computed and discarded; the
//! boundary between a real cell and a phantom one is the town's perimeter road.
//! The silhouette is therefore the outline of the settled ground — lobed,
//! concave, with inlets where a quarter grew round a fold of terrain.
//!
//! **The lattice is anchored at the world origin, never at the bounding box.** A
//! bbox-anchored lattice would shift every boundary phantom the moment the town
//! grew by one plot on the far side, and PRD §7.7 forbids exactly that.
//!
//! The two costs the previous port named for phantoms were real and are paid
//! elsewhere: an interior gap wide enough to grow phantoms in becomes a hole in
//! the map, and a settlement that grows in disconnected islands becomes a
//! disconnected road graph. [`crate::accrete`] answers both with a hard rule —
//! a new plot must be no further than
//! [`crate::accrete::Params::touch_max`] separations from the nearest settled
//! plot — so there is no gap to grow a phantom in and no island to disconnect.
//! `components = 1` is measured, and the rule is why.

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

use crate::determinism::{quantize_f64, SeededRng};
use crate::geom::{
    add, clip_halfplane, dedupe_ring, dist, dist2, dot, len, mul, qp, signed_area2, sub, Pt,
};

/// Radius, in units of `sep`, within which a new plot can disturb a cell.
const AFFECT_RADIUS: f64 = 3.6;

/// Lattice pitch of the phantom ring, in units of `sep`.
///
/// Below one separation the ring is denser than the town it bounds and the
/// perimeter road turns into a fringe of tiny cells; above it, gaps open and a
/// real cell escapes to the frame. `0.80` is the prototype's measured value and
/// it holds at both scales here.
const PHANTOM_PITCH: f64 = 0.80;

/// How far a lattice site jitters, as a fraction of the pitch.
///
/// Seeded from the site's own integer coordinates and the layout seed, so the
/// jitter is a property of the *place* and not of the growth order: a phantom
/// that appears, vanishes and reappears comes back to the same point.
const PHANTOM_JITTER: f64 = 0.55;

/// Nearest a phantom may sit to real ground, in separations.
///
/// Any closer and the phantom's bisector cuts into the real cell it is supposed
/// to bound, shaving frontage off the outermost parcels.
const PHANTOM_NEAR: f64 = 1.02;

/// Furthest a phantom may sit from real ground, in separations.
///
/// Any further and it is open country, not the edge of town: it would draw a
/// road round nothing.
const PHANTOM_FAR: f64 = 2.15;

/// How far past the settlement the lattice is swept, in separations.
const PHANTOM_MARGIN: f64 = 3.0;

/// The territories of the settled plots.
// `cells.cells` reads oddly and is the honest name for it: the struct is the
// diagram, the field is the rings.
#[allow(clippy::struct_field_names)]
#[derive(Debug, Clone, Default)]
pub(crate) struct Cells {
    /// One ring per real plot, counter-clockwise, quantised. Empty only when the
    /// plot coincides with another, which the separation rule prevents.
    pub(crate) cells: Vec<Vec<Pt>>,
    /// Phantom positions, in lattice-key order.
    phantoms: Vec<Pt>,
    /// Which lattice sites are currently phantoms.
    phantom_keys: BTreeSet<(i64, i64)>,
    /// Lattice pitch in world units.
    pitch: f64,
    /// The layout seed the lattice jitter is drawn from.
    seed: u64,
}

impl Cells {
    /// Plots whose territory came out empty.
    ///
    /// A plot with no cell contributes no boundary to the road graph and lands
    /// inside no face, so its files are attached to whichever block is nearest
    /// (see [`crate::city::CityReport::plots_off_face`]). Zero is the design's
    /// promise; the number exists so a breach is visible rather than only
    /// audible as a large overflow.
    pub(crate) fn empty(&self) -> usize {
        self.cells.iter().filter(|c| c.len() < 3).count()
    }
}

/// A uniform bucket grid over a fixed point set.
#[derive(Debug)]
struct SiteGrid {
    cell: f64,
    buckets: BTreeMap<(i32, i32), Vec<u32>>,
    pts: Vec<Pt>,
}

impl SiteGrid {
    fn build(pts: Vec<Pt>, cell: f64) -> Self {
        let cell = cell.max(1e-6);
        let mut buckets: BTreeMap<(i32, i32), Vec<u32>> = BTreeMap::new();
        for (i, p) in pts.iter().enumerate() {
            let k = ((p[0] / cell).floor() as i32, (p[1] / cell).floor() as i32);
            buckets
                .entry(k)
                .or_default()
                .push(u32::try_from(i).expect("site count fits in u32"));
        }
        Self { cell, buckets, pts }
    }

    fn query(&self, p: Pt, r: f64, out: &mut Vec<u32>) {
        out.clear();
        let n = (r / self.cell).ceil() as i32;
        let kx = (p[0] / self.cell).floor() as i32;
        let ky = (p[1] / self.cell).floor() as i32;
        let r2 = r * r;
        for dy in -n..=n {
            for dx in -n..=n {
                if let Some(b) = self.buckets.get(&(kx + dx, ky + dy)) {
                    for &i in b {
                        if dist2(self.pts[i as usize], p) <= r2 {
                            out.push(i);
                        }
                    }
                }
            }
        }
        out.sort_unstable();
    }

    /// Distance to the nearest site, or infinity when none is within `r`.
    fn nearest(&self, p: Pt, r: f64, scratch: &mut Vec<u32>) -> f64 {
        self.query(p, r, scratch);
        scratch
            .iter()
            .map(|&i| dist(self.pts[i as usize], p))
            .fold(f64::INFINITY, f64::min)
    }
}

/// Position of lattice site `(ix, iy)`.
fn lattice(ix: i64, iy: i64, pitch: f64, seed: u64) -> Pt {
    let mut rng = SeededRng::for_seed(
        seed ^ (ix as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ (iy as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F),
        "phantom.jitter",
    );
    let jx = (rng.next_f64() - 0.5) * pitch * PHANTOM_JITTER;
    let jy = (rng.next_f64() - 0.5) * pitch * PHANTOM_JITTER;
    qp([ix as f64 * pitch + jx, iy as f64 * pitch + jy])
}

/// Lay phantom plots wherever the ground is empty but close enough to the
/// settlement to bound it.
fn phantom_ring(real: &[Pt], sep: f64, pitch: f64, seed: u64) -> BTreeSet<(i64, i64)> {
    let mut out = BTreeSet::new();
    if real.is_empty() {
        return out;
    }
    let mut lo = real[0];
    let mut hi = real[0];
    for p in real {
        lo = [lo[0].min(p[0]), lo[1].min(p[1])];
        hi = [hi[0].max(p[0]), hi[1].max(p[1])];
    }
    let margin = sep * PHANTOM_MARGIN;
    let grid = SiteGrid::build(real.to_vec(), sep * 1.4);
    let mut scratch = Vec::new();
    let ix0 = ((lo[0] - margin) / pitch).floor() as i64;
    let ix1 = ((hi[0] + margin) / pitch).ceil() as i64;
    let iy0 = ((lo[1] - margin) / pitch).floor() as i64;
    let iy1 = ((hi[1] + margin) / pitch).ceil() as i64;
    let near = sep * PHANTOM_NEAR;
    let far = sep * PHANTOM_FAR;
    for iy in iy0..=iy1 {
        for ix in ix0..=ix1 {
            let c = lattice(ix, iy, pitch, seed);
            let nearest = grid.nearest(c, far, &mut scratch);
            if nearest >= near && nearest <= far {
                out.insert((ix, iy));
            }
        }
    }
    out
}

/// Build the territory of every real plot, bounded by a phantom ring.
pub(crate) fn build(real: &[Pt], sep: f64, seed: u64) -> Cells {
    let pitch = sep * PHANTOM_PITCH;
    let keys = phantom_ring(real, sep, pitch, seed);
    let mut cells = Cells {
        cells: vec![Vec::new(); real.len()],
        phantoms: keys
            .iter()
            .map(|&(a, b)| lattice(a, b, pitch, seed))
            .collect(),
        phantom_keys: keys,
        pitch,
        seed,
    };
    let which: Vec<usize> = (0..real.len()).collect();
    compute_cells(real, sep, &mut cells, &which);
    cells
}

/// Re-test the lattice around a newly settled plot and return every phantom
/// position that appeared or vanished. Only this window can change.
pub(crate) fn update_phantoms(cells: &mut Cells, real: &[Pt], at: Pt, sep: f64) -> Vec<Pt> {
    let pitch = cells.pitch;
    if pitch <= 0.0 {
        return Vec::new();
    }
    let seed = cells.seed;
    let win = sep * (PHANTOM_FAR + PHANTOM_MARGIN);
    let ix0 = ((at[0] - win) / pitch).floor() as i64;
    let ix1 = ((at[0] + win) / pitch).ceil() as i64;
    let iy0 = ((at[1] - win) / pitch).floor() as i64;
    let iy1 = ((at[1] + win) / pitch).ceil() as i64;
    let grid = SiteGrid::build(real.to_vec(), sep * 1.4);
    let near = sep * PHANTOM_NEAR;
    let far = sep * PHANTOM_FAR;
    let mut scratch = Vec::new();
    let mut changed = Vec::new();
    let mut dirty = false;
    for iy in iy0..=iy1 {
        for ix in ix0..=ix1 {
            let c = lattice(ix, iy, pitch, seed);
            let nearest = grid.nearest(c, far, &mut scratch);
            let want = nearest >= near && nearest <= far;
            let had = cells.phantom_keys.contains(&(ix, iy));
            if want != had {
                changed.push(c);
                dirty = true;
                if want {
                    cells.phantom_keys.insert((ix, iy));
                } else {
                    cells.phantom_keys.remove(&(ix, iy));
                }
            }
        }
    }
    if dirty {
        cells.phantoms = cells
            .phantom_keys
            .iter()
            .map(|&(a, b)| lattice(a, b, pitch, seed))
            .collect();
    }
    changed
}

/// Recompute only the listed plots' territories, reusing the rest.
pub(crate) fn rebuild_subset(real: &[Pt], sep: f64, cells: &mut Cells, which: &[usize]) {
    cells.cells.resize(real.len(), Vec::new());
    compute_cells(real, sep, cells, which);
}

/// Which plots can change when the sites in `moved` appear.
///
/// A cell depends only on plots closer than twice its own radius, so inserting
/// one plot can only disturb its immediate neighbourhood. This is the whole
/// incremental story: one growth step touches a handful of cells.
pub(crate) fn affected(real: &[Pt], moved: &[Pt], sep: f64) -> Vec<usize> {
    let r2 = (sep * AFFECT_RADIUS) * (sep * AFFECT_RADIUS);
    (0..real.len())
        .filter(|&i| moved.iter().any(|m| dist2(real[i], *m) <= r2))
        .collect()
}

/// The clipping itself.
fn compute_cells(real: &[Pt], sep: f64, cells: &mut Cells, which: &[usize]) {
    let n_real = real.len();
    let mut all: Vec<Pt> = Vec::with_capacity(n_real + cells.phantoms.len());
    all.extend_from_slice(real);
    all.extend_from_slice(&cells.phantoms);
    let grid = SiteGrid::build(all.clone(), sep * 1.4);

    // A fixed frame, not the settlement's bounding box: a box that grows with
    // the town would perturb every boundary cell on every growth step. It is
    // never reached — the phantom ring bounds every real cell long before this
    // — and it exists only so the clip has somewhere to start.
    let far = (sep * 4096.0).max(4096.0);
    let frame = vec![[-far, -far], [far, -far], [far, far], [-far, far]];

    let mut scratch = Vec::new();
    for &i in which {
        if i >= n_real {
            continue;
        }
        let s = all[i];
        let mut radius = sep * 3.2;
        let mut ring;
        loop {
            grid.query(s, radius, &mut scratch);
            ring = frame.clone();
            for &j in &scratch {
                if j as usize == i {
                    continue;
                }
                let o = all[j as usize];
                let d = sub(o, s);
                let l = len(d);
                if l < 1e-12 {
                    continue;
                }
                // The perpendicular bisector: `dot(unit(d), x) <= dot(unit(d), mid)`.
                let nrm = [d[0] / l, d[1] / l];
                let mid = mul(add(s, o), 0.5);
                ring = clip_halfplane(&ring, nrm, dot(nrm, mid));
                if ring.len() < 3 {
                    break;
                }
            }
            if ring.len() < 3 {
                break;
            }
            // A site farther than `2 · rmax` cannot cut this cell.
            let rmax = ring.iter().map(|p| dist(*p, s)).fold(0.0f64, f64::max);
            if 2.0 * rmax <= radius + 1e-9 {
                break;
            }
            radius = 2.0 * rmax + sep;
            if radius > sep * 200.0 {
                break;
            }
        }
        let mut ring: Vec<Pt> = dedupe_ring(ring.into_iter().map(qp).collect(), 1e-6);
        if signed_area2(&ring) < 0.0 {
            ring.reverse();
        }
        if ring.len() < 3 {
            ring.clear();
        }
        cells.cells[i] = ring;
    }
}

/// Quantise onto the weld lattice used to identify shared cell corners.
///
/// This is PRD §7.2's snap, applied where it actually bridges something: two
/// neighbouring cells compute the same corner from the same bisector equations
/// but in a different clip order, and coarsening the lattice deliberately merges
/// near-coincident corners into one junction.
#[inline]
pub(crate) fn weld_key(p: Pt, tol: f64) -> (i64, i64) {
    (
        (quantize_f64(p[0]) / tol).round() as i64,
        (quantize_f64(p[1]) / tol).round() as i64,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::{area, contains};

    const SEED: u64 = 0x5EED_0001;

    fn lattice_sites(n: i32, step: f64) -> Vec<Pt> {
        let mut out = Vec::new();
        for y in 0..n {
            for x in 0..n {
                out.push(qp([f64::from(x) * step, f64::from(y) * step]));
            }
        }
        out
    }

    #[test]
    fn every_cell_is_bounded_and_contains_its_site() {
        let sites = lattice_sites(6, 1.0);
        let cells = build(&sites, 1.0, SEED);
        assert_eq!(cells.cells.len(), sites.len());
        for (i, ring) in cells.cells.iter().enumerate() {
            assert!(ring.len() >= 3, "cell {i} is degenerate");
            assert!(area(ring) > 0.0, "cell {i} has no area");
            assert!(contains(ring, sites[i]), "cell {i} lost its own site");
        }
    }

    /// The whole point of the phantom ring: no real cell runs away to the frame.
    #[test]
    fn the_phantom_ring_bounds_every_cell() {
        let sites = lattice_sites(6, 1.0);
        let cells = build(&sites, 1.0, SEED);
        assert!(!cells.phantoms.is_empty(), "no phantoms were laid");
        let mut worst = 0.0f64;
        for (i, ring) in cells.cells.iter().enumerate() {
            for v in ring {
                worst = worst.max(dist(*v, sites[i]));
            }
        }
        assert!(
            worst < 4.0,
            "a cell reached {worst} from its site: the ring did not bound it"
        );
    }

    /// The silhouette follows the ground, so a settlement with a bite out of it
    /// has a bite out of its outline. This is the property the convex city limit
    /// destroyed.
    #[test]
    fn the_outline_is_concave_when_the_ground_is() {
        // An L-shaped settlement: a full 6x6 block with the top-right quadrant
        // removed.
        let mut sites = Vec::new();
        for y in 0..6 {
            for x in 0..6 {
                if x >= 3 && y >= 3 {
                    continue;
                }
                sites.push(qp([f64::from(x), f64::from(y)]));
            }
        }
        let cells = build(&sites, 1.0, SEED);
        // No cell may reach into the missing quadrant's far corner.
        let hole = [4.6, 4.6];
        for (i, ring) in cells.cells.iter().enumerate() {
            assert!(
                !contains(ring, hole),
                "cell {i} covered ground nobody settled"
            );
        }
    }

    #[test]
    fn a_rebuilt_subset_matches_a_full_rebuild() {
        let sites = lattice_sites(5, 1.0);
        let full = build(&sites, 1.0, SEED);
        let mut partial = build(&sites, 1.0, SEED);
        let which: Vec<usize> = (0..sites.len()).step_by(3).collect();
        rebuild_subset(&sites, 1.0, &mut partial, &which);
        for i in which {
            assert_eq!(full.cells[i], partial.cells[i], "cell {i} moved");
        }
    }

    #[test]
    fn a_new_plot_disturbs_only_its_neighbourhood() {
        let mut sites = lattice_sites(7, 1.0);
        let before = build(&sites, 1.0, SEED);
        let at = qp([3.5, 3.5]);
        sites.push(at);
        let mut after = before.clone();
        let moved = update_phantoms(&mut after, &sites, at, 1.0);
        let mut touched = vec![at];
        touched.extend(moved);
        let which = affected(&sites, &touched, 1.0);
        rebuild_subset(&sites, 1.0, &mut after, &which);
        let reference = build(&sites, 1.0, SEED);
        for i in 0..sites.len() - 1 {
            assert_eq!(
                after.cells[i], reference.cells[i],
                "cell {i} disagrees with a full rebuild"
            );
        }
        assert!(
            which.len() < sites.len(),
            "the affected set was the whole city"
        );
    }

    /// The lattice is anchored at the origin, so a phantom is a property of the
    /// place: the same site comes back to the same point whatever else moved.
    #[test]
    fn the_phantom_lattice_is_anchored_at_the_origin() {
        let a = lattice(3, -4, 0.8, SEED);
        let b = lattice(3, -4, 0.8, SEED);
        assert_eq!(a, b);
        assert!(dist(a, [3.0 * 0.8, -4.0 * 0.8]) < 0.8 * PHANTOM_JITTER);
    }

    #[test]
    fn welding_merges_near_coincident_corners() {
        assert_eq!(weld_key([1.0, 1.0], 0.004), weld_key([1.001, 1.0], 0.004));
        assert_ne!(weld_key([1.0, 1.0], 0.004), weld_key([1.02, 1.0], 0.004));
    }
}
