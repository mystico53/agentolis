//! The district layer: the territory constraint, and the two rules that make a
//! district's ground **one piece** rather than confetti.
//!
//! # The problem this module exists to solve
//!
//! In a pure accretion layout, district membership is *emergent* — a district is
//! whatever fell out of where its plots happened to land. Measured on the design
//! bake-off's 5 000-file corpus that gave **102 of 276 districts more than one
//! disconnected piece**: in the dense core the colour changed every two or three
//! blocks, and the district masses were unreadable exactly where the city was
//! densest. That violates two binding requirements at once:
//!
//! * PRD §9 — "the tree determines placement, because directory paths are the
//!   addressing system already in use in every tool call, error message, and
//!   conversation". A district scattered across the map is not addressed by its
//!   path; it is addressed by an accident of growth order.
//! * PRD §8 — the district skeleton must stay readable at every zoom.
//!
//! # The two rules
//!
//! Everything here reduces to two invariants on where a plot may settle. They
//! are enforced in [`crate::accrete::Settlement::settle_position`]
//! unconditionally — not asserted in a debug build and hoped for in release —
//! and every departure from them is counted into [`crate::city::CityReport`], so
//! "the constraint binds" is a number in the report rather than a claim in a
//! comment.
//!
//! **T — territory.** A plot may only settle inside its own district's polygon
//! ([`crate::territory`], the partition grafted from `treemap-arterials`). The
//! polygons are convex, disjoint, and tile the city limit, and a district's
//! outer polygon is fixed by its parent — which is what upgrades PRD §7.7's
//! no-rearrangement property from "measured" to "structurally impossible to
//! violate", and what makes siblings share a border by construction.
//!
//! **A — adjacency.** After its first, every plot of a district must have one of
//! *its own* district's plots as its nearest plot in the whole city.
//!
//! # Why rule A is what actually guarantees contiguity
//!
//! Rule T alone is not enough, and it is worth being precise about why, because
//! the difference is two districts on a 5 000-file map.
//!
//! Blocks are faces of the Voronoi diagram of the settled plots, merged only
//! within a district (`roads::choose_prunes` never deletes a border edge), so a
//! district's blocks are the union of its plots' cells. Rule T puts every one of
//! those plots inside one convex polygon — but a convex polygon is not a
//! guarantee about cells: a neighbouring district's plot pressed against the
//! shared border can own a cell that reaches *through* the polygon and cuts the
//! district in two.
//!
//! Rule A closes that gap with a standard fact about Delaunay triangulations:
//! **the nearest-neighbour graph is a subgraph of the Delaunay graph**, so if
//! `q` is the nearest plot to `p`, then `p`'s cell and `q`'s cell share an edge.
//! Requiring each new plot to have a sibling as its nearest plot therefore adds
//! it to its district's cell-adjacency component. By induction over the growth
//! order, a district's cells are one edge-connected region, and merging cells
//! inside it cannot disconnect them. Contiguity stops being a measurement and
//! becomes a property of the construction.
//!
//! One thing rule A does not buy on its own: the shared Voronoi edge has to
//! survive the weld and the short-edge collapse that turn the diagram into a
//! road network. Contracting a boundary is the whole point of the collapse pass
//! — it is what makes a four-way junction out of two three-way corners — but the
//! two cells either side of a contracted boundary are left touching at a single
//! node, and if that boundary was the only thing joining a two-parcel district,
//! the district comes out in two pieces. Measured: exactly one district in 238
//! at 3 000 files, with both plots placed cleanly and nothing else near them.
//! [`links_to_keep`] marks a spanning forest of the cell-adjacency graph before
//! the collapse runs — ordered by the depth of two cells' lowest common
//! directory, so that it is a spanning forest of *every* subtree at once — and
//! those boundaries are never contracted. That is the last step of the chain,
//! and it is what makes the guarantee hold for `src/auth/` and for `src/` and
//! not only for a leaf directory.
//!
//! What is left is measured rather than assumed: [`fragmented`] runs on every
//! generated city, `presplit_districts` reports any district the diagram itself
//! never joined, and the M1 gate asserts both are zero at four scales and on
//! both fixture repositories.
//!
//! # What is *not* structural, and why the difference is worth stating
//!
//! A whole directory **subtree** being one mass is a stronger claim than a
//! district being one mass, and it is not guaranteed. Two sibling districts are
//! given faces that share a chord, but nothing forces a plot on one side of that
//! chord to be a Voronoi neighbour of a plot on the other, so a cousin's cell
//! can reach across it and leave the parent directory in two pieces even though
//! every district in it is whole. [`fragmented_subtrees`] measures exactly that;
//! it is 0, 1, 4 and 2 of 32, 144, 238 and 312 subtrees at 200, 1 000, 3 000 and
//! 5 000 files, and the gate bounds it rather than asserting zero.
//!
//! Extending rule A to founding plots — a district's first plot must touch the
//! nearest ancestor quarter that has ground — would close it by the same
//! induction, and it was tried. It is worse on measurement: at 1 000 files it
//! took fragmented districts from 0 to 1, packages from 0 to 1 and rule-A bends
//! from 0 to 18, because a founding plot is the one with the least room to
//! manoeuvre. ADR-0058 records it.
//!
//! # The hard cases
//!
//! * **A district too small for its file count.** Cannot arise from the split
//!   itself — the partition allocates area in proportion to quantised subtree
//!   weight, so a district's ground scales with its demand at every depth. It
//!   can still arise when the recursion is stopped early by
//!   `territory::MIN_FACE_PLOTS` or `MAX_DEPTH`, and those cases are counted as
//!   `shared_faces` rather than hidden. They are 0 on the 5 000-file corpus.
//! * **A district with one file.** [`crate::accrete::Params::district_demand`]
//!   rounds *up* to whole plots and adds a border band, so a one-file directory
//!   is given the ground one plot plus its clearance actually needs.
//! * **Deeply nested trees.** Each level cuts its parent's face, so depth costs
//!   area but never contiguity; `MAX_DEPTH` is 40 against a measured maximum
//!   nesting of 6.
//! * **The industrial zone (PRD §8).** `vendor`, `node_modules`, `target` are
//!   districts like any other, so the graft gives the whole subtree one
//!   contiguous polygon — which is exactly what PRD §8 asks for, one dull
//!   uniform mass the eye slides off, rather than the scatter of individual
//!   buildings an emergent assignment produced.

// Two of the pedantic lints fire here and neither is telling us anything:
//
// * `cast_precision_loss` — the one cast is a file count into the `[0, 1]`
//   growth-progress denominator, and a repository with 2^53 files is not the
//   failure mode to design against;
// * `float_cmp` — exact float comparison is how determinism is *tested* (PRD
//   §7.4). An approximate comparison in the order-independence test would be the
//   bug it is there to catch.
#![allow(clippy::cast_precision_loss, clippy::float_cmp)]

use std::collections::{BTreeMap, BTreeSet};

use polis_events::LogicalPath;

use crate::accrete::FileRec;
use crate::blocks::{district_of, BlockPlan};
use crate::territory::{Demand, Territory};

/// Ground each district asks the partition for, in path order.
///
/// The list is grouped and sorted here, so the order the caller happened to
/// collect the files in cannot reach the partition (PRD §7.4).
///
/// The count is what the tree is weighed by and the growth index is what it is
/// ordered by; neither depends on the order the files arrived in.
pub(crate) fn demands(files: &[FileRec]) -> Vec<(LogicalPath, Demand)> {
    let mut by_district: BTreeMap<LogicalPath, (usize, u32)> = BTreeMap::new();
    for f in files {
        let entry = by_district
            .entry(district_of(&f.path))
            .or_insert((0, u32::MAX));
        entry.0 += 1;
        entry.1 = entry.1.min(f.growth_index);
    }
    by_district
        .into_iter()
        .map(|(path, (count, oldest))| {
            (
                path,
                Demand {
                    oldest,
                    files: u32::try_from(count).unwrap_or(u32::MAX),
                },
            )
        })
        .collect()
}

/// Rule **A**: is a candidate position touching its own district's ground?
///
/// `nearest` is the distance to the closest settled plot of any district and
/// `nearest_same` the closest of the district's own, both measured over the same
/// search window — which is what makes the comparison sound: if a sibling is the
/// nearest plot inside the window, no plot outside the window can be nearer.
///
/// The tolerance admits an exact tie. Two plots equidistant from the candidate
/// are both Delaunay neighbours of it, so a tie is still an adjacency.
pub(crate) fn touches_own(nearest: f64, nearest_same: f64) -> bool {
    nearest_same.is_finite() && nearest_same <= nearest + 1e-9
}

/// The top-level package of every district, by territory index.
///
/// Colour is keyed on the package (the renderer's hue family), so a package in
/// two pieces is a package the eye cannot find — which makes this the coarse
/// contiguity metric that actually decides whether the map is readable.
pub(crate) fn package_of(territory: &Territory) -> BTreeMap<u32, u32> {
    let mut owner_of_name: BTreeMap<&str, u32> = BTreeMap::new();
    for (i, node) in territory.nodes.iter().enumerate() {
        let name = node.path.components().next().unwrap_or("");
        owner_of_name
            .entry(name)
            .or_insert_with(|| u32::try_from(i).expect("district count fits in u32"));
    }
    territory
        .nodes
        .iter()
        .enumerate()
        .map(|(i, node)| {
            let name = node.path.components().next().unwrap_or("");
            (
                u32::try_from(i).expect("district count fits in u32"),
                owner_of_name.get(name).copied().unwrap_or(0),
            )
        })
        .collect()
}

/// The cell boundaries that must survive the short-edge collapse.
///
/// Rules T and A make a district's cells edge-adjacent in the Voronoi diagram —
/// but the diagram is not what the city is made of. Welding, and then the
/// short-edge collapse that fuses two three-way corners into the four- and
/// five-way junctions the map needs, can contract the *one* boundary two cells
/// share down to a single node. The cells then meet at a point, the face walk
/// gives them no shared edge, and a district of two parcels comes out as two
/// pieces. Measured: exactly one district in 238 at 3 000 files, with both plots
/// placed cleanly and no other plot anywhere near their shared boundary.
///
/// So a **hierarchical maximum spanning forest** of the cell-adjacency graph is
/// marked and never contracted: Kruskal, with the boundaries ordered by the
/// depth of the two cells' lowest common directory first and by length second.
///
/// Ordering by common-ancestor depth is what makes the guarantee reach every
/// level of the tree rather than only the leaves. Every boundary internal to a
/// subtree is considered before any boundary that leaves it, so the forest
/// restricted to a subtree is a spanning forest *of that subtree* — `src/auth/`
/// stays one neighbourhood, `src/` stays one quarter, and so does everything
/// between. Ordering by length second means the boundaries taken are the longest
/// available, which the short-edge pass would never have contracted anyway, so
/// the guarantee costs about half a point of four-and-five-plus junction share.
///
/// It is also the minimum that can work: remove any one edge of a spanning
/// forest and its two sides fall apart.
///
/// Returns the mask and the number of districts whose cells were **already** in
/// more than one piece before any contraction, which no mask can repair; that is
/// 0 at every scale measured, and it is returned rather than assumed.
pub(crate) fn links_to_keep(
    cell_edges: &[Vec<usize>],
    cell_district: &[u32],
    territory: &Territory,
    edge_len: &dyn Fn(usize) -> f64,
    edges: usize,
) -> (Vec<bool>, usize) {
    // Which two cells each boundary separates. A boundary used by one cell is
    // the city limit; one used by more than two would not be planar.
    let mut sides: Vec<[u32; 2]> = vec![[u32::MAX; 2]; edges];
    for (ci, list) in cell_edges.iter().enumerate() {
        let cell = u32::try_from(ci).expect("cell count fits in u32");
        for &e in list {
            if e >= edges {
                continue;
            }
            if sides[e][0] == u32::MAX {
                sides[e][0] = cell;
            } else if sides[e][1] == u32::MAX {
                sides[e][1] = cell;
            }
        }
    }
    let depth = territory.depths();
    // Candidates, deepest common directory first and longest first within that.
    // The length is quantised before it is compared so two runs cannot order two
    // near-equal boundaries differently, and the edge index breaks the remaining
    // ties (PRD §7.4).
    let mut candidates: Vec<(u32, f64, usize)> = (0..edges)
        .filter_map(|e| {
            let [a, b] = sides[e];
            if a == u32::MAX || b == u32::MAX {
                return None;
            }
            let da = *cell_district.get(a as usize)?;
            let db = *cell_district.get(b as usize)?;
            Some((
                common_depth(territory, &depth, da, db),
                crate::determinism::quantize_f64(edge_len(e)),
                e,
            ))
        })
        .collect();
    candidates.sort_by(|x, y| {
        y.0.cmp(&x.0)
            .then_with(|| y.1.total_cmp(&x.1))
            .then_with(|| x.2.cmp(&y.2))
    });

    let cells = cell_edges.len();
    let mut parent: Vec<u32> = (0..u32::try_from(cells).expect("cell count fits in u32")).collect();
    let mut mask = vec![false; edges];
    for (_, _, e) in candidates {
        let [a, b] = sides[e];
        let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
        if ra == rb {
            continue;
        }
        parent[ra.max(rb) as usize] = ra.min(rb);
        mask[e] = true;
    }
    // A district still in pieces on its *own* boundaries was never joined by the
    // diagram at all, and no mask could have repaired it.
    let mut own: Vec<u32> = (0..u32::try_from(cells).expect("cell count fits in u32")).collect();
    for &[a, b] in &sides {
        if a == u32::MAX
            || b == u32::MAX
            || cell_district.get(a as usize) != cell_district.get(b as usize)
        {
            continue;
        }
        let (ra, rb) = (find(&mut own, a), find(&mut own, b));
        if ra != rb {
            own[ra.max(rb) as usize] = ra.min(rb);
        }
    }
    let mut split: BTreeMap<u32, BTreeSet<u32>> = BTreeMap::new();
    for (ci, list) in cell_edges.iter().enumerate() {
        if list.is_empty() {
            continue;
        }
        let Some(&d) = cell_district.get(ci) else {
            continue;
        };
        let r = find(&mut own, u32::try_from(ci).expect("fits"));
        split.entry(d).or_default().insert(r);
    }
    let already = split.values().filter(|s| s.len() > 1).count();
    (mask, already)
}

/// Depth of the lowest directory that contains both districts.
///
/// `u32::MAX` for two cells of the same district, so a district's own boundaries
/// always sort before any boundary it shares with a neighbour.
pub(crate) fn common_depth(territory: &Territory, depth: &[u32], first: u32, second: u32) -> u32 {
    if first == second {
        return u32::MAX;
    }
    let (mut x, mut y) = (first, second);
    let up = |n: u32| territory.nodes[n as usize].parent;
    while depth[x as usize] > depth[y as usize] {
        match up(x) {
            Some(above) => x = above,
            None => break,
        }
    }
    while depth[y as usize] > depth[x as usize] {
        match up(y) {
            Some(above) => y = above,
            None => break,
        }
    }
    while x != y {
        match (up(x), up(y)) {
            (Some(px), Some(py)) => {
                x = px;
                y = py;
            }
            _ => return 0,
        }
    }
    depth[x as usize]
}

/// Union-find with path halving.
fn find(parent: &mut [u32], mut x: u32) -> u32 {
    while parent[x as usize] != x {
        parent[x as usize] = parent[parent[x as usize] as usize];
        x = parent[x as usize];
    }
    x
}

/// How many blocks of each group are edge-connected: `group → piece count`.
///
/// Two blocks count as connected only when they share a whole road **edge**.
/// Touching at a single junction does not count, deliberately: a district joined
/// to itself through one point is a district the eye reads as two.
pub(crate) fn pieces(
    blocks: &[BlockPlan],
    group_of: &dyn Fn(u32) -> Option<u32>,
) -> BTreeMap<u32, usize> {
    let n = blocks.len();
    let mut out: BTreeMap<u32, usize> = BTreeMap::new();
    if n == 0 {
        return out;
    }
    let mut edge_blocks: BTreeMap<usize, Vec<u32>> = BTreeMap::new();
    for (bi, b) in blocks.iter().enumerate() {
        for &h in &b.half_edges {
            edge_blocks
                .entry(h / 2)
                .or_default()
                .push(u32::try_from(bi).expect("block count fits in u32"));
        }
    }
    let mut parent: Vec<u32> = (0..u32::try_from(n).expect("block count fits in u32")).collect();
    for group in edge_blocks.values() {
        if group.len() != 2 {
            continue;
        }
        let (x, y) = (group[0], group[1]);
        let gx = blocks[x as usize].district.and_then(group_of);
        let gy = blocks[y as usize].district.and_then(group_of);
        if gx != gy || gx.is_none() {
            continue;
        }
        let (rx, ry) = (find(&mut parent, x), find(&mut parent, y));
        if rx != ry {
            parent[rx.max(ry) as usize] = rx.min(ry);
        }
    }
    let mut roots: BTreeMap<u32, BTreeSet<u32>> = BTreeMap::new();
    for (bi, b) in blocks.iter().enumerate() {
        if let Some(g) = b.district.and_then(group_of) {
            let r = find(&mut parent, u32::try_from(bi).expect("fits"));
            roots.entry(g).or_default().insert(r);
        }
    }
    for (g, set) in roots {
        out.insert(g, set.len());
    }
    out
}

/// How many groups have blocks in more than one connected piece. **Zero** is the
/// design's promise; it is measured on every generated city.
pub(crate) fn fragmented(blocks: &[BlockPlan], group_of: &dyn Fn(u32) -> Option<u32>) -> usize {
    pieces(blocks, group_of)
        .values()
        .filter(|&&n| n > 1)
        .count()
}

/// How many *subtrees* of the directory tree are in more than one piece.
///
/// This is "adjacent directories are adjacent on the ground" stated as something
/// measurable. A district being contiguous is only the leaf case; the claim the
/// graft actually makes is stronger — a subtree is cut out of one face, so every
/// directory at every level should be one mass, `src/` one quarter and
/// `src/auth/` one neighbourhood inside it. Checking only the leaves, or only the
/// top-level packages, would miss a middle level coming apart.
///
/// One union-find per node, over that node's own blocks, so the cost is the
/// tree's depth times the block count rather than its node count times it.
pub(crate) fn fragmented_subtrees(blocks: &[BlockPlan], territory: &Territory) -> usize {
    let n = blocks.len();
    if n == 0 || territory.nodes.is_empty() {
        return 0;
    }
    // Blocks under each node, walking the parent chain up from each block.
    let mut under: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for (bi, b) in blocks.iter().enumerate() {
        let Some(d) = b.district else { continue };
        let bid = u32::try_from(bi).expect("block count fits in u32");
        let mut node = Some(d);
        while let Some(x) = node {
            under.entry(x).or_default().push(bid);
            node = territory.nodes.get(x as usize).and_then(|t| t.parent);
        }
    }
    // Which two blocks each road edge separates.
    let mut edge_blocks: BTreeMap<usize, Vec<u32>> = BTreeMap::new();
    for (bi, b) in blocks.iter().enumerate() {
        for &h in &b.half_edges {
            edge_blocks
                .entry(h / 2)
                .or_default()
                .push(u32::try_from(bi).expect("fits"));
        }
    }
    let neighbours: Vec<(u32, u32)> = edge_blocks
        .values()
        .filter(|g| g.len() == 2)
        .map(|g| (g[0], g[1]))
        .collect();
    let mut member: Vec<u32> = vec![u32::MAX; n];
    let mut split = 0usize;
    for (node, list) in &under {
        for &b in list {
            member[b as usize] = *node;
        }
        let mut parent: Vec<u32> = (0..u32::try_from(n).expect("fits")).collect();
        for &(x, y) in &neighbours {
            if member[x as usize] != *node || member[y as usize] != *node {
                continue;
            }
            let (rx, ry) = (find(&mut parent, x), find(&mut parent, y));
            if rx != ry {
                parent[rx.max(ry) as usize] = rx.min(ry);
            }
        }
        let mut roots: BTreeSet<u32> = BTreeSet::new();
        for &b in list {
            roots.insert(find(&mut parent, b));
        }
        if roots.len() > 1 {
            split += 1;
        }
        for &b in list {
            member[b as usize] = u32::MAX;
        }
    }
    split
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accrete::Params;
    use crate::accrete::{self, Settlement};
    use crate::age::AgeRamp;
    use crate::geom::dist;
    use crate::terrain::TerrainField;
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
                size_bytes: 900 + (i as u64) * 41,
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

    fn settle(files: Vec<FileRec>) -> Settlement {
        let params = Params::for_file_count(files.len());
        let terrain = TerrainField::generate(0x51, 100.0);
        let ramp = ramp_of(&files);
        let t = territory::build(&demands(&files), &|p| p.as_str().starts_with("vendor"));
        accrete::grow(files, t, params, ramp, terrain)
    }

    /// A tree with a deep unbalanced spine — the shape that exposed the
    /// swapped-halves bug in the partition.
    fn deep_tree() -> Vec<&'static str> {
        vec![
            "README.md",
            "core/lib.rs",
            "core/util.rs",
            "web/a.ts",
            "web/adapters/b.ts",
            "web/adapters/server/c.ts",
            "web/adapters/server/retry/d.ts",
            "web/adapters/server/retry/policy/e.ts",
            "web/adapters/server/retry/policy/hasher/f0.ts",
            "web/adapters/server/retry/policy/hasher/f1.ts",
            "web/adapters/server/retry/policy/hasher/f2.ts",
            "web/adapters/server/retry/policy/hasher/f3.ts",
            "web/adapters/server/retry/policy/hasher/f4.ts",
            "web/adapters/server/retry/policy/hasher/f5.ts",
            "web/adapters/server/retry/policy/hasher/f6.ts",
            "web/adapters/server/retry/policy/hasher/f7.ts",
            "docs/guide.md",
            "vendor/blob.js",
            "vendor/other.js",
        ]
    }

    #[test]
    fn demands_do_not_depend_on_the_order_the_files_arrived_in() {
        let files = corpus(&deep_tree());
        let forward = demands(&files);
        let mut shuffled = files;
        shuffled.reverse();
        shuffled.rotate_left(5);
        let backward = demands(&shuffled);
        assert_eq!(forward.len(), backward.len());
        for (a, b) in forward.iter().zip(backward.iter()) {
            assert_eq!(a.0, b.0);
            assert_eq!(a.1.oldest, b.1.oldest);
            assert_eq!(a.1.files, b.1.files);
        }
    }

    /// Rule **A**, checked by brute force rather than trusted.
    ///
    /// Every plot but the first of its district has one of its own district's
    /// plots as its nearest plot *at the moment it was settled*, so its cell is
    /// a Delaunay neighbour of a sibling's.
    #[test]
    fn every_plot_after_the_first_touches_its_own_districts_ground() {
        let s = settle(corpus(&deep_tree()));
        let mut seen_in: BTreeMap<u32, usize> = BTreeMap::new();
        let mut off = 0usize;
        for (i, p) in s.plots.iter().enumerate() {
            let first = *seen_in.entry(p.district).or_insert(0) == 0;
            *seen_in.get_mut(&p.district).expect("just inserted") += 1;
            if first {
                continue;
            }
            let mut nearest = f64::INFINITY;
            let mut nearest_same = f64::INFINITY;
            for q in &s.plots[..i] {
                let d = dist(p.pos, q.pos);
                nearest = nearest.min(d);
                if q.district == p.district {
                    nearest_same = nearest_same.min(d);
                }
            }
            if !touches_own(nearest, nearest_same) {
                off += 1;
            }
        }
        assert_eq!(
            off, s.relaxed.nonadjacent,
            "{off} plots were founded away from their own ground but only {} were counted",
            s.relaxed.nonadjacent
        );
    }

    /// **Every district is one place on the map**, on the real pipeline.
    ///
    /// This is the property the territory polygon used to buy and the graph
    /// partition now buys instead, and it is checked the way the map is read:
    /// over the plot adjacency graph, district by district. The polygon test it
    /// replaces asserted something weaker — that a plot was inside a polygon —
    /// which was true on a city that measured 0.9975 solidity and read as a pie
    /// chart.
    #[test]
    fn every_district_is_one_connected_piece_of_ground() {
        let s = settle(corpus(&deep_tree()));
        let positions = s.positions();
        let cells = crate::voronoi::build(&positions, s.params.sep_rim, 0x51);
        let (graph, cell_edges) =
            crate::roads::Graph::from_cells_indexed(&cells.cells, crate::roads::WELD_TOLERANCE);
        let adjacency = crate::regions::adjacency(&cell_edges, graph.edges.len());
        let seating = crate::regions::partition(&s, &adjacency);
        let mut by_district: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for (i, d) in seating.plot_district.iter().enumerate() {
            by_district
                .entry(*d)
                .or_default()
                .push(u32::try_from(i).expect("fits"));
        }
        assert!(by_district.len() > 4, "the corpus made no districts");
        for (d, plots) in &by_district {
            let inside: BTreeSet<u32> = plots.iter().copied().collect();
            let mut seen: BTreeSet<u32> = BTreeSet::new();
            let mut stack = vec![plots[0]];
            seen.insert(plots[0]);
            while let Some(u) = stack.pop() {
                for &v in &adjacency[u as usize] {
                    if inside.contains(&v) && seen.insert(v) {
                        stack.push(v);
                    }
                }
            }
            assert_eq!(
                seen.len(),
                plots.len(),
                "district {d} came out in more than one piece"
            );
        }
    }

    /// Every file is on a plot of its own district, and no file is lost.
    #[test]
    fn every_file_sits_on_its_own_districts_ground() {
        let s = settle(corpus(&deep_tree()));
        let positions = s.positions();
        let cells = crate::voronoi::build(&positions, s.params.sep_rim, 0x51);
        let (graph, cell_edges) =
            crate::roads::Graph::from_cells_indexed(&cells.cells, crate::roads::WELD_TOLERANCE);
        let adjacency = crate::regions::adjacency(&cell_edges, graph.edges.len());
        let seating = crate::regions::partition(&s, &adjacency);
        assert_eq!(seating.file_plot.len(), s.files.len());
        let seated: usize = seating.plot_files.iter().map(Vec::len).sum();
        assert_eq!(seated, s.files.len(), "a file was dropped by the seating");
        assert_eq!(seating.faceless, 0, "a district was given no ground at all");
    }

    /// A root with `kids` children, which is enough tree for the ordering tests.
    fn siblings(kids: u32) -> Territory {
        let node = |path: &str, parent: Option<u32>| crate::territory::Node {
            path: if path.is_empty() {
                LogicalPath::root()
            } else {
                lp(path)
            },
            parent,
            children: Vec::new(),
            own_units: 1,
            subtree_units: 1,
            own_files: 1,
            subtree_files: 1,
            own_oldest: 0,
            oldest: 0,
            industrial: false,
        };
        let mut t = Territory {
            nodes: vec![node("", None)],
            ..Territory::default()
        };
        for i in 0..kids {
            t.nodes.push(node(&format!("d{i}"), Some(0)));
            t.nodes[0].children.push(i + 1);
        }
        t
    }

    /// The kept links are a spanning forest, deepest common directory first and
    /// longest first within that.
    #[test]
    fn the_kept_links_are_a_hierarchical_maximum_spanning_forest() {
        // Cells 0,1,2 of district 7 in a triangle (edges 0,1,2), cell 3 of the
        // neighbouring district 8 hanging off cell 0 (edge 3).
        let cell_edges = vec![vec![0, 2, 3], vec![0, 1], vec![1, 2], vec![3]];
        let district = vec![7, 7, 7, 8];
        let len = |e: usize| match e {
            0 => 3.0,
            1 => 5.0,
            2 => 4.0,
            _ => 9.0,
        };
        let t = siblings(9);
        let (mask, already) = links_to_keep(&cell_edges, &district, &t, &len, 4);
        assert_eq!(already, 0);
        // Inside district 7, two of the three edges hold the triangle together
        // and the shortest — the one the collapse pass most wants — is left free.
        // Edge 3 is kept too, and only because it is what holds the two
        // districts' *parent* together; it is considered after every one of
        // district 7's own edges even though it is the longest of the four.
        assert_eq!(mask, vec![false, true, true, true], "{mask:?}");
    }

    /// A district the diagram itself never joined is reported, not papered over.
    #[test]
    fn cells_that_never_touched_are_counted_as_already_split() {
        let cell_edges = vec![vec![0], vec![1]];
        let district = vec![7, 7];
        let t = siblings(9);
        let (mask, already) = links_to_keep(&cell_edges, &district, &t, &|_| 1.0, 2);
        assert_eq!(already, 1);
        assert_eq!(mask, vec![false, false]);
    }

    #[test]
    fn a_group_of_one_block_is_one_piece() {
        let plan = |district: Option<u32>, half_edges: Vec<usize>| BlockPlan {
            ring: vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0]],
            district,
            plots: Vec::new(),
            open: true,
            industrial: false,
            half_edges,
            birth: 0,
        };
        // Two blocks of district 0 sharing edge 1, one block of district 1 apart.
        let blocks = vec![
            plan(Some(0), vec![0, 2]),
            plan(Some(0), vec![3, 4]),
            plan(Some(1), vec![6]),
        ];
        let counts = pieces(&blocks, &|d| Some(d));
        assert_eq!(counts.get(&0), Some(&1));
        assert_eq!(counts.get(&1), Some(&1));
        assert_eq!(fragmented(&blocks, &|d| Some(d)), 0);
        // Break the shared edge: the two blocks of district 0 fall apart.
        let split = vec![
            plan(Some(0), vec![0]),
            plan(Some(0), vec![4]),
            plan(Some(1), vec![6]),
        ];
        assert_eq!(pieces(&split, &|d| Some(d)).get(&0), Some(&2));
        assert_eq!(fragmented(&split, &|d| Some(d)), 1);
    }
}
