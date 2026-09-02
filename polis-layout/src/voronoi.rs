//! Stage 1 — the road *substrate*.
//!
//! > **The road network is the boundary network of the settled ground.** A road
//! > is the line where one parcel's territory stops and the next one's begins.
//!
//! That is a Voronoi diagram of the plot positions, and it is planar, connected
//! and full of closed faces **by construction** rather than by tuning a snap
//! radius. Every cell contributes one independent cycle; there is no parameter
//! setting at which it degenerates into a tree, which is exactly the failure
//! mode PRD §7.2 warns about and the previous M1 attempt hit.
//!
//! # Half-plane clipping, not a triangulation
//!
//! A cell is built by starting from the city limit and clipping by the
//! perpendicular bisector with every neighbour inside a query radius, widening
//! the radius until it provably covers the cell (`2 · rmax ≤ radius`). No
//! degenerate predicates, no insertion-order sensitivity, and the neighbour set
//! is exact rather than heuristic.
//!
//! **This is the code that must stay `f64`** (ADR-0053). See [`crate::geom`].
//!
//! # The city limit is the frame, and there are no phantom sites
//!
//! The design this port is based on bounded its outer cells with a ring of
//! *phantom* plots laid wherever the ground was empty, and discarded their
//! cells. That has two costs, both of which showed up on measurement. Empty
//! ground **inside** the settlement also grows phantoms, so a gap between two
//! quarters becomes a hole in the map and, if the gap is wide enough, splits the
//! road graph — eleven components at 5 000 files, where the whole design turns
//! on there being one. And the outer boundary follows wherever the growth
//! happened to reach, which is the ragged fringe and the tentacles the bake-off
//! called out.
//!
//! Clipping every cell to [`crate::territory::Territory::rim`] instead costs
//! nothing and gives both properties for free: every cell is bounded, the union
//! of the cells **is** the city limit, so the real cells are mutually adjacent
//! and `components = 1` is structural rather than measured; and the drawn edge
//! of town is the drawn city limit. Unsettled ground inside the limit becomes a
//! larger cell rather than a hole, which is what open country on the edge of a
//! town actually looks like.
//!
//! # Quarters: where an avenue comes from
//!
//! A plain Voronoi diagram of accreted plots has no straight line in it. Every
//! boundary is the bisector of two irregularly-placed sites, so it bends within
//! a cell or two, and the map reads as soap foam — the bake-off's second named
//! defect, and the reason `junctions-large.png` has no long line anywhere.
//!
//! Graft 2 asks for through-streets by *seeding*: pull the plots near a district
//! border onto a lattice aligned to it, and a run of collinearly-seeded sites
//! gives a run of collinear boundaries. That was built and measured first, and
//! the measurement is why this module looks the way it does. A boundary segment
//! on the line needs a **facing pair** — two plots mirrored across it — and the
//! two sides of a cut are different districts, settled at different times, so a
//! run needs the second district to choose exactly the rungs the first one used,
//! over and over. The share of lattice slots that can be filled at all is
//! `pitch² × plot density`, which at 5 000 files is about a quarter; the best
//! measured run of consecutive facing pairs, with the lattice slots reserved
//! from before the first file and a bonus for completing a pair, was **five**,
//! against the eighteen cells a 46 %-of-diameter avenue spans. Seeding alone
//! cannot get there at this plot density, and the sweep says so at every pitch
//! from 1.0 to 4.0 × `sep`.
//!
//! So the avenue is made structural instead, in the one way that does not draw a
//! road: the diagram is computed **per quarter**. The partition promotes a few
//! of its cuts to avenues ([`crate::territory`]) and those cuts divide the city
//! limit into convex *quarters*; a plot's cell starts from its own quarter's
//! polygon and is clipped only against plots of the same quarter. Each quarter
//! is therefore tiled exactly by its own cells, the quarters tile the city
//! limit, and the shared edge between two quarters is the chord itself — one
//! exactly straight road, for its whole length, with no coordination between the
//! two sides at all.
//!
//! This is *not* treemap's chord-splitting. Treemap made every cut at every
//! depth a road, which is the crazed-glaze artefact, and added city-spanning
//! arterials on top. Here a cut is promoted only when it is between a quarter
//! and a half of the city diameter — long enough to read as a through-street,
//! never long enough to cross the city — there are at most
//! [`crate::territory::MAX_AVENUES`] of them against some three hundred cuts,
//! and every other road in the city is still the bisector of two accreted plots.
//!
//! The lattice seeding is kept, because it is what makes the frontage along an
//! avenue regular and the cross streets meet it squarely. It is no longer what
//! makes the avenue straight.

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

use crate::determinism::quantize_f64;
use crate::geom::{
    add, clip_halfplane, dedupe_ring, dist, dist2, dot, len, mul, qp, signed_area2, sub, Pt,
};

/// Radius, in units of `sep`, within which a new plot can disturb a cell.
const AFFECT_RADIUS: f64 = 3.6;

/// The territories of the settled plots.
// `cells.cells` reads oddly and is the honest name for it: the struct is the
// diagram, the field is the rings.
#[allow(clippy::struct_field_names)]
#[derive(Debug, Clone, Default)]
pub(crate) struct Cells {
    /// One ring per plot, counter-clockwise, quantised. Empty only when the
    /// plot coincides with another, which the separation rule prevents.
    pub(crate) cells: Vec<Vec<Pt>>,
    /// The convex frame each quarter's cells start from. One entry; the whole
    /// city limit when the partition promoted no avenue at all.
    frames: Vec<Vec<Pt>>,
    /// Which quarter each plot's cell is computed in.
    quarter: Vec<u32>,
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
}

/// Build the territory of every plot, each inside its own quarter.
///
/// `frames` are the quarter polygons and `quarter[i]` is the quarter plot `i`
/// belongs to. Passing a single frame and all-zero quarters is the plain
/// city-limit-clipped diagram.
pub(crate) fn build(real: &[Pt], frames: &[Vec<Pt>], quarter: &[u32], sep: f64) -> Cells {
    let mut cells = Cells {
        cells: vec![Vec::new(); real.len()],
        frames: frames.to_vec(),
        quarter: quarter.to_vec(),
    };
    cells.quarter.resize(real.len(), 0);
    let which: Vec<usize> = (0..real.len()).collect();
    compute_cells(real, sep, &mut cells, &which);
    cells
}

/// Recompute only the listed plots' territories, reusing the rest.
///
/// `quarter` must cover every plot: a new plot's quarter is decided by the
/// caller (by point location, [`crate::city`]), never inferred here.
pub(crate) fn rebuild_subset(
    real: &[Pt],
    quarter: &[u32],
    sep: f64,
    cells: &mut Cells,
    which: &[usize],
) {
    cells.cells.resize(real.len(), Vec::new());
    cells.quarter = quarter.to_vec();
    cells.quarter.resize(real.len(), 0);
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
    let grid = SiteGrid::build(real.to_vec(), sep * 1.4);
    let frames = cells.frames.clone();
    let quarter = cells.quarter.clone();
    let mut scratch = Vec::new();
    for &i in which {
        let s = real[i];
        let q = quarter.get(i).copied().unwrap_or(0) as usize;
        let Some(frame) = frames.get(q) else {
            cells.cells[i].clear();
            continue;
        };
        let frame = frame.clone();
        let mut radius = sep * 3.2;
        let mut ring;
        loop {
            grid.query(s, radius, &mut scratch);
            ring = frame.clone();
            for &j in &scratch {
                if j as usize == i {
                    continue;
                }
                // Only against the plots of the same quarter: an avenue is where
                // one quarter's ground stops, so a plot on the far side of it
                // must not cut this cell.
                if quarter.get(j as usize).copied().unwrap_or(0) as usize != q {
                    continue;
                }
                let o = real[j as usize];
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

    /// The one-quarter diagram: every site clipped to the same frame.
    fn plain(sites: &[Pt], f: &[Pt]) -> Cells {
        build(
            sites,
            std::slice::from_ref(&f.to_vec()),
            &vec![0; sites.len()],
            1.0,
        )
    }

    fn frame(r: f64) -> Vec<Pt> {
        vec![[-r, -r], [r, -r], [r, r], [-r, r]]
    }

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
        let cells = plain(&sites, &frame(40.0));
        assert_eq!(cells.cells.len(), sites.len());
        for (i, ring) in cells.cells.iter().enumerate() {
            assert!(ring.len() >= 3, "cell {i} is degenerate");
            assert!(area(ring) > 0.0, "cell {i} has no area");
            assert!(contains(ring, sites[i]), "cell {i} lost its own site");
        }
    }

    #[test]
    fn the_cells_tile_the_frame_exactly() {
        let sites = lattice_sites(5, 1.0);
        let f = frame(9.0);
        let cells = plain(&sites, &f);
        let total: f64 = cells.cells.iter().map(|r| area(r)).sum();
        assert!(
            (total - area(&f)).abs() < area(&f) * 1e-9,
            "cells sum to {total}, the city limit is {}",
            area(&f)
        );
        // A site is inside its own cell and no other's.
        for (i, s) in sites.iter().enumerate() {
            for (j, ring) in cells.cells.iter().enumerate() {
                if i != j {
                    assert!(!contains(ring, *s), "site {i} is inside cell {j}");
                }
            }
        }
    }

    #[test]
    fn no_cell_escapes_the_city_limit() {
        let sites = lattice_sites(4, 1.0);
        let cells = plain(&sites, &frame(6.0));
        for ring in &cells.cells {
            for v in ring {
                assert!(
                    v[0] >= -6.001 && v[0] <= 6.001 && v[1] >= -6.001 && v[1] <= 6.001,
                    "a cell vertex left the city limit at {v:?}"
                );
            }
        }
    }

    #[test]
    fn a_rebuilt_subset_matches_a_full_rebuild() {
        let sites = lattice_sites(5, 1.0);
        let f = frame(40.0);
        let full = plain(&sites, &f);
        let mut partial = plain(&sites, &f);
        let which: Vec<usize> = (0..sites.len()).step_by(3).collect();
        rebuild_subset(&sites, &vec![0; sites.len()], 1.0, &mut partial, &which);
        for i in which {
            assert_eq!(full.cells[i], partial.cells[i], "cell {i} moved");
        }
    }

    #[test]
    fn a_new_plot_disturbs_only_its_neighbourhood() {
        let mut sites = lattice_sites(7, 1.0);
        let f = frame(40.0);
        let before = plain(&sites, &f);
        let at = qp([3.5, 3.5]);
        sites.push(at);
        let mut after = before.clone();
        let which = affected(&sites, &[at], 1.0);
        rebuild_subset(&sites, &vec![0; sites.len()], 1.0, &mut after, &which);
        let reference = plain(&sites, &f);
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

    /// A quarter boundary is a straight road because the cells stop at it.
    /// Two properties have to hold together: every cell stays on its own side,
    /// and the two sides still tile the frame between them.
    #[test]
    fn cells_stop_at_a_quarter_boundary() {
        let f = frame(4.0);
        let left = vec![[-4.0, -4.0], [0.0, -4.0], [0.0, 4.0], [-4.0, 4.0]];
        let right = vec![[0.0, -4.0], [4.0, -4.0], [4.0, 4.0], [0.0, 4.0]];
        // Two sites either side, deliberately *not* mirror images: a plain
        // diagram would put a tilted bisector between them.
        let sites = vec![qp([-1.0, -2.0]), qp([-1.5, 2.0]), qp([1.0, 0.4])];
        let cells = build(&sites, &[left.clone(), right.clone()], &[0, 0, 1], 1.0);
        let total: f64 = cells.cells.iter().map(|r| area(r)).sum();
        assert!(
            (total - area(&f)).abs() < area(&f) * 1e-9,
            "the cells sum to {total} against a frame of {}",
            area(&f)
        );
        for (i, ring) in cells.cells.iter().enumerate() {
            for v in ring {
                let inside = if i < 2 { v[0] <= 1e-9 } else { v[0] >= -1e-9 };
                assert!(inside, "cell {i} crossed the quarter boundary at {v:?}");
            }
        }
    }

    #[test]
    fn welding_merges_near_coincident_corners() {
        assert_eq!(weld_key([1.0, 1.0], 0.004), weld_key([1.001, 1.0], 0.004));
        assert_ne!(weld_key([1.0, 1.0], 0.004), weld_key([1.02, 1.0], 0.004));
    }
}
