//! Stage 1 — the road *substrate*.
//!
//! The road network is the boundary network of the settled ground: the set of
//! edges where one plot's territory meets the next. That is a Voronoi diagram
//! of the plot positions, and it is planar, connected and full of closed faces
//! *by construction* rather than by tuning a snap radius.
//!
//! Cells are built by half-plane clipping rather than by a Delaunay
//! triangulation: no degenerate-predicate handling, no insertion-order
//! sensitivity, and the neighbour set is exact because the query radius is
//! widened until it provably covers the cell.
//!
//! A ring of **phantom** plots is laid just outside the settled ground so every
//! real cell is bounded. Phantom cells are discarded; the boundary between real
//! and phantom is the town's perimeter road.

use std::collections::BTreeMap;

use polis_layout::determinism::{quantize_f64, SeededRng};

use crate::geom::*;

pub struct Cells {
    /// One ring per real plot, counter-clockwise, quantised.
    pub cells: Vec<Vec<P>>,
    /// Phantom positions, in lattice-key order.
    pub phantoms: Vec<P>,
    /// Which lattice sites are currently phantoms.
    pub phantom_keys: std::collections::BTreeSet<(i64, i64)>,
    pub g: f64,
    pub seed: u64,
}

struct SiteGrid {
    cell: f64,
    buckets: BTreeMap<(i32, i32), Vec<u32>>,
    pts: Vec<P>,
}

impl SiteGrid {
    fn build(pts: Vec<P>, cell: f64) -> Self {
        let mut buckets: BTreeMap<(i32, i32), Vec<u32>> = BTreeMap::new();
        for (i, p) in pts.iter().enumerate() {
            let k = (
                (p[0] / cell).floor() as i32,
                (p[1] / cell).floor() as i32,
            );
            buckets.entry(k).or_default().push(i as u32);
        }
        Self { cell, buckets, pts }
    }
    fn query(&self, p: P, r: f64, out: &mut Vec<u32>) {
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
}

/// Position of lattice site `(ix, iy)`.
///
/// The lattice is anchored at the **world origin**, never at the settlement's
/// bounding box. That is what makes growth incremental: a new plot on one edge
/// of town must not shift the phantom that bounds a cell on the opposite edge,
/// and a bbox-anchored lattice would move every one of them (PRD §7.7).
#[inline]
fn lattice(ix: i64, iy: i64, g: f64, seed: u64) -> P {
    let mut rng = SeededRng::for_seed(
        seed ^ (ix as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ (iy as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F),
        "phantom.jitter",
    );
    let jx = (rng.next_f64() - 0.5) * g * 0.55;
    let jy = (rng.next_f64() - 0.5) * g * 0.55;
    qp([ix as f64 * g + jx, iy as f64 * g + jy])
}

/// Lay phantom plots on the lattice wherever the ground is empty but close
/// enough to the settlement to bound it.
fn phantom_ring(real: &[P], sep: f64, seed: u64) -> std::collections::BTreeSet<(i64, i64)> {
    let mut out = std::collections::BTreeSet::new();
    if real.is_empty() {
        return out;
    }
    let (lo, hi) = bounds(real);
    let margin = sep * 3.0;
    let g = sep * 0.80;
    let grid = SiteGrid::build(real.to_vec(), sep * 1.4);
    let mut scratch = Vec::new();
    let ix0 = ((lo[0] - margin) / g).floor() as i64;
    let ix1 = ((hi[0] + margin) / g).ceil() as i64;
    let iy0 = ((lo[1] - margin) / g).floor() as i64;
    let iy1 = ((hi[1] + margin) / g).ceil() as i64;
    let near = sep * 1.02;
    let far = sep * 2.15;
    for iy in iy0..=iy1 {
        for ix in ix0..=ix1 {
            let c = lattice(ix, iy, g, seed);
            grid.query(c, far, &mut scratch);
            if scratch.is_empty() {
                continue; // too far from town: open country, no cell needed
            }
            let mut nearest = f64::INFINITY;
            for &i in &scratch {
                nearest = nearest.min(dist(c, real[i as usize]));
            }
            if nearest >= near && nearest <= far {
                out.insert((ix, iy));
            }
        }
    }
    out
}

/// Build the territory of every real plot.
pub fn build(real: &[P], sep: f64, seed: u64) -> Cells {
    let g = sep * 0.80;
    let keys = phantom_ring(real, sep, seed);
    let which: Vec<usize> = (0..real.len()).collect();
    let mut cells = Cells {
        cells: vec![Vec::new(); real.len()],
        phantoms: keys.iter().map(|&(a, b)| lattice(a, b, g, seed)).collect(),
        phantom_keys: keys,
        g,
        seed,
    };
    compute_cells(real, sep, &mut cells, &which);
    cells
}

/// Re-test the lattice around a newly settled plot and return every phantom
/// position that appeared or vanished. Only this window can change.
pub fn update_phantoms(cells: &mut Cells, real: &[P], at: P, sep: f64) -> Vec<P> {
    let g = cells.g;
    let seed = cells.seed;
    let win = sep * 4.0;
    let ix0 = ((at[0] - win) / g).floor() as i64;
    let ix1 = ((at[0] + win) / g).ceil() as i64;
    let iy0 = ((at[1] - win) / g).floor() as i64;
    let iy1 = ((at[1] + win) / g).ceil() as i64;
    let grid = SiteGrid::build(real.to_vec(), sep * 1.4);
    let near = sep * 1.02;
    let far = sep * 2.15;
    let mut scratch = Vec::new();
    let mut changed = Vec::new();
    let mut dirty = false;
    for iy in iy0..=iy1 {
        for ix in ix0..=ix1 {
            let c = lattice(ix, iy, g, seed);
            grid.query(c, far, &mut scratch);
            let mut nearest = f64::INFINITY;
            for &i in &scratch {
                nearest = nearest.min(dist(c, real[i as usize]));
            }
            let want = nearest.is_finite() && nearest >= near && nearest <= far;
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
            .map(|&(a, b)| lattice(a, b, g, seed))
            .collect();
    }
    changed
}

/// Recompute only the listed plots' territories, reusing the rest.
///
/// A plot's cell depends only on plots closer than twice its own cell radius,
/// so inserting one plot can only disturb its immediate neighbourhood. This is
/// the whole incremental story: one growth step touches a handful of cells.
pub fn rebuild_subset(real: &[P], sep: f64, cells: &mut Cells, which: &[usize]) {
    cells.cells.resize(real.len(), Vec::new());
    compute_cells(real, sep, cells, which);
}

/// Which real plots can possibly change when the sites in `moved` appear.
pub fn affected(real: &[P], moved: &[P], sep: f64) -> Vec<usize> {
    let r = sep * 3.6;
    let r2 = r * r;
    let mut out = Vec::new();
    for (i, p) in real.iter().enumerate() {
        if moved.iter().any(|m| dist2(*p, *m) <= r2) {
            out.push(i);
        }
    }
    out
}

fn compute_cells(real: &[P], sep: f64, cells: &mut Cells, which: &[usize]) {
    let n_real = real.len();
    let mut all: Vec<P> = Vec::with_capacity(n_real + cells.phantoms.len());
    all.extend_from_slice(real);
    all.extend_from_slice(&cells.phantoms);
    let grid = SiteGrid::build(all.clone(), sep * 1.4);

    // A fixed frame, not the settlement's bounding box: a box that grows with
    // the town would perturb every boundary cell on every growth step.
    const FAR: f64 = 4096.0;
    let frame = vec![
        [-FAR, -FAR],
        [FAR, -FAR],
        [FAR, FAR],
        [-FAR, FAR],
    ];

    let mut scratch = Vec::new();
    for &i in which {
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
                // Perpendicular bisector: dot(unit(d), x) <= dot(unit(d), mid).
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
            // A site farther than 2 * rmax cannot cut this cell.
            let rmax = ring.iter().map(|p| dist(*p, s)).fold(0.0f64, f64::max);
            if 2.0 * rmax <= radius + 1e-9 {
                break;
            }
            radius = 2.0 * rmax + sep;
            if radius > sep * 60.0 {
                break;
            }
        }
        let mut ring: Vec<P> = ring.into_iter().map(qp).collect();
        ring = dedupe_ring(ring, 1e-6);
        if signed_area2(&ring) < 0.0 {
            ring.reverse();
        }
        cells.cells[i] = ring;
    }
}

/// Quantise to the weld lattice used to identify shared cell corners. This is
/// PRD §7.2's snap, applied where it actually bridges: two neighbouring cells
/// compute the same corner from the same bisector equations but by different
/// clip orders, and coarsening the lattice deliberately merges *near*-coincident
/// corners into one four- or five-way junction.
#[inline]
pub fn weld_key(p: P, tol: f64) -> (i64, i64) {
    (
        (quantize_f64(p[0]) / tol).round() as i64,
        (quantize_f64(p[1]) / tol).round() as i64,
    )
}
