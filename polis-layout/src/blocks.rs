//! Stage 3 — blocks (PRD §7.2 step 3).
//!
//! > **Blocks** are the closed loops (faces) in the resulting planar road graph.
//!
//! They are recovered honestly, by an actual half-edge face traversal of the
//! planar embedding (`roads::Graph::faces`) — **not** by assuming one
//! block per plot. The two counts genuinely differ: pruning merges parcels, so a
//! city of 1 200 plots comes out with rather fewer faces, and a pipeline that
//! assumed otherwise would be silently wrong in a way no test could see.
//!
//! This module is thin on purpose. The face walk lives with the graph; what is
//! left here is deciding **which district a block belongs to** and turning the
//! `f64` face into the public [`Block`].
//!
//! # Districts tile
//!
//! A block's district is the majority district of the plots whose territory it
//! covers. Because a plot may only settle inside its own district's polygon
//! (the `territory` partition) and the polygons are disjoint, that majority is
//! usually unanimous, and the district masses come out as contiguous blobs that
//! tile the map — which is what PRD §8 needs the wayfinding layer to be.

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

use polis_events::LogicalPath;

use crate::accrete::Settlement;
use crate::geom::{area, bounds, centroid, contains, dist, dist_to_boundary, to_polygon, Pt};
use crate::roads::Face;
use crate::{Block, BlockId};

/// A face smaller than this fraction of the median is a sliver.
///
/// The threshold the design bake-off measured against, kept so the numbers stay
/// comparable.
pub const SLIVER_AREA_FRACTION: f64 = 0.06;

/// A face longer than this, relative to its width, is a sliver.
pub const SLIVER_ASPECT: f64 = 6.0;

/// The district a file belongs to: its parent directory (PRD §3, §9).
///
/// > A **District** is a directory […] the tree determines placement, because
/// > directory paths are the addressing system already in use in every tool
/// > call, error message, and conversation.
#[must_use]
pub fn district_of(path: &LogicalPath) -> LogicalPath {
    path.parent().unwrap_or_else(LogicalPath::root)
}

/// One block, in the pipeline's internal `f64` form.
#[derive(Debug, Clone)]
pub(crate) struct BlockPlan {
    /// The ring, counter-clockwise.
    pub(crate) ring: Vec<Pt>,
    /// Territory node index of the district that owns it, if any.
    pub(crate) district: Option<u32>,
    /// Plots whose position falls inside it.
    pub(crate) plots: Vec<u32>,
    /// No plot inside: a square, a green, or a scrap of undeveloped ground.
    pub(crate) open: bool,
    /// `node_modules`, `vendor`, `target`: drawn as one dull mass rather than as
    /// individual buildings, because the eye should slide off it (PRD §8).
    pub(crate) industrial: bool,
    /// Half-edges of the face, for the district-border layer.
    pub(crate) half_edges: Vec<usize>,
    /// Growth step at which the oldest plot inside it was settled.
    pub(crate) birth: u32,
}

impl BlockPlan {
    /// Unsigned area.
    pub(crate) fn area(&self) -> f64 {
        area(&self.ring)
    }
}

/// A uniform grid over block bounding boxes, for point location.
struct BlockIndex {
    buckets: BTreeMap<(i32, i32), Vec<u32>>,
    cell: f64,
    origin: Pt,
}

impl BlockIndex {
    fn build(blocks: &[BlockPlan]) -> Self {
        let mut lo = [f64::INFINITY, f64::INFINITY];
        let mut hi = [f64::NEG_INFINITY, f64::NEG_INFINITY];
        let mut span = 0.0;
        for b in blocks {
            let (l, h) = bounds(&b.ring);
            lo[0] = lo[0].min(l[0]);
            lo[1] = lo[1].min(l[1]);
            hi[0] = hi[0].max(h[0]);
            hi[1] = hi[1].max(h[1]);
            span += (h[0] - l[0]).max(h[1] - l[1]);
        }
        if !lo[0].is_finite() {
            lo = [0.0, 0.0];
        }
        let cell = (span / blocks.len().max(1) as f64).max(1e-3);
        let mut buckets: BTreeMap<(i32, i32), Vec<u32>> = BTreeMap::new();
        for (i, b) in blocks.iter().enumerate() {
            let id = u32::try_from(i).expect("block count fits in u32");
            let (l, h) = bounds(&b.ring);
            let x0 = ((l[0] - lo[0]) / cell).floor() as i32;
            let x1 = ((h[0] - lo[0]) / cell).floor() as i32;
            let y0 = ((l[1] - lo[1]) / cell).floor() as i32;
            let y1 = ((h[1] - lo[1]) / cell).floor() as i32;
            for y in y0..=y1 {
                for x in x0..=x1 {
                    buckets.entry((x, y)).or_default().push(id);
                }
            }
        }
        Self {
            buckets,
            cell,
            origin: lo,
        }
    }

    /// The block containing `p`, lowest index first.
    ///
    /// Falls back to the nearest block when no ring contains the point. A plot
    /// whose cell was pruned away still has files, and dropping it would lose
    /// their buildings silently — which is the one outcome the accounting in
    /// [`crate::lots::LotReport`] exists to make impossible.
    fn locate(&self, blocks: &[BlockPlan], p: Pt) -> Option<u32> {
        let kx = ((p[0] - self.origin[0]) / self.cell).floor() as i32;
        let ky = ((p[1] - self.origin[1]) / self.cell).floor() as i32;
        let mut best: Option<u32> = None;
        for dy in -2..=2 {
            for dx in -2..=2 {
                if let Some(list) = self.buckets.get(&(kx + dx, ky + dy)) {
                    for &bi in list {
                        if contains(&blocks[bi as usize].ring, p) && best.is_none_or(|b| bi < b) {
                            best = Some(bi);
                        }
                    }
                }
            }
        }
        if best.is_some() {
            return best;
        }
        let mut nearest: Option<(f64, u32)> = None;
        for (i, b) in blocks.iter().enumerate() {
            if b.ring.len() < 3 {
                continue;
            }
            let id = u32::try_from(i).expect("block count fits in u32");
            let d = crate::geom::dist(centroid(&b.ring), p);
            if nearest.is_none_or(|(bd, bi)| d < bd || (d == bd && id < bi)) {
                nearest = Some((d, id));
            }
        }
        nearest.map(|(_, i)| i)
    }
}

/// The majority district of the plots inside each face, before pruning.
///
/// Needed *before* blocks exist, because pruning must not merge across a
/// district border: a merged block takes one district's ground and gives it to
/// the neighbour, which is how a contiguous district becomes two pieces.
pub(crate) fn face_districts(faces: &[Face], s: &Settlement) -> Vec<Option<u32>> {
    let plans: Vec<BlockPlan> = faces
        .iter()
        .map(|f| BlockPlan {
            ring: f.ring.clone(),
            district: None,
            plots: Vec::new(),
            open: true,
            industrial: false,
            half_edges: Vec::new(),
            birth: u32::MAX,
        })
        .collect();
    let index = BlockIndex::build(&plans);
    let mut tally: Vec<BTreeMap<u32, u32>> = vec![BTreeMap::new(); faces.len()];
    for plot in &s.plots {
        if let Some(bi) = index.locate(&plans, plot.pos) {
            *tally[bi as usize].entry(plot.district).or_insert(0) += 1;
        }
    }
    tally
        .into_iter()
        .map(|t| {
            t.iter()
                .max_by_key(|(d, c)| (**c, std::cmp::Reverse(**d)))
                .map(|(d, _)| *d)
        })
        .collect()
}

/// Which edges have a different district on each side.
pub(crate) fn border_edges(faces: &[Face], districts: &[Option<u32>], edges: usize) -> Vec<bool> {
    let mut side: Vec<[i64; 2]> = vec![[-1, -1]; edges];
    for (fi, f) in faces.iter().enumerate() {
        let d = districts[fi].map_or(-2, i64::from);
        for &h in &f.half_edges {
            let e = h / 2;
            if e < side.len() {
                side[e][h % 2] = d;
            }
        }
    }
    side.iter().map(|[a, b]| a != b).collect()
}

/// Turn the road graph's faces into blocks and give each one a district.
///
/// Returns the blocks and, for each plot, the block it landed in.
pub(crate) fn assign(
    faces: Vec<Face>,
    s: &Settlement,
) -> (Vec<BlockPlan>, Vec<Option<u32>>, usize) {
    let mut blocks: Vec<BlockPlan> = faces
        .into_iter()
        .map(|f| BlockPlan {
            ring: f.ring,
            district: None,
            plots: Vec::new(),
            open: true,
            industrial: false,
            half_edges: f.half_edges,
            birth: u32::MAX,
        })
        .collect();

    let index = BlockIndex::build(&blocks);
    let mut plot_block: Vec<Option<u32>> = vec![None; s.plots.len()];
    // Plots that landed in no face at all and had to take
    // [`BlockIndex::locate`]'s nearest-centroid fallback. **Counted**, because
    // the fallback is unbounded: on a repository with thousands of two-file
    // directories it silently piled five hundred files into one sliver, every
    // one of them reported as `placed` with a lot it could not fit on. A number
    // in the report is the difference between a bug that shows and one that
    // hides behind an accurate-looking total.
    let mut off_face = 0usize;
    for (pi, plot) in s.plots.iter().enumerate() {
        if let Some(bi) = index.locate(&blocks, plot.pos) {
            if !contains(&blocks[bi as usize].ring, plot.pos) {
                off_face += 1;
            }
            plot_block[pi] = Some(bi);
            blocks[bi as usize]
                .plots
                .push(u32::try_from(pi).expect("plot count fits in u32"));
        }
    }

    // The per-face tallies are kept, not consumed: `seat_unseated_districts`
    // below needs to know how strongly a district voted on a face it lost.
    let mut tallies: Vec<BTreeMap<u32, i64>> = vec![BTreeMap::new(); blocks.len()];
    for (bi, b) in blocks.iter_mut().enumerate() {
        if b.plots.is_empty() {
            continue;
        }
        b.open = false;
        b.birth = b
            .plots
            .iter()
            .map(|&pi| s.plots[pi as usize].birth)
            .min()
            .unwrap_or(u32::MAX);
        // Whose face is this? A **depth-weighted** vote, not a head count.
        //
        // A plot votes with how far inside the face it sits. Its own cell's
        // face has it well inside; a plot that ended up in a *neighbour's* face
        // — because the collapse moved a boundary a fraction of a separation
        // past it — sits right against the edge and votes with almost nothing.
        //
        // A head count cannot tell those apart, and on a two-plot face it lets
        // the stray win the tie outright. This is a tie-break with a reason
        // rather than a fix for a measured failure — the district fragmentation
        // it was first written against turned out to come from `regions` and is
        // fixed there — but it is the right rule either way: a face belongs to
        // the cell that made it.
        let mut tally: BTreeMap<u32, i64> = BTreeMap::new();
        for &pi in &b.plots {
            let depth = dist_to_boundary(&b.ring, s.plots[pi as usize].pos).max(0.0);
            let weight = (crate::determinism::quantize_f64(depth) * 1_000.0).round() as i64;
            *tally.entry(s.plots[pi as usize].district).or_insert(0) += weight.max(1);
        }
        // Ties to the lowest territory index — creation order, which is a
        // property of the directory tree and not of iteration.
        b.district = tally
            .iter()
            .max_by_key(|(d, c)| (**c, std::cmp::Reverse(**d)))
            .map(|(d, _)| *d);
        tallies[bi] = tally;
    }

    seat_unseated_districts(&mut blocks, &tallies, s);

    // Industrial is read off the district, so it is settled after the seats are.
    for b in &mut blocks {
        if b.plots.is_empty() {
            continue;
        }
        let industrial_plots = b
            .plots
            .iter()
            .filter(|&&pi| {
                s.plots[pi as usize]
                    .files
                    .iter()
                    .any(|&fi| s.files[fi as usize].industrial)
            })
            .count();
        b.industrial = industrial_plots * 2 > b.plots.len()
            || b.district
                .is_some_and(|d| s.territory.nodes[d as usize].industrial);
    }
    (blocks, plot_block, off_face)
}

/// A district that settled ground must appear on the ground.
///
/// The vote in [`assign`] is taken one face at a time, so a district every one
/// of whose faces also holds a deeper-voting neighbour can win *none* of them.
/// [`crate::regions`] cannot prevent it — it guarantees the district a connected
/// set of **plots**, and this is a fact about **faces**, which the collapse and
/// the prune have moved since.
///
/// It is rare and it was invisible until the age ramp began seating 999 plots on
/// the 5 000-file corpus where it had seated 955: two districts of 312 lost
/// their ground and eight files became buildings standing in a stranger's
/// quarter, which PRD §8 draws as a directory that is simply not on the map.
///
/// The repair is the same rule run once more with the winners fixed: each
/// unseated district takes the one face where its own plots voted highest, and
/// only from an owner that keeps at least one other face. That second clause is
/// what makes this terminate and what stops it unseating someone in turn — the
/// number of seated districts strictly rises and no district is ever emptied.
///
/// Order is by territory index over a `BTreeMap`, never by iteration (PRD §7.4).
fn seat_unseated_districts(
    blocks: &mut [BlockPlan],
    tallies: &[BTreeMap<u32, i64>],
    s: &Settlement,
) {
    // How many faces each district holds, and which districts have plots.
    let mut seats: BTreeMap<u32, usize> = BTreeMap::new();
    for b in blocks.iter() {
        if let Some(d) = b.district {
            *seats.entry(d).or_insert(0) += 1;
        }
    }
    let mut wanting: BTreeSet<u32> = BTreeSet::new();
    for p in &s.plots {
        if !seats.contains_key(&p.district) {
            wanting.insert(p.district);
        }
    }
    for &d in &wanting {
        // The face where this district's own plots sit deepest, among those
        // whose current owner can spare it.
        let mut best: Option<(i64, usize)> = None;
        for (bi, t) in tallies.iter().enumerate() {
            let Some(&mine) = t.get(&d) else { continue };
            let Some(owner) = blocks[bi].district else {
                continue;
            };
            if owner == d || seats.get(&owner).copied().unwrap_or(0) < 2 {
                continue;
            }
            // Ties to the lowest block index, which is a face of the road graph
            // and so a property of the geometry rather than of this loop.
            if best.is_none_or(|(w, _)| mine > w) {
                best = Some((mine, bi));
            }
        }
        if let Some((_, bi)) = best {
            if let Some(owner) = blocks[bi].district {
                *seats.entry(owner).or_insert(1) -= 1;
            }
            blocks[bi].district = Some(d);
            seats.insert(d, 1);
        }
    }
}

/// Put every district that came out in more than one piece back together, on
/// the **blocks** rather than on the plots.
///
/// [`crate::regions`] guarantees that a district's *plots* are one connected
/// part of the plot adjacency graph, and that is where the guarantee belongs —
/// but it is not quite the thing the map shows. A block is a face of the road
/// graph after the collapse and the prune, and the collapse moves boundaries: a
/// plot can end up a hair inside its neighbour's face, take that face's label
/// with it if the face is small enough, and cut the district next door in two.
/// Measured on a 200-file fixture, that was one district of thirty-two.
///
/// So the last word on district shape is taken here, on the object the metric
/// reads. A district keeps the piece holding its oldest block — the old town
/// stays where it was — and each stray piece is given to the neighbour that
/// already borders it most, preferring one in its own top-level package,
/// because a stray handed to a cousin heals one district and splits the
/// directory above it.
///
/// Giving a stray away cannot break the receiving district: the stray borders
/// it, so the union stays connected. Each pass therefore strictly reduces the
/// number of pieces.
pub(crate) fn heal_districts(blocks: &mut [BlockPlan], territory: &crate::territory::Territory) {
    let depth = territory.depths();
    let n = blocks.len();
    if n == 0 {
        return;
    }
    // Which two faces each road edge separates.
    let mut sides: BTreeMap<usize, Vec<u32>> = BTreeMap::new();
    for (i, b) in blocks.iter().enumerate() {
        for &h in &b.half_edges {
            sides
                .entry(h / 2)
                .or_default()
                .push(u32::try_from(i).expect("block count fits in u32"));
        }
    }
    let mut adj: Vec<Vec<u32>> = vec![Vec::new(); n];
    for list in sides.values() {
        if list.len() != 2 {
            continue;
        }
        adj[list[0] as usize].push(list[1]);
        adj[list[1] as usize].push(list[0]);
    }
    for list in &mut adj {
        list.sort_unstable();
        list.dedup();
    }

    for _pass in 0..HEAL_PASSES {
        let mut moved = false;
        let mut groups: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for (i, b) in blocks.iter().enumerate() {
            if let Some(d) = b.district {
                groups
                    .entry(d)
                    .or_default()
                    .push(u32::try_from(i).expect("fits"));
            }
        }
        for (d, members) in groups {
            if members.len() < 2 {
                continue;
            }
            let inside: BTreeSet<u32> = members.iter().copied().collect();
            let mut seen: BTreeSet<u32> = BTreeSet::new();
            let mut comps: Vec<Vec<u32>> = Vec::new();
            for &start in &members {
                if !seen.insert(start) {
                    continue;
                }
                let mut comp = vec![start];
                let mut stack = vec![start];
                while let Some(u) = stack.pop() {
                    for &v in &adj[u as usize] {
                        if inside.contains(&v) && seen.insert(v) {
                            comp.push(v);
                            stack.push(v);
                        }
                    }
                }
                comps.push(comp);
            }
            if comps.len() < 2 {
                continue;
            }
            let age = |c: &Vec<u32>| {
                c.iter()
                    .map(|&b| (blocks[b as usize].birth, b))
                    .min()
                    .unwrap_or((u32::MAX, u32::MAX))
            };
            let keep = (0..comps.len())
                .min_by_key(|&i| age(&comps[i]))
                .unwrap_or(0);
            for (i, comp) in comps.iter().enumerate() {
                if i == keep {
                    continue;
                }
                let mut tally: BTreeMap<u32, usize> = BTreeMap::new();
                for &b in comp {
                    for &v in &adj[b as usize] {
                        let Some(e) = blocks[v as usize].district else {
                            continue;
                        };
                        if e != d {
                            *tally.entry(e).or_insert(0) += 1;
                        }
                    }
                }
                // The **nearest relative** that borders the stray: the
                // neighbour sharing the deepest directory with it. That keeps
                // every subtree in one piece and not only the district and the
                // package — a stray handed to a cousin heals a district and
                // splits the directory above it.
                let Some((&target, _)) = tally.iter().max_by_key(|(e, c)| {
                    (
                        crate::districts::common_depth(territory, &depth, d, **e),
                        **c,
                        std::cmp::Reverse(**e),
                    )
                }) else {
                    continue;
                };
                for &b in comp {
                    blocks[b as usize].district = Some(target);
                }
                moved = true;
            }
        }
        if !moved {
            break;
        }
    }
}

/// How many times [`heal_districts`] sweeps before it gives up.
const HEAL_PASSES: usize = 4;

/// PRD §8's civic square: open ground at the historic centre.
///
/// > **Civic square** — repo root / top-level config — a recognisable open space
/// > at the historic centre.
///
/// The centre of the map is the oldest ground, because it was settled first, so
/// "nearest the origin" is "at the historic centre" and needs no extra state.
pub(crate) fn civic_square(blocks: &[BlockPlan], s: &Settlement) -> Option<u32> {
    // The reserved plot is the first one settled, and it holds no files.
    let civic = s.plots.first().filter(|p| p.cap == 0)?;
    let mut best: Option<(f64, u32)> = None;
    for (i, b) in blocks.iter().enumerate() {
        let id = u32::try_from(i).expect("fits");
        if contains(&b.ring, civic.pos) {
            return Some(id);
        }
        let d = dist(centroid(&b.ring), civic.pos);
        if best.is_none_or(|(bd, bi)| d < bd || (d == bd && id < bi)) {
            best = Some((d, id));
        }
    }
    best.map(|(_, i)| i)
}

/// Convert the internal blocks to the public [`Block`] list.
pub(crate) fn publish(
    blocks: &[BlockPlan],
    district_path: &dyn Fn(u32) -> LogicalPath,
) -> Vec<Block> {
    blocks
        .iter()
        .enumerate()
        .map(|(i, b)| Block {
            id: BlockId(u32::try_from(i).expect("block count fits in u32")),
            boundary: to_polygon(&b.ring),
            district: b.district.map_or_else(LogicalPath::root, district_path),
        })
        .collect()
}

/// Blocks of each district, in id order (PRD §8's district skeleton).
#[must_use]
pub fn group_by_district(blocks: &[Block]) -> BTreeMap<LogicalPath, Vec<BlockId>> {
    let mut out: BTreeMap<LogicalPath, Vec<BlockId>> = BTreeMap::new();
    for block in blocks {
        out.entry(block.district.clone())
            .or_default()
            .push(block.id);
    }
    for ids in out.values_mut() {
        ids.sort_unstable();
    }
    out
}

/// Drop vertices that sit on the straight line between their neighbours.
///
/// A welded Voronoi corner where two collinear boundaries meet is a real node of
/// the graph but not a corner of the block; leaving it in makes a golden file
/// noisier without describing anything.
#[must_use]
pub fn collapse_collinear(ring: &[Pt], tolerance: f64) -> Vec<Pt> {
    let n = ring.len();
    if n < 4 {
        return ring.to_vec();
    }
    let mut out: Vec<Pt> = Vec::with_capacity(n);
    for i in 0..n {
        let a = ring[(i + n - 1) % n];
        let b = ring[i];
        let c = ring[(i + 1) % n];
        let ab = crate::geom::sub(b, a);
        let bc = crate::geom::sub(c, b);
        let l = crate::geom::len(ab).max(crate::geom::len(bc));
        if l > 1e-12 && (crate::geom::cross(ab, bc) / l).abs() <= tolerance {
            continue;
        }
        out.push(b);
    }
    if out.len() < 3 {
        ring.to_vec()
    } else {
        out
    }
}

/// How many blocks are slivers: too thin, or too small next to the median.
#[must_use]
pub(crate) fn sliver_count(blocks: &[BlockPlan]) -> usize {
    if blocks.is_empty() {
        return 0;
    }
    let mut areas: Vec<f64> = blocks.iter().map(BlockPlan::area).collect();
    areas.sort_by(f64::total_cmp);
    let median = areas[areas.len() / 2].max(1e-12);
    blocks
        .iter()
        .filter(|b| {
            crate::geom::aspect_ratio(&b.ring) > SLIVER_ASPECT
                || b.area() < median * SLIVER_AREA_FRACTION
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lp(s: &str) -> LogicalPath {
        LogicalPath::new(s).expect("a valid test path")
    }

    #[test]
    fn a_district_is_the_parent_directory() {
        assert_eq!(district_of(&lp("src/auth/session.rs")), lp("src/auth"));
        assert_eq!(district_of(&lp("README.md")), LogicalPath::root());
    }

    #[test]
    fn collinear_vertices_are_dropped() {
        let ring: Vec<Pt> = vec![[0.0, 0.0], [1.0, 0.0], [2.0, 0.0], [2.0, 2.0], [0.0, 2.0]];
        let out = collapse_collinear(&ring, 1e-9);
        assert_eq!(out.len(), 4, "{out:?}");
        assert!((area(&out) - area(&ring)).abs() < 1e-9);
    }

    #[test]
    fn a_triangle_survives_collapsing() {
        let tri: Vec<Pt> = vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]];
        assert_eq!(collapse_collinear(&tri, 1e-9).len(), 3);
    }

    #[test]
    fn grouping_is_by_district_and_sorted() {
        let blocks = vec![
            Block {
                id: BlockId(2),
                boundary: crate::Polygon::default(),
                district: lp("src"),
            },
            Block {
                id: BlockId(0),
                boundary: crate::Polygon::default(),
                district: lp("src"),
            },
            Block {
                id: BlockId(1),
                boundary: crate::Polygon::default(),
                district: lp("docs"),
            },
        ];
        let grouped = group_by_district(&blocks);
        assert_eq!(grouped[&lp("src")], vec![BlockId(0), BlockId(2)]);
        assert_eq!(grouped[&lp("docs")], vec![BlockId(1)]);
        // A `BTreeMap`, so the districts come out in path order every time.
        let order: Vec<&str> = grouped.keys().map(LogicalPath::as_str).collect();
        assert_eq!(order, vec!["docs", "src"]);
    }

    #[test]
    fn a_slab_is_a_sliver_and_a_square_is_not() {
        let square = BlockPlan {
            ring: vec![[0.0, 0.0], [2.0, 0.0], [2.0, 2.0], [0.0, 2.0]],
            district: None,
            plots: Vec::new(),
            open: true,
            industrial: false,
            half_edges: Vec::new(),
            birth: 0,
        };
        let slab = BlockPlan {
            ring: vec![[0.0, 0.0], [20.0, 0.0], [20.0, 0.2], [0.0, 0.2]],
            ..square.clone()
        };
        assert_eq!(sliver_count(&[square.clone(), square.clone()]), 0);
        assert_eq!(sliver_count(&[square.clone(), square, slab]), 1);
    }
}
