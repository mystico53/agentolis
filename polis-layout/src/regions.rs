//! Stage 2 — the district partition, on the **graph** and not on the plane.
//!
//! # The question this module answers
//!
//! Accretion decides district membership emergently: a district is whatever
//! fell out of where its plots happened to land. Measured on the prototype at
//! 5 000 files that gave 102 of 276 districts more than one disconnected piece,
//! so in the dense core the colour changed every two or three blocks and the
//! district masses were unreadable exactly where the city was densest. That
//! breaks PRD §9 ("the tree determines placement") and PRD §8 ("the district
//! skeleton must stay readable at every zoom").
//!
//! The previous fix was to partition the **plane** first — a convex city limit,
//! fanned into wedges and chord-split down the directory tree, with a plot
//! allowed to settle only inside its own district's polygon. It worked, and it
//! is also precisely why three M1 gates in a row reported a pie chart on a coin:
//! a convex partition of a convex region has a convex silhouette (solidity
//! 0.9975) and straight borders (four to nine of them running from the middle of
//! the city to its edge, within two degrees of radial).
//!
//! Contiguity is a property of a graph, so this module obtains it on the graph:
//!
//! > The plots are already settled. Partition the **plot adjacency graph**
//! > recursively down the directory tree, balanced by weight, exactly as the
//! > treemap partitioned the plane — but each part is a connected set of
//! > *cells*, and its boundary is whatever shape the cells make.
//!
//! Nothing here touches a coordinate. The outline of the city stays the outline
//! of the ground the growth settled, every border is a chain of Voronoi
//! bisectors, and no construction in the module has a centre or a radius.
//!
//! # Why every part comes out connected
//!
//! A split of a connected plot set `S` picks two seeds, `a` (the oldest ground
//! in `S`) and `b` (the vertex furthest from `a` by shortest path), runs
//! Dijkstra from each, and cuts on the **difference**:
//!
//! ```text
//!     A = { v ∈ S : d_a(v) − d_b(v) <  λ }
//!     B = { v ∈ S : d_a(v) − d_b(v) ≥ λ }
//! ```
//!
//! Both halves are connected, for any λ, and the proof is three lines. Take
//! `v ∈ A` and any `u` on a shortest path from `v` to `a`, with `w` the length
//! of the `u…v` leg. Then `d_a(u) = d_a(v) − w` exactly, while `d_b(u) ≥
//! d_b(v) − w` by the triangle inequality. Subtracting,
//! `d_a(u) − d_b(u) ≤ d_a(v) − d_b(v) < λ`, so `u ∈ A` too — every vertex of
//! `A` reaches `a` without leaving `A`. The same argument with the roles
//! swapped puts every vertex of `B` on a path to `b` inside `B`.
//!
//! So the cut is chosen purely for **balance**: `A` is a prefix of the vertices
//! sorted by `d_a − d_b`, and the prefix length is picked to give each side
//! enough plot **capacity** for the files under it. Connectivity is not traded
//! against balance; it is free.
//!
//! Distances are integers (thousandths of a world unit), so the sort key is
//! exact and two runs cannot disagree in the last bit (PRD §7.4). Where several
//! vertices share a key the cut may fall inside the group, which is the one case
//! the argument does not cover: two vertices with the same key can lie on each
//! other's shortest path to a seed. Such a cut is **checked** rather than
//! forbidden — forbidding it is not free, because a district one plot short is a
//! district whose files double up on somebody else's parcel, and that is how a
//! file vanishes from the map.
//!
//! And whatever the search settles on, [`repair`] runs after it. The property
//! this module exists for is not allowed to depend on a search succeeding.
//!
//! # Where the old town ends up
//!
//! Seed `a` is the **oldest plot** in `S`, and the older half of the item list
//! takes `A`. The growth settles from the origin outward in commit order, so the
//! oldest ground is the middle of the town — and the old quarters therefore end
//! up in the middle without anything in this module knowing what "middle" means
//! (PRD §7.1).
//!
//! # Where the terrain gets in
//!
//! An edge of the adjacency graph costs its length plus a share of the height
//! difference across it ([`GRADIENT_LEAN`]). A shortest path therefore prefers
//! to run along a contour, the `d_a − d_b` field bends with the relief, and a
//! district border follows a valley rather than cutting across it — the same
//! intent the chord partition had with its 30 % gradient lean, obtained without
//! a chord.

// Three lint families fire on this module without telling us anything: the
// `cast_*` family, where every cast lands in an index or a quantised integer
// distance that is clamped on purpose; `float_cmp`, because an exact comparison
// is how a determinism tie is broken (PRD §7.4) and an approximate one there
// would be the bug; and `many_single_char_names`, because `a`, `b`, `d` and `s`
// are the names the graph argument itself uses.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::float_cmp,
    clippy::many_single_char_names,
    clippy::too_many_lines
)]

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};

use crate::accrete::Settlement;
use crate::determinism::quantize_f64;
use crate::geom::dist;

/// How much of the height difference across an adjacency is charged to it, per
/// world unit of height, in thousandths.
///
/// A border follows a contour when climbing is expensive. Set so a one-unit
/// climb costs about as much as three units of level ground — steep enough to
/// bend a border round a hill, not so steep that a district cannot cross a
/// ridge at all.
const GRADIENT_LEAN: f64 = 3.0;

/// Distances are integers in thousandths of a world unit.
const MILLI: f64 = 1_000.0;

/// Which district each plot belongs to, and which plot each file sits on.
///
/// Produced by [`partition`] from the geometry alone. The provisional labels the
/// growth used for its own shaping weights are **not** an input: keeping them
/// out is what makes this a pure function of the settled ground, so an
/// incrementally grown city and one generated from scratch agree.
#[derive(Debug, Clone, Default)]
pub(crate) struct Seating {
    /// Territory node that owns each plot.
    pub(crate) plot_district: Vec<u32>,
    /// Files on each plot, in growth order.
    pub(crate) plot_files: Vec<Vec<u32>>,
    /// `files[i]` lives in `plots[file_plot[i]]`.
    pub(crate) file_plot: Vec<u32>,
    /// Districts that had to share another district's ground because the
    /// partition ran out of plots to divide. Reported, never hidden.
    pub(crate) shared: usize,
    /// Districts with files that were given no ground of their own at all.
    pub(crate) faceless: usize,
}

/// Which plots are Voronoi neighbours, from the welded cell boundaries.
///
/// A pair is adjacent when their cells share a road edge — the same relation the
/// finished map draws, so a district connected here is a district whose blocks
/// are edge-connected on the map.
pub(crate) fn adjacency(cell_edges: &[Vec<usize>], edges: usize) -> Vec<Vec<u32>> {
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
    let mut out: Vec<Vec<u32>> = vec![Vec::new(); cell_edges.len()];
    for &[a, b] in &sides {
        if a == u32::MAX || b == u32::MAX || a == b {
            continue;
        }
        out[a as usize].push(b);
        out[b as usize].push(a);
    }
    for list in &mut out {
        list.sort_unstable();
        list.dedup();
    }
    out
}

/// State the recursion carries.
struct Cutter<'a> {
    s: &'a Settlement,
    /// Neighbour lists, plus the integer cost of each adjacency.
    nbr: Vec<Vec<(u32, i64)>>,
    /// Plots a district owns outright.
    owner: Vec<u32>,
    /// Plots each district's files are seated on.
    home: Vec<Vec<u32>>,
    /// Districts with files under each subtree: the hard floor on its plots.
    floor: Vec<u32>,
    shared: usize,
}

/// One entry in a node's split list: the directory's own files, or a child.
#[derive(Debug, Clone, Copy)]
struct Item {
    /// `None` for the directory's own files.
    child: Option<u32>,
    /// Quantised weight, for choosing the split index.
    units: u64,
    /// Files under this item. What its ground has to have room for.
    need: u32,
    /// Districts with files of their own under this item: what it must have.
    ///
    /// A district given no plot at all has to seat its files on a stranger's
    /// ground, which is how a district disappears from the map under its own
    /// name. One plot each is the floor, and it is enforced rather than hoped
    /// for.
    floor: u32,
    oldest: u32,
}

/// Partition the plot adjacency graph down the directory tree.
pub(crate) fn partition(s: &Settlement, adj: &[Vec<u32>]) -> Seating {
    let n = s.plots.len();
    let mut out = Seating {
        plot_district: vec![0; n],
        plot_files: vec![Vec::new(); n],
        file_plot: vec![0; s.files.len()],
        ..Seating::default()
    };
    if n == 0 || s.territory.nodes.is_empty() {
        return out;
    }
    let nbr = weighted(s, adj);
    let districts = s.territory.nodes.len();

    // Districts with files of their own: the hard floor of one plot each.
    let mut floor = vec![0u32; districts];
    for (d, node) in s.territory.nodes.iter().enumerate() {
        floor[d] = u32::from(node.own_files > 0);
    }
    for i in (0..districts).rev() {
        let kids = s.territory.nodes[i].children.clone();
        let mut total = floor[i];
        for c in kids {
            total = total.saturating_add(floor[c as usize]);
        }
        floor[i] = total;
    }

    let mut cut = Cutter {
        s,
        nbr,
        owner: vec![u32::MAX; n],
        home: vec![Vec::new(); districts],
        floor,
        shared: 0,
    };
    let all: Vec<u32> = (0..u32::try_from(n).expect("plot count fits in u32")).collect();
    let root = s.territory.root();
    // Scratch for the induced-subgraph index, allocated once and reused by
    // every split. See [`bisect`] for why it is not a map.
    let mut mark: Vec<u32> = vec![0; n];
    descend(&mut cut, &mut mark, root, all, true);
    heal(&mut cut);

    // A district the recursion never reached takes its nearest ancestor's
    // ground. Counted, because a district with no ground of its own is a
    // district that does not appear on the map under its own name.
    let mut faceless = 0usize;
    for d in 0..districts {
        if s.territory.nodes[d].own_files == 0 || !cut.home[d].is_empty() {
            continue;
        }
        faceless += 1;
        let mut up = s.territory.nodes[d].parent;
        while let Some(a) = up {
            if !cut.home[a as usize].is_empty() {
                let ground = cut.home[a as usize].clone();
                cut.home[d] = ground;
                break;
            }
            up = s.territory.nodes[a as usize].parent;
        }
        if cut.home[d].is_empty() {
            cut.home[d] = vec![0];
        }
    }

    // Every plot has an owner: the root's recursion covers all of them, but a
    // plot the recursion somehow missed belongs to the root rather than to
    // whatever index happened to be in the slot.
    for (p, o) in cut.owner.iter_mut().enumerate() {
        if *o == u32::MAX {
            *o = root;
        }
        out.plot_district[p] = *o;
    }
    out.shared = cut.shared;
    out.faceless = faceless;
    seat_files(s, &cut.home, &mut out);
    out
}

/// Neighbour lists with the integer cost of each adjacency.
fn weighted(s: &Settlement, adj: &[Vec<u32>]) -> Vec<Vec<(u32, i64)>> {
    let n = s.plots.len();
    let mut nbr: Vec<Vec<(u32, i64)>> = vec![Vec::new(); n];
    for (a, list) in adj.iter().enumerate().take(n) {
        for &b in list {
            if b as usize >= n {
                continue;
            }
            nbr[a].push((b, cost(s, a as u32, b)));
        }
    }
    // The diagram can leave an island — one plot whose cell was clipped away, or
    // a pocket the phantom ring cut off — and a disconnected graph would make
    // the shortest-path argument vacuous for the piece that does not contain a
    // seed. Islands are joined to the mainland by their nearest plot, so the
    // partition always sees one connected graph.
    join_islands(s, &mut nbr);
    for list in &mut nbr {
        list.sort_unstable();
        list.dedup();
    }
    nbr
}

/// Cost of one adjacency: its length, plus a share of the climb across it.
fn cost(s: &Settlement, a: u32, b: u32) -> i64 {
    let pa = s.plots[a as usize].pos;
    let pb = s.plots[b as usize].pos;
    let d = dist(pa, pb);
    let climb = (s.terrain.height_f64(pa[0], pa[1]) - s.terrain.height_f64(pb[0], pb[1])).abs();
    let w = quantize_f64(d + GRADIENT_LEAN * climb);
    ((w * MILLI).round() as i64).max(1)
}

/// Connect every component of the adjacency graph to the one holding plot 0.
fn join_islands(s: &Settlement, nbr: &mut [Vec<(u32, i64)>]) {
    let n = nbr.len();
    if n == 0 {
        return;
    }
    let mut comp = vec![u32::MAX; n];
    let mut roots: Vec<u32> = Vec::new();
    for start in 0..n {
        if comp[start] != u32::MAX {
            continue;
        }
        let id = u32::try_from(roots.len()).expect("component count fits in u32");
        roots.push(u32::try_from(start).expect("fits"));
        comp[start] = id;
        let mut stack = vec![start];
        while let Some(u) = stack.pop() {
            for &(v, _) in &nbr[u] {
                if comp[v as usize] == u32::MAX {
                    comp[v as usize] = id;
                    stack.push(v as usize);
                }
            }
        }
    }
    if roots.len() <= 1 {
        return;
    }
    for id in 1..roots.len() {
        let id = u32::try_from(id).expect("fits");
        // The nearest pair between this island and everything already joined,
        // scanned in plot order so the pair is a property of the geometry.
        let mut best: Option<(f64, u32, u32)> = None;
        for a in 0..n {
            if comp[a] != id {
                continue;
            }
            for (b, cb) in comp.iter().enumerate().take(n) {
                if *cb == id {
                    continue;
                }
                let d = quantize_f64(dist(s.plots[a].pos, s.plots[b].pos));
                let a32 = u32::try_from(a).expect("fits");
                let b32 = u32::try_from(b).expect("fits");
                if best.is_none_or(|(bd, ba, bb)| d < bd || (d == bd && (a32, b32) < (ba, bb))) {
                    best = Some((d, a32, b32));
                }
            }
        }
        if let Some((_, a, b)) = best {
            let w = cost(s, a, b);
            nbr[a as usize].push((b, w));
            nbr[b as usize].push((a, w));
            let from = comp[b as usize];
            for c in &mut comp {
                if *c == id {
                    *c = from;
                }
            }
        }
    }
}

/// Divide `plots` between a district's own files and its children.
fn descend(cut: &mut Cutter<'_>, mark: &mut [u32], node: u32, plots: Vec<u32>, strict: bool) {
    if plots.is_empty() {
        return;
    }
    let n = &cut.s.territory.nodes[node as usize];
    let own_files = n.own_files;
    let kids: Vec<u32> = n.children.clone();
    let own_units = n.own_units;
    let own_oldest = n.own_oldest;

    let mut items: Vec<Item> = Vec::with_capacity(kids.len() + 1);
    if own_files > 0 {
        items.push(Item {
            child: None,
            units: own_units.max(1),
            need: own_files,
            floor: u32::from(own_files > 0),
            // The directory's own files are the oldest thing in it that is not a
            // child's, or the civic square would migrate to whichever child
            // happened to be committed first.
            oldest: own_oldest,
        });
    }
    for &c in &kids {
        let kid = &cut.s.territory.nodes[c as usize];
        if kid.subtree_files == 0 {
            continue;
        }
        items.push(Item {
            child: Some(c),
            units: kid.subtree_units.max(1),
            need: kid.subtree_files,
            floor: cut.floor[c as usize],
            oldest: kid.oldest,
        });
    }
    if items.is_empty() {
        assign(cut, node, &plots);
        return;
    }
    // `(oldest, path)`, with the directory's own files first among equals: the
    // civic square belongs at the historic centre of its own quarter.
    let paths: Vec<String> = items
        .iter()
        .map(|it| {
            it.child.map_or_else(String::new, |c| {
                cut.s.territory.nodes[c as usize].path.as_str().to_owned()
            })
        })
        .collect();
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by(|&x, &y| {
        (items[x].oldest, items[x].child.is_some(), &paths[x]).cmp(&(
            items[y].oldest,
            items[y].child.is_some(),
            &paths[y],
        ))
    });
    let items: Vec<Item> = order.into_iter().map(|i| items[i]).collect();
    place(cut, mark, node, &items, plots, strict);
}

/// Split a run of items between the plots it was given.
fn place(
    cut: &mut Cutter<'_>,
    mark: &mut [u32],
    node: u32,
    items: &[Item],
    plots: Vec<u32>,
    strict: bool,
) {
    if items.len() == 1 {
        match items[0].child {
            None => assign(cut, node, &plots),
            // Below the top level a package is already one connected piece;
            // what happens inside it cannot break that.
            Some(c) => descend(cut, mark, c, plots, false),
        }
        return;
    }
    if plots.len() < 2 {
        // Not enough ground left to divide. The oldest item takes it and the
        // rest are seated on the same plots — the honest outcome, and counted.
        share(cut, &items[1..], &plots);
        place(cut, mark, node, &items[..1], plots, strict);
        return;
    }
    // Split at the index that best balances quantised weight, keeping the order.
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
    let need_a: u64 = items[..k].iter().map(|i| u64::from(i.need)).sum();
    let need_b: u64 = items[k..].iter().map(|i| u64::from(i.need)).sum();
    let floor_a: usize = items[..k].iter().map(|i| i.floor as usize).sum();
    let floor_b: usize = items[k..].iter().map(|i| i.floor as usize).sum();
    let (a, b) = bisect(cut, mark, &plots, need_a, need_b, floor_a, floor_b, strict);
    place(cut, mark, node, &items[..k], a, strict);
    place(cut, mark, node, &items[k..], b, strict);
}

/// Put every district that came out in more than one piece back together.
///
/// The recursion guarantees connectivity where it is bought at the top level;
/// below that a cut goes where the balance wants and a district can end up with
/// a stray parcel or two on the far side of a neighbour. This gives those
/// strays away — to the neighbouring district that already borders them most —
/// and keeps the piece holding the district's oldest ground.
///
/// Giving a stray away cannot break the receiving district: the stray is
/// adjacent to it, so the union stays connected. Each pass therefore strictly
/// reduces the number of pieces, and the loop terminates.
fn heal(cut: &mut Cutter<'_>) {
    let n = cut.owner.len();
    // A stray is given to a neighbour **in its own top-level package** wherever
    // one borders it. Handing it across a package boundary would heal the
    // district and split the package, which is the coarser of the two failures:
    // colour is keyed on the package, so a package in two pieces is a family the
    // eye cannot find.
    let package = crate::districts::package_of(&cut.s.territory);
    for _pass in 0..HEAL_PASSES {
        let mut moved = false;
        let mut by_district: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for (i, d) in cut.owner.iter().enumerate() {
            by_district
                .entry(*d)
                .or_default()
                .push(u32::try_from(i).expect("plot count fits in u32"));
        }
        for (d, plots) in by_district {
            if plots.len() < 2 {
                continue;
            }
            let inside: std::collections::BTreeSet<u32> = plots.iter().copied().collect();
            // Components, and which one holds the oldest ground: that is the
            // piece the district keeps, so the old town stays where it was.
            let mut seen = vec![false; n];
            let mut comps: Vec<Vec<u32>> = Vec::new();
            for &start in &plots {
                if seen[start as usize] {
                    continue;
                }
                let mut comp = Vec::new();
                let mut stack = vec![start];
                seen[start as usize] = true;
                while let Some(u) = stack.pop() {
                    comp.push(u);
                    for &(v, _) in &cut.nbr[u as usize] {
                        if inside.contains(&v) && !seen[v as usize] {
                            seen[v as usize] = true;
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
                    .map(|&p| (cut.s.plots[p as usize].birth, p))
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
                // The neighbouring district that borders this piece most, ties
                // to the lowest index.
                let mut tally: BTreeMap<u32, usize> = BTreeMap::new();
                let mut kin: BTreeMap<u32, usize> = BTreeMap::new();
                let own_package = package.get(&d).copied();
                for &p in comp {
                    for &(v, _) in &cut.nbr[p as usize] {
                        let e = cut.owner[v as usize];
                        if e == d {
                            continue;
                        }
                        *tally.entry(e).or_insert(0) += 1;
                        if package.get(&e).copied() == own_package {
                            *kin.entry(e).or_insert(0) += 1;
                        }
                    }
                }
                let pick = if kin.is_empty() { &tally } else { &kin };
                let Some((&target, _)) = pick.iter().max_by_key(|(e, c)| (**c, Reverse(**e)))
                else {
                    continue;
                };
                for &p in comp {
                    cut.owner[p as usize] = target;
                }
                cut.home[target as usize].extend_from_slice(comp);
                cut.home[target as usize].sort_unstable();
                cut.home[target as usize].dedup();
                cut.home[d as usize].retain(|p| !comp.contains(p));
                moved = true;
            }
        }
        if !moved {
            break;
        }
    }
}

/// How many times [`heal`] sweeps the districts before it gives up.
///
/// Each sweep strictly reduces the number of pieces, so two is nearly always
/// one more than it needs; the cap is there so a bug cannot spin.
const HEAL_PASSES: usize = 4;

/// A district's own ground: it owns these plots and its files sit on them.
fn assign(cut: &mut Cutter<'_>, node: u32, plots: &[u32]) {
    for &p in plots {
        cut.owner[p as usize] = node;
    }
    cut.home[node as usize] = plots.to_vec();
}

/// Seat every district under these items on ground it does not own.
fn share(cut: &mut Cutter<'_>, items: &[Item], plots: &[u32]) {
    let mut stack: Vec<u32> = Vec::new();
    for it in items {
        match it.child {
            None => {
                cut.shared += 1;
                // The `None` item is the node itself, handled by its caller.
            }
            Some(c) => stack.push(c),
        }
    }
    while let Some(d) = stack.pop() {
        if cut.s.territory.nodes[d as usize].own_files > 0 && cut.home[d as usize].is_empty() {
            cut.home[d as usize] = plots.to_vec();
            cut.shared += 1;
        }
        for &c in &cut.s.territory.nodes[d as usize].children {
            stack.push(c);
        }
    }
}

/// Cut a connected plot set in two, both halves connected, balanced by need.
fn bisect(
    cut: &Cutter<'_>,
    mark: &mut [u32],
    plots: &[u32],
    need_a: u64,
    need_b: u64,
    floor_a: usize,
    floor_b: usize,
    strict: bool,
) -> (Vec<u32>, Vec<u32>) {
    let n = plots.len();
    debug_assert!(n >= 2);
    // Local indices, so the Dijkstra runs over the induced subgraph only.
    //
    // A flat `plot -> local index + 1`, with 0 for "not in this set", rather
    // than a map: this is read once per edge in the innermost loop of two
    // Dijkstras and of `repair`, and a `BTreeMap` probe there costs more than
    // the relaxation it guards. Only the touched entries are cleared, so a
    // split stays linear in its own plot count rather than in the city's.
    // It is a lookup structure and nothing iterates it, so the search, the
    // orderings and every byte downstream are unchanged.
    for (i, &p) in plots.iter().enumerate() {
        mark[p as usize] = u32::try_from(i + 1).expect("plot count fits in u32");
    }
    // The oldest ground in the set. The older half of the item list takes it,
    // so the old town ends up where the town started.
    let mut seed_a = 0usize;
    let mut key_a = (u32::MAX, u32::MAX);
    for (i, &p) in plots.iter().enumerate() {
        let k = (cut.s.plots[p as usize].birth, p);
        if k < key_a {
            key_a = k;
            seed_a = i;
        }
    }
    let da = dijkstra(cut, plots, mark, seed_a);
    // …and the far end of the settlement from it: the graph's own longest axis.
    let mut seed_b = seed_a;
    let mut key_b = (i64::MIN, u32::MAX);
    for i in 0..n {
        let k = (da[i], plots[i]);
        if k.0 > key_b.0 || (k.0 == key_b.0 && k.1 < key_b.1) {
            key_b = k;
            seed_b = i;
        }
    }
    if seed_b == seed_a {
        seed_b = usize::from(seed_a == 0);
    }
    let db = dijkstra(cut, plots, mark, seed_b);

    // Sort on `d_a − d_b`, ties by distance from `a` and then by plot id, and cut
    // the prefix that gives each side the plots its files need. The key ordering
    // is what makes both halves connected — see the module documentation — and
    // the cut point is then free to be exactly the balance that was asked for.
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by_key(|&i| (da[i] - db[i], plots[i]));

    // The quantity being divided is **capacity**, not plots. A district's ground
    // has to hold its files, and a plot's capacity is set by the age of the
    // ground it was settled on — so two halves with the right plot counts and
    // the wrong ages leave one of them short. That was measured: a directory of
    // five files was handed one plot of capacity two, its files doubled up, and
    // one of them lost its building.
    let cap: Vec<u64> = order
        .iter()
        .map(|&i| u64::from(cut.s.plots[plots[i] as usize].cap))
        .collect();
    let mut prefix: Vec<u64> = Vec::with_capacity(n + 1);
    prefix.push(0);
    for c in &cap {
        prefix.push(prefix[prefix.len() - 1] + c);
    }
    let total = prefix[n];
    let want = (total * need_a)
        .checked_div(need_a + need_b)
        .unwrap_or(total / 2);

    // Every district under each half must get at least one plot, so the cut is
    // clamped into the window both floors leave. A window that has closed means
    // there is not enough ground for one plot each, and the recursion records
    // the shortfall as a shared face rather than silently losing it.
    let lo = floor_a.max(1).min(n - 1);
    let hi = n.saturating_sub(floor_b.max(1)).max(lo);

    // At the top level the cut may only fall **between two key groups**.
    //
    // That is the connectivity guarantee, and it is exact: the argument in the
    // module documentation shows that `{v : key(v) < λ}` and its complement are
    // both connected for any λ, a cut at a group boundary *is* such a pair, and
    // a cut inside a group is not — two vertices sharing a key can lie on each
    // other's shortest path to a seed. The top level is where the top-level
    // **packages** are divided, so each package comes out as one connected piece
    // of ground and nothing that happens inside it afterwards can change that.
    //
    // Below the top level the restriction is dropped and the cut goes wherever
    // the balance wants, because there the cost is real and the benefit is not:
    // a package is already one piece, [`heal`] puts any district that came out
    // in two back together, and restricting every cut all the way down left 171
    // of 2 075 Django districts seated on a sibling's ground for a property that
    // was already held.
    let key_at = |i: usize| da[order[i]] - db[order[i]];
    let boundary = |m: usize| m > 0 && m < n && key_at(m - 1) != key_at(m);
    let mut candidates: Vec<usize> = (lo..=hi.min(n - 1))
        .filter(|m| !strict || boundary(*m))
        .collect();
    if candidates.is_empty() {
        // The floors cannot both be met at a group boundary. Meeting them is a
        // preference; staying connected is not.
        candidates = (1..n).filter(|m| !strict || boundary(*m)).collect();
    }
    let mut chosen: Option<(u64, usize)> = None;
    for m in candidates {
        let ca = prefix[m];
        let cb = total - ca;
        // A file with no parcel of its own and a district with no ground of its
        // own are the two failures worth avoiding, and they weigh the same.
        let short = need_a.saturating_sub(ca)
            + need_b.saturating_sub(cb)
            + (floor_a.saturating_sub(m) + floor_b.saturating_sub(n - m)) as u64;
        let penalty = short.saturating_mul(DEFICIT_WEIGHT) + ca.abs_diff(want);
        if chosen.is_none_or(|(best, _)| penalty < best) {
            chosen = Some((penalty, m));
        }
    }

    let mut side: Vec<bool> = vec![false; n];
    if let Some((_, m)) = chosen {
        for &i in &order[..m] {
            side[i] = true;
        }
    } else {
        // Every vertex has the same key — a set the shortest-path field
        // cannot tell apart at all, which happens on a two-plot leaf. There
        // is no group boundary to cut at, so cut where the balance wants and
        // let [`repair`] be the net.
        let m = prefix.partition_point(|c| *c < want).clamp(1, n - 1);
        for &i in &order[..m] {
            side[i] = true;
        }
        repair(cut, plots, mark, &mut side, order[0], true);
        repair(cut, plots, mark, &mut side, order[n - 1], false);
    }

    let mut a: Vec<u32> = Vec::new();
    let mut b: Vec<u32> = Vec::new();
    for (i, &p) in plots.iter().enumerate() {
        if side[i] {
            a.push(p);
        } else {
            b.push(p);
        }
    }
    a.sort_unstable();
    b.sort_unstable();
    // Hand the scratch back empty. The recursion below this split re-marks a
    // subset of these same plots, so a stale entry would put a plot in a set it
    // does not belong to.
    for &p in plots {
        mark[p as usize] = 0;
    }
    (a, b)
}

/// How much worse one file of capacity shortfall is than one unit of imbalance.
///
/// Large, because they are different kinds of thing: an imbalance is a district
/// with roomier parcels than its neighbour, and a shortfall is a file with no
/// parcel of its own.
const DEFICIT_WEIGHT: u64 = 1 << 20;

/// Move every piece of one side that does not hold its own anchor to the other.
///
/// # Why this is safe in this order, and only in this order
///
/// Run for `A` first. Every component of `A` that does not hold `A`'s anchor is
/// a *maximal* connected piece of `A`, so all of its outside neighbours are in
/// `B` — moving it across therefore attaches it to `B` rather than stranding it,
/// and what is left of `A` is the single piece holding the anchor.
///
/// Then run for `B`. `A` is now connected, and the same argument applies with
/// the sides swapped: every component of `B` that does not hold `B`'s anchor has
/// all of its outside neighbours in the current `A`, so moving it across keeps
/// `A` connected. Both anchors stay where they were, so neither side empties.
fn repair(
    cut: &Cutter<'_>,
    plots: &[u32],
    mark: &[u32],
    side: &mut [bool],
    anchor: usize,
    want: bool,
) {
    let n = plots.len();
    if side[anchor] != want {
        return;
    }
    let mut seen = vec![false; n];
    let mut stack = vec![anchor];
    seen[anchor] = true;
    while let Some(u) = stack.pop() {
        for &(v, _) in &cut.nbr[plots[u] as usize] {
            let j = match mark[v as usize] {
                0 => continue,
                i => i as usize - 1,
            };
            if side[j] == want && !seen[j] {
                seen[j] = true;
                stack.push(j);
            }
        }
    }
    for i in 0..n {
        if side[i] == want && !seen[i] {
            side[i] = !want;
        }
    }
}

/// Integer Dijkstra over the induced subgraph, from one local index.
fn dijkstra(cut: &Cutter<'_>, plots: &[u32], mark: &[u32], from: usize) -> Vec<i64> {
    // A vertex the walk cannot reach is placed at the far end of the ordering
    // rather than at infinity, so an arithmetic difference stays finite.
    const UNREACHED: i64 = 1 << 40;
    let mut d = vec![UNREACHED; plots.len()];
    d[from] = 0;
    let mut heap: BinaryHeap<Reverse<(i64, u32)>> = BinaryHeap::new();
    heap.push(Reverse((0, plots[from])));
    while let Some(Reverse((cost, plot))) = heap.pop() {
        let u = match mark[plot as usize] {
            0 => continue,
            i => i as usize - 1,
        };
        if cost > d[u] {
            continue;
        }
        for &(v, w) in &cut.nbr[plot as usize] {
            let j = match mark[v as usize] {
                0 => continue,
                i => i as usize - 1,
            };
            let next = cost + w;
            if next < d[j] {
                d[j] = next;
                heap.push(Reverse((next, v)));
            }
        }
    }
    d
}

/// Put every file on a plot of its own district.
///
/// Files in growth order onto plots in settlement order, filling each to its
/// capacity: within a district, the older files take the older ground, so the
/// age gradient survives the partition. A district given fewer plots than its
/// capacity needs spreads the surplus over the emptiest of them rather than
/// piling it on the last.
fn seat_files(s: &Settlement, home: &[Vec<u32>], out: &mut Seating) {
    let mut by_district: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for fi in 0..s.files.len() {
        // The file's district is a property of its path, never of where the
        // growth happened to put its plot.
        let fi = u32::try_from(fi).expect("file count fits in u32");
        by_district
            .entry(s.district_of_file(fi))
            .or_default()
            .push(fi);
    }
    for (d, mut files) in by_district {
        files.sort_by(|&x, &y| {
            let a = &s.files[x as usize];
            let b = &s.files[y as usize];
            (a.growth_index, a.path.as_str()).cmp(&(b.growth_index, b.path.as_str()))
        });
        let mut plots = home.get(d as usize).cloned().unwrap_or_default();
        if plots.is_empty() {
            plots = vec![0];
        }
        plots.sort_by_key(|&p| (s.plots[p as usize].birth, p));
        let mut at = 0usize;
        for f in files {
            // The first plot with room, then the emptiest of them all.
            let mut chosen = None;
            while at < plots.len() {
                let p = plots[at];
                if (out.plot_files[p as usize].len() as u32) < s.plots[p as usize].cap {
                    chosen = Some(p);
                    break;
                }
                at += 1;
            }
            let p = chosen.unwrap_or_else(|| {
                let mut best = (usize::MAX, u32::MAX);
                for &p in &plots {
                    let k = (out.plot_files[p as usize].len(), p);
                    if k < best {
                        best = k;
                    }
                }
                best.1
            });
            out.plot_files[p as usize].push(f);
            out.file_plot[f as usize] = p;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::Pt;

    /// A grid of `n × n` plots, four-neighbour adjacency, unit spacing.
    fn grid(n: usize) -> (Vec<Pt>, Vec<Vec<u32>>) {
        let mut pts = Vec::new();
        for y in 0..n {
            for x in 0..n {
                pts.push([x as f64, y as f64]);
            }
        }
        let mut adj = vec![Vec::new(); n * n];
        let idx = |x: usize, y: usize| u32::try_from(y * n + x).expect("fits");
        for y in 0..n {
            for x in 0..n {
                if x + 1 < n {
                    adj[y * n + x].push(idx(x + 1, y));
                    adj[y * n + x + 1].push(idx(x, y));
                }
                if y + 1 < n {
                    adj[y * n + x].push(idx(x, y + 1));
                    adj[(y + 1) * n + x].push(idx(x, y));
                }
            }
        }
        (pts, adj)
    }

    /// The property the whole module exists for, on a graph rather than on a
    /// city: both halves of a cut are connected, whatever the balance asked for.
    #[test]
    fn both_halves_of_every_cut_are_connected() {
        let (pts, adj) = grid(9);
        let n = pts.len();
        let nbr: Vec<Vec<(u32, i64)>> = adj
            .iter()
            .enumerate()
            .map(|(a, l)| {
                l.iter()
                    .map(|&b| {
                        (
                            b,
                            ((dist(pts[a], pts[b as usize]) * MILLI).round() as i64).max(1),
                        )
                    })
                    .collect()
            })
            .collect();
        let connected = |set: &[u32]| -> bool {
            if set.is_empty() {
                return true;
            }
            let inside: std::collections::BTreeSet<u32> = set.iter().copied().collect();
            let mut seen = std::collections::BTreeSet::new();
            let mut stack = vec![set[0]];
            seen.insert(set[0]);
            while let Some(u) = stack.pop() {
                for &(v, _) in &nbr[u as usize] {
                    if inside.contains(&v) && seen.insert(v) {
                        stack.push(v);
                    }
                }
            }
            seen.len() == set.len()
        };
        // Exercise the cut directly, at every balance from 1:80 to 80:1.
        for want in 1..n {
            let (a, b) = split_for_test(&pts, &nbr, want);
            assert!(connected(&a), "half A is in pieces at {want}");
            assert!(connected(&b), "half B is in pieces at {want}");
            assert_eq!(a.len() + b.len(), n);
        }
    }

    /// The cut mechanism, lifted out of `Cutter` so the invariant can be tested
    /// without a whole settlement behind it.
    fn split_for_test(pts: &[Pt], nbr: &[Vec<(u32, i64)>], want: usize) -> (Vec<u32>, Vec<u32>) {
        let n = pts.len();
        let sp = |from: usize| -> Vec<i64> {
            let mut d = vec![i64::MAX / 4; n];
            d[from] = 0;
            let mut heap: BinaryHeap<Reverse<(i64, u32)>> = BinaryHeap::new();
            heap.push(Reverse((0, u32::try_from(from).expect("fits"))));
            while let Some(Reverse((c, u))) = heap.pop() {
                if c > d[u as usize] {
                    continue;
                }
                for &(v, w) in &nbr[u as usize] {
                    if c + w < d[v as usize] {
                        d[v as usize] = c + w;
                        heap.push(Reverse((c + w, v)));
                    }
                }
            }
            d
        };
        let da = sp(0);
        let mut seed_b = 0;
        for i in 0..n {
            if da[i] > da[seed_b] {
                seed_b = i;
            }
        }
        let db = sp(seed_b);
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by_key(|&i| (da[i] - db[i], i));
        let mut best = (usize::MAX, 1usize);
        for m in 1..n {
            if da[order[m - 1]] - db[order[m - 1]] == da[order[m]] - db[order[m]] {
                continue;
            }
            let diff = m.abs_diff(want);
            if diff < best.0 {
                best = (diff, m);
            }
        }
        let m = if best.0 == usize::MAX { n / 2 } else { best.1 }.clamp(1, n - 1);
        (
            order[..m].iter().map(|&i| i as u32).collect(),
            order[m..].iter().map(|&i| i as u32).collect(),
        )
    }

    /// The balance the cut can actually reach on a grid: within a couple of
    /// plots of what was asked for, which is what a tie group costs.
    #[test]
    fn the_cut_lands_near_the_balance_it_was_asked_for() {
        let (pts, adj) = grid(9);
        let nbr: Vec<Vec<(u32, i64)>> = adj
            .iter()
            .enumerate()
            .map(|(a, l)| {
                l.iter()
                    .map(|&b| {
                        (
                            b,
                            ((dist(pts[a], pts[b as usize]) * MILLI).round() as i64).max(1),
                        )
                    })
                    .collect()
            })
            .collect();
        for want in [10usize, 27, 40, 55, 70] {
            let (a, _) = split_for_test(&pts, &nbr, want);
            assert!(
                a.len().abs_diff(want) <= 9,
                "asked for {want}, got {}",
                a.len()
            );
        }
    }
}
