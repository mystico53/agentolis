//! Step 3 — blocks (PRD §7.2).
//!
//! > **Blocks** are the closed loops (faces) in the resulting planar road graph.
//!
//! Face extraction, not polygon guessing: the block boundary *is* the road loop,
//! which is why irregular road growth yields irregular blocks for free. Guessing
//! a polygon from nearby nodes gives shapes that do not match the roads drawn
//! over them, and the mismatch is visible immediately.
//!
//! # The algorithm
//!
//! The standard planar-subdivision face walk. At each node, sort the incident
//! half-edges by angle once; then from every unvisited half-edge, repeatedly take
//! the **next half-edge clockwise from the reverse of the one you arrived on**
//! until the walk closes. Each traversal yields one face. A traversal that comes
//! back with negative signed area is an unbounded outer face, and it is
//! discarded.
//!
//! # The four things that break a naive face walk
//!
//! Every one of these happens on a real road graph, and each has its own
//! handling here rather than a guard that hides it:
//!
//! * **The outer face.** Including it turns the whole city into one enormous
//!   block. It is the traversal whose signed area is negative, and [`extract`]
//!   drops it. Note the plural: a graph with *k* connected components has *k*
//!   outer faces, so "exactly one negative face" is true only of a connected
//!   graph and is **not** a safe filter. The filter is *positive area*, not
//!   *not the single most negative one*.
//! * **Dangling edges and degree-1 nodes.** Space colonisation leaves stubs —
//!   a branch that grew towards an attractor that was consumed before the
//!   branch arrived. A stub is traversed twice by the same face, out and back,
//!   which puts a zero-area spike in the boundary. [`prune_spurs`] removes them,
//!   cyclically, so the spike does not survive into a lot.
//! * **Isolated components.** A road fragment that never joined the network has
//!   its own faces and its own outer face. Faces are extracted per component and
//!   labelled with one, because a face of component *A* can *contain* the whole
//!   of component *B* — an annulus, which no simple polygon can represent.
//!   See [`ExtractReport::enclosed_dropped`] for what is done about it.
//! * **Collinear and zero-length segments.** Three snapped nodes on a straight
//!   run give three short boundary edges where there is one long one, and
//!   [`crate::lots`] then splits perpendicular to a fragment instead of to the
//!   real long axis. Collinear runs are collapsed; a zero-length segment has no
//!   direction at all and is dropped before the angle sort ever sees it.
//!
//! # Determinism
//!
//! Faces must be enumerated in a deterministic order or PRD §7.4 is violated in
//! a way that only surfaces as a golden-file diff on somebody else's machine:
//!
//! * Start the walks from half-edges in **index order**, not from a set.
//! * Break angle ties at a node by the neighbour's node index; two segments at
//!   the same angle happen whenever snapping merges a junction, which is
//!   constantly.
//! * Emit each face's boundary starting from its **lowest node index**, so the
//!   same loop serializes identically however the walk entered it.
//! * Sort the faces themselves by that canonical vertex sequence, so the
//!   emission order does not depend on which dart the scan reached first.
//!
//! The angle sort uses [`pseudo_angle`], not `atan2`: [`crate::determinism`]
//! rule 4 forbids a transcendental function on a value that reaches the layout,
//! and a monotone rational function of the true angle is all a *sort* needs.
//!
//! # Districts: the tree decides, never the import graph
//!
//! > **Do not let the import graph fight the directory tree for position.** The
//! > tree determines placement, because directory paths are the addressing
//! > system already in use in every tool call, error message, and conversation.
//! > Imports act only as a weak attraction force *within* a district, and as
//! > drawn edges everywhere else. (PRD §9)
//!
//! That rule is enforced structurally: nothing in this module takes a
//! `polis_repo::imports::ImportGraph`, and [`DistrictSites`] is built from the
//! attractor scatter, which [`crate::roads`] derives from the directory tree and
//! git growth order alone. There is no parameter through which an import edge
//! could move a block, which is a stronger guarantee than a comment.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use polis_events::LogicalPath;
use polis_repo::RepoTree;

use crate::determinism::{debug_assert_canonical_order, narrow};
use crate::roads::Attractor;
use crate::{Block, BlockId, NodeId, Point, Polygon, RoadGraph, Vec2};

// ---------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------

/// Smallest face [`extract`] keeps, in city-space area units.
///
/// Equal to [`crate::lots::MIN_LOT_AREA`] on purpose, and written as that
/// constant so the two cannot drift: a block that cannot hold one minimum lot
/// cannot hold a building, so keeping it only spends [`crate::lots`]'s recursion
/// budget on a parcel nothing will ever stand on.
pub const MIN_BLOCK_AREA: f32 = crate::lots::MIN_LOT_AREA;

/// Perpendicular distance under which three consecutive boundary vertices count
/// as collinear and the middle one is dropped.
///
/// Ten times [`crate::determinism::QUANTUM`], so a run that survives collapse is
/// a bend the serialized layout can actually represent. Roads are grown at a
/// segment length of 10 with a snap tolerance of 2, so 0.01 is far below any
/// intentional geometry.
pub const COLLINEAR_TOLERANCE: f32 = 0.01;

/// Cell size of [`DistrictSites`]'s lookup grid, in city-space units.
///
/// Matched to [`crate::roads::ATTRACTOR_SPACING`], which is the density the
/// scatter places points at, so a typical cell holds a handful of files.
pub const SITE_CELL: f32 = crate::roads::ATTRACTOR_SPACING;

// ---------------------------------------------------------------------------
// Angles without trigonometry
// ---------------------------------------------------------------------------

/// A monotone stand-in for the angle of `v`, in `[-2, 2]`.
///
/// The "diamond angle": `x / (|x| + |y|)` folded into the four quadrants. It is
/// a strictly monotone function of `atan2(y, x)` over `(-π, π]`, which is
/// everything a *sort* needs, and it is built from `abs`, `+` and `/` alone — all
/// correctly rounded by IEEE-754 on every target Polis supports.
/// [`crate::determinism`] rule 4 bans `atan2` on a value that reaches the
/// layout, and the face walk's angle order reaches every block boundary in the
/// city, so this is the load-bearing path, not a micro-optimisation.
///
/// Returns `0` for the zero vector and for any non-finite input, where an angle
/// is meaningless; callers drop those edges before sorting.
///
/// Landmarks: `(1, 0) → 0`, `(0, 1) → 1`, `(-1, 0) → 2`, `(0, -1) → -1`. Pinned
/// with literal values by `pseudo_angle_is_pinned` ([`crate::determinism`]
/// rule 7).
#[must_use]
pub fn pseudo_angle(v: Vec2) -> f64 {
    let x = f64::from(v.x);
    let y = f64::from(v.y);
    let sum = x.abs() + y.abs();
    if !sum.is_finite() || sum <= 0.0 {
        return 0.0;
    }
    let p = x / sum;
    if y < 0.0 {
        p - 1.0
    } else {
        1.0 - p
    }
}

// ---------------------------------------------------------------------------
// The planar half-edge structure
// ---------------------------------------------------------------------------

/// The half-edge (dart) view of a road graph.
///
/// Built once per extraction. A *dart* is one direction of one segment; darts
/// are numbered so that every dart leaving node `n` is contiguous, in
/// counter-clockwise angle order, which is what makes "start the walks in index
/// order" a canonical statement rather than a hopeful one.
struct Planar {
    /// `neighbours[n]` — nodes adjacent to `n`, counter-clockwise by
    /// [`pseudo_angle`], ties broken by node index.
    neighbours: Vec<Vec<NodeId>>,
    /// `base[n]` — dart id of `neighbours[n][0]`.
    base: Vec<u32>,
    /// Source node of each dart.
    from: Vec<u32>,
    /// Target node of each dart.
    to: Vec<u32>,
    /// `twin[d]` — the dart running the other way along the same segment.
    twin: Vec<u32>,
}

impl Planar {
    /// Builds the dart structure, dropping self-loops, duplicate segments,
    /// segments with an out-of-range endpoint, and segments whose two endpoints
    /// coincide (no direction, so no place in an angle sort).
    fn build(graph: &RoadGraph) -> Self {
        let node_count = graph.nodes.len();
        // `BTreeSet` rather than a `Vec` + dedup: `RoadGraph::segments` is a
        // public field, so a caller that bypassed `add_segment` can have put a
        // duplicate or a reversed duplicate in it, and a duplicated dart makes
        // the face walk revisit and terminate early.
        let mut adjacency: Vec<BTreeSet<u32>> = vec![BTreeSet::new(); node_count];
        for segment in &graph.segments {
            let (a, b) = (segment.from.0 as usize, segment.to.0 as usize);
            if a == b || a >= node_count || b >= node_count {
                continue;
            }
            let (pa, pb) = (graph.nodes[a].position, graph.nodes[b].position);
            if !pa.is_finite() || !pb.is_finite() || pa.distance_squared(pb) <= f32::EPSILON {
                continue;
            }
            adjacency[a].insert(segment.to.0);
            adjacency[b].insert(segment.from.0);
        }

        let mut neighbours: Vec<Vec<NodeId>> = Vec::with_capacity(node_count);
        let mut base = Vec::with_capacity(node_count);
        let mut from = Vec::new();
        let mut to = Vec::new();
        for (index, set) in adjacency.iter().enumerate() {
            let here = graph.nodes[index].position;
            // `set` iterates in node-index order, so the pre-sort input is
            // canonical and the stable sort below cannot introduce an ordering
            // that depends on anything but the angles.
            let mut ring: Vec<NodeId> = set.iter().copied().map(NodeId).collect();
            ring.sort_by(|a, b| {
                let ka = pseudo_angle(graph.nodes[a.0 as usize].position - here);
                let kb = pseudo_angle(graph.nodes[b.0 as usize].position - here);
                ka.total_cmp(&kb).then_with(|| a.cmp(b))
            });
            base.push(u32::try_from(from.len()).unwrap_or(u32::MAX));
            for neighbour in &ring {
                from.push(u32::try_from(index).unwrap_or(u32::MAX));
                to.push(neighbour.0);
            }
            neighbours.push(ring);
        }

        let mut planar = Self {
            neighbours,
            base,
            from,
            to,
            twin: Vec::new(),
        };
        let twin: Vec<u32> = (0..planar.from.len())
            .map(|dart| {
                let v = planar.to[dart] as usize;
                let u = NodeId(planar.from[dart]);
                let slot = planar.neighbours[v]
                    .iter()
                    .position(|n| *n == u)
                    .expect("adjacency is symmetric by construction");
                planar.base[v] + u32::try_from(slot).unwrap_or(u32::MAX)
            })
            .collect();
        planar.twin = twin;
        planar
    }

    /// Number of darts — twice the number of surviving segments.
    fn darts(&self) -> usize {
        self.from.len()
    }

    /// The face walk's step: arriving at `v` along `dart`, the next boundary
    /// dart is the one immediately **clockwise** from the reversed dart, which
    /// is the predecessor of the twin in counter-clockwise order.
    ///
    /// Taking the clockwise neighbour is what makes bounded faces come out
    /// counter-clockwise (positive area) and outer faces clockwise; taking the
    /// other one inverts both and quietly keeps the outside as a block.
    fn next_dart(&self, dart: u32) -> u32 {
        let twin = self.twin[dart as usize];
        let v = self.from[twin as usize] as usize;
        let ring = self.neighbours[v].len();
        let slot = (twin - self.base[v]) as usize;
        self.base[v] + u32::try_from((slot + ring - 1) % ring).unwrap_or(0)
    }

    /// Component label per node, assigned by breadth-first search from the
    /// lowest unvisited node index. Nodes with no surviving edge get their own
    /// label, which keeps the labelling total.
    fn components(&self) -> (Vec<u32>, u32) {
        let mut label = vec![u32::MAX; self.neighbours.len()];
        let mut next = 0_u32;
        let mut queue = Vec::new();
        for start in 0..self.neighbours.len() {
            if label[start] != u32::MAX {
                continue;
            }
            label[start] = next;
            queue.clear();
            queue.push(start);
            while let Some(node) = queue.pop() {
                for neighbour in &self.neighbours[node] {
                    let n = neighbour.0 as usize;
                    if label[n] == u32::MAX {
                        label[n] = next;
                        queue.push(n);
                    }
                }
            }
            next += 1;
        }
        (label, next)
    }
}

// ---------------------------------------------------------------------------
// Faces
// ---------------------------------------------------------------------------

/// One traversal of the planar road graph.
///
/// Every dart belongs to exactly one face, so the faces partition the graph's
/// darts and their number is fixed by Euler's formula — which is why a missing
/// or duplicated face is a bug in the walk, never an input.
#[derive(Debug, Clone, PartialEq)]
pub struct Face {
    /// Boundary nodes, spurs pruned, rotated to start at the lowest node index.
    /// Empty for a traversal that pruned away to nothing — a tree component.
    pub nodes: Vec<NodeId>,
    /// The same boundary as geometry, collinear runs collapsed.
    pub boundary: Polygon,
    /// Which connected component of the graph this face belongs to.
    pub component: u32,
    /// Number of darts the traversal consumed, before any pruning. A face whose
    /// dart count exceeds its vertex count was walked over dangling edges.
    pub darts: usize,
}

impl Face {
    /// True when the face encloses positive area — a block candidate.
    #[must_use]
    pub fn is_bounded(&self) -> bool {
        self.boundary.vertices.len() >= 3 && self.boundary.signed_area_x2() > 0.0
    }
}

/// What [`extract`] did, for the caller that wants to know why a block is
/// missing rather than guessing.
///
/// Every count is a real event on a real road graph; none of them is an error.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExtractReport {
    /// Face traversals performed. Equal to the number of faces of the planar
    /// subdivision, including the outer face of every component.
    pub traversals: usize,
    /// Traversals discarded as unbounded — one per connected component that has
    /// at least one edge.
    pub outer_faces: usize,
    /// Traversals that pruned away to fewer than three distinct vertices: a
    /// tree component, or a chain of dangling edges.
    pub degenerate: usize,
    /// Bounded faces dropped because a face of another component contained
    /// them. See [`extract`] for why the *inner* one is the one that goes.
    pub enclosed_dropped: usize,
    /// Bounded faces dropped by [`drop_slivers`].
    pub slivers_dropped: usize,
    /// Connected components of the graph, counting isolated nodes.
    pub components: usize,
    /// Nodes with exactly one surviving edge — the tip of a dangling road.
    pub dangling_nodes: usize,
    /// Nodes with no surviving edge at all.
    pub isolated_nodes: usize,
}

impl ExtractReport {
    /// Blocks that came out, given what went in.
    #[must_use]
    pub fn kept(&self) -> usize {
        self.traversals
            .saturating_sub(self.outer_faces)
            .saturating_sub(self.degenerate)
            .saturating_sub(self.enclosed_dropped)
            .saturating_sub(self.slivers_dropped)
    }
}

/// Every face of the planar road graph, including the unbounded ones.
///
/// The raw traversal, for callers that want the outer boundary of a component or
/// want to count faces against Euler's formula. [`extract`] is what the pipeline
/// calls.
///
/// Faces come back in a canonical order: by boundary node sequence, each
/// sequence rotated to start at its lowest node index. That order is
/// independent of which dart the scan happened to reach first, so it survives a
/// change to the segment order that does not change the geometry.
#[must_use]
pub fn faces(graph: &RoadGraph) -> Vec<Face> {
    let planar = Planar::build(graph);
    let (component, _) = planar.components();
    let dart_count = planar.darts();
    let mut visited = vec![false; dart_count];
    let mut out: Vec<Face> = Vec::new();

    for start in 0..dart_count {
        if visited[start] {
            continue;
        }
        let mut cycle: Vec<NodeId> = Vec::new();
        let mut dart = u32::try_from(start).unwrap_or(u32::MAX);
        // Every dart belongs to exactly one face, so a walk cannot exceed the
        // dart count. The bound is a guard against a malformed graph turning a
        // pipeline stage into a hang, not an expected exit.
        for _ in 0..=dart_count {
            visited[dart as usize] = true;
            cycle.push(NodeId(planar.from[dart as usize]));
            dart = planar.next_dart(dart);
            if dart as usize == start {
                break;
            }
            debug_assert!(!visited[dart as usize], "a dart is in two faces");
        }

        let darts = cycle.len();
        let label = component[cycle[0].0 as usize];
        let nodes = rotate_to_lowest(prune_spurs(&cycle));
        let boundary = collapse_collinear(&Polygon::new(
            nodes
                .iter()
                .map(|n| graph.nodes[n.0 as usize].position)
                .collect(),
        ));
        out.push(Face {
            nodes,
            boundary,
            component: label,
            darts,
        });
    }

    out.sort_by(|a, b| a.nodes.cmp(&b.nodes));
    out
}

/// Extracts every bounded face of the planar road graph.
///
/// The returned blocks are indexed in emission order, and each
/// [`Block::id`] matches its position — a block moved between two runs takes
/// every lot and building on it with it.
///
/// [`Block::district`] is left for [`assign_districts`] to fill, since a face
/// has no idea which directory it belongs to until the districts are placed;
/// until then every block reports [`LogicalPath::root`].
///
/// # What is dropped, and why
///
/// * The **outer face of every component** — not just one. A graph with three
///   fragments has three outer faces, and a filter that removes only the most
///   negative one puts two enormous inside-out blocks in the city.
/// * **Degenerate traversals** — a tree component, or a chain of dangling
///   edges, prunes away to fewer than three vertices and encloses nothing.
/// * **Slivers**, below [`MIN_BLOCK_AREA`]. Snapping makes them routine: two
///   junctions a hair apart enclose a triangle with no room for a lot.
/// * **Faces of a component that another component's face contains.** This is
///   the annulus case, which no simple [`Polygon`] can represent. The *inner*
///   component's faces are the ones dropped, not the enclosing face: the
///   enclosing face is a genuine bounded region of the road network and is
///   almost always far larger, whereas the enclosed fragment is a stray loop
///   that failed to join the network. Dropping the big one instead would put a
///   hole in the city to protect a curiosity. The count is reported in
///   [`ExtractReport::enclosed_dropped`] so it is visible rather than silent.
#[must_use]
pub fn extract(graph: &RoadGraph) -> Vec<Block> {
    extract_with_report(graph).0
}

/// [`extract`], plus a count of everything it discarded.
#[must_use]
pub fn extract_with_report(graph: &RoadGraph) -> (Vec<Block>, ExtractReport) {
    let all = faces(graph);
    let planar = Planar::build(graph);
    let (component_of, component_count) = planar.components();

    let mut report = ExtractReport {
        traversals: all.len(),
        components: component_count as usize,
        ..ExtractReport::default()
    };
    for ring in &planar.neighbours {
        match ring.len() {
            0 => report.isolated_nodes += 1,
            1 => report.dangling_nodes += 1,
            _ => {}
        }
    }

    let mut bounded: Vec<Face> = Vec::with_capacity(all.len());
    for face in all {
        if face.boundary.vertices.len() < 3 {
            report.degenerate += 1;
        } else if is_outer_face(&face.boundary) {
            report.outer_faces += 1;
        } else {
            bounded.push(face);
        }
    }

    if component_count > 1 {
        let enclosed = enclosed_components(graph, &component_of, component_count, &bounded);
        if !enclosed.is_empty() {
            let before = bounded.len();
            bounded.retain(|face| !enclosed.contains(&face.component));
            report.enclosed_dropped = before - bounded.len();
        }
    }

    let mut blocks: Vec<Block> = bounded
        .into_iter()
        .enumerate()
        .map(|(index, face)| Block {
            id: BlockId(u32::try_from(index).unwrap_or(u32::MAX)),
            boundary: face.boundary,
            district: LogicalPath::root(),
        })
        .collect();
    report.slivers_dropped = drop_slivers(&mut blocks, MIN_BLOCK_AREA);
    (blocks, report)
}

/// Components whose every face is swallowed by a bounded face of another
/// component.
///
/// A planar graph's faces partition the plane, so a point not on component `c`
/// lies in exactly one face of every other component. Testing one representative
/// point per component is therefore enough, and the components are disjoint and
/// non-crossing (PRD §7.2 planarises the graph), so a component with one point
/// inside a face is wholly inside it.
fn enclosed_components(
    graph: &RoadGraph,
    component_of: &[u32],
    component_count: u32,
    bounded: &[Face],
) -> BTreeSet<u32> {
    // Lowest-index node of each component: deterministic, and it is the node the
    // breadth-first search started from.
    let mut representative: BTreeMap<u32, Point> = BTreeMap::new();
    for (index, component) in component_of.iter().enumerate() {
        representative
            .entry(*component)
            .or_insert_with(|| graph.nodes[index].position);
    }
    debug_assert!(representative.len() <= component_count as usize);

    let mut enclosed = BTreeSet::new();
    for face in bounded {
        let Some((lo, hi)) = face.boundary.bounds() else {
            continue;
        };
        for (component, point) in &representative {
            if *component == face.component || enclosed.contains(component) {
                continue;
            }
            if point.x < lo.x || point.x > hi.x || point.y < lo.y || point.y > hi.y {
                continue;
            }
            if face.boundary.contains(*point) {
                enclosed.insert(*component);
            }
        }
    }
    enclosed
}

/// The half-edges leaving a node, sorted counter-clockwise by angle.
///
/// Exposed because it is the piece of the face walk that is worth testing on its
/// own: a wrong sort produces faces that look plausible and enclose the wrong
/// area, which is the hardest failure here to spot by eye.
///
/// Ties break by neighbour node index. Two segments at the same angle from the
/// same node means two neighbours on the same ray, which snapping produces
/// whenever a junction merges. An unbroken tie there is a coin flip that decides
/// which of two faces a whole district ends up in.
///
/// The order is *increasing angle over `(-π, π]`*, so the list begins just
/// clockwise of the negative x axis and ends on it: `(0, -1)`, `(1, 0)`,
/// `(0, 1)`, `(-1, 0)`. Only the **cyclic** order matters to the face walk —
/// where the list is cut is arbitrary and fixed, not meaningful.
///
/// Self-loops, duplicate segments, out-of-range endpoints and zero-length
/// segments are all absent from the result: none of them has a usable direction,
/// and each is a normal artefact of a public `segments` vector.
#[must_use]
pub fn sorted_incident(graph: &RoadGraph, node: NodeId) -> Vec<NodeId> {
    let index = node.0 as usize;
    if index >= graph.nodes.len() {
        return Vec::new();
    }
    let mut planar = Planar::build(graph);
    std::mem::take(&mut planar.neighbours[index])
}

/// True for the unbounded outer face — the one traversal whose signed area is
/// negative.
///
/// Every planar face walk produces one **per connected component**, and
/// including any of them turns that component into a single enormous block.
///
/// Also true for a traversal that encloses no area at all — a walk out and back
/// along a chain of dangling edges — and for a boundary whose area is not a
/// number. Neither is a block, and a caller using this as its only filter is
/// better served by a predicate that says so than by one that is honest about
/// the sign and silently lets a degenerate face through. [`Face::is_bounded`] is
/// the same test from the other side.
#[must_use]
pub fn is_outer_face(boundary: &Polygon) -> bool {
    // "Not strictly positive", spelled so that `NaN` — which compares false
    // against everything, including in a negated test — lands on the safe side.
    boundary.signed_area_x2().partial_cmp(&0.0) != Some(Ordering::Greater)
}

/// Drops faces below a minimum area.
///
/// Snapping produces slivers — two junctions a hair apart enclose a triangle
/// with no room for a lot. They are geometry, not city, and subdividing them
/// wastes the whole lot budget on invisible parcels.
///
/// Survivors are renumbered so [`Block::id`] still matches the index; returns
/// how many were dropped.
pub fn drop_slivers(blocks: &mut Vec<Block>, min_area: f32) -> usize {
    let before = blocks.len();
    let floor = if min_area.is_finite() {
        min_area
    } else {
        MIN_BLOCK_AREA
    };
    blocks.retain(|block| block.boundary.is_valid() && block.boundary.area() >= floor);
    renumber(blocks);
    before - blocks.len()
}

/// Rewrites every [`Block::id`] to match its index.
///
/// The invariant [`extract`] promises, restated as an operation, because any
/// filtering or concatenation breaks it and a block whose id does not match its
/// index silently hands its lots to a different block.
pub fn renumber(blocks: &mut [Block]) {
    for (index, block) in blocks.iter_mut().enumerate() {
        block.id = BlockId(u32::try_from(index).unwrap_or(u32::MAX));
    }
}

// ---------------------------------------------------------------------------
// Boundary cleanup
// ---------------------------------------------------------------------------

/// Removes out-and-back spurs from a face's node cycle, cyclically.
///
/// A dangling road is traversed twice by the face that contains it, out and
/// back, which puts `… a b a …` in the cycle. The spur encloses no area, so the
/// signed area is right either way, but the zero-width spike survives into the
/// lot subdivision and into the renderer, where it is a visible artefact.
///
/// The cyclic part matters: the spur can straddle the seam, in which case the
/// tip is the *first* element and its two neighbours are the last element and
/// the second. A linear pass alone leaves that one in place.
#[must_use]
pub fn prune_spurs(cycle: &[NodeId]) -> Vec<NodeId> {
    let mut out: Vec<NodeId> = Vec::with_capacity(cycle.len());
    for node in cycle {
        if out.len() >= 2 && out[out.len() - 2] == *node {
            out.pop();
        } else {
            out.push(*node);
        }
    }
    // Bounded by the length: each pass removes at least one element.
    for _ in 0..cycle.len() {
        let len = out.len();
        if len < 3 {
            break;
        }
        if out[len - 1] == out[1] {
            // `out[0]` is a spur tip straddling the seam.
            out.remove(0);
            out.pop();
        } else if out[len - 1] == out[0] {
            out.pop();
        } else {
            break;
        }
    }
    if out.len() < 3 {
        out.clear();
    }
    out
}

/// Rotates a node cycle to start at its lowest node index.
///
/// The same loop entered from two different darts is the same block, and it must
/// serialize identically or PRD §16's golden file moves for no reason.
fn rotate_to_lowest(cycle: Vec<NodeId>) -> Vec<NodeId> {
    let Some(pivot) = (0..cycle.len()).min_by_key(|i| cycle[*i]) else {
        return cycle;
    };
    let mut out = Vec::with_capacity(cycle.len());
    out.extend_from_slice(&cycle[pivot..]);
    out.extend_from_slice(&cycle[..pivot]);
    out
}

/// Collapses runs of collinear vertices, cyclically.
///
/// Three snapped nodes on a straight run are three boundary edges where the eye
/// and [`crate::lots`] both see one. [`Polygon::longest_edge`] is the
/// subdivision axis, so a boundary carrying fragments of its own long side
/// splits perpendicular to a fragment and produces lots at right angles to the
/// ones next door.
///
/// The test is perpendicular distance, not a cross-product threshold, so it is
/// scale-free: a vertex is dropped when it lies within
/// [`COLLINEAR_TOLERANCE`] of the line through its neighbours.
#[must_use]
pub fn collapse_collinear(polygon: &Polygon) -> Polygon {
    let mut points = polygon.vertices.clone();
    for _ in 0..polygon.vertices.len() {
        if points.len() < 3 {
            break;
        }
        let len = points.len();
        let mut kept: Vec<Point> = Vec::with_capacity(len);
        for i in 0..len {
            let previous = *kept.last().unwrap_or(&points[(i + len - 1) % len]);
            let here = points[i];
            let next = points[(i + 1) % len];
            // Never drop below a triangle: `kept` so far, plus everything after
            // `i` that is still to be considered.
            if kept.len() + (len - i - 1) > 2 && is_collinear(previous, here, next) {
                continue;
            }
            kept.push(here);
        }
        let stable = kept.len() == points.len();
        points = kept;
        if stable {
            break;
        }
    }
    if points.len() < 3 {
        points.clear();
    }
    Polygon::new(points)
}

/// True when `here` lies within [`COLLINEAR_TOLERANCE`] of the line through
/// `previous` and `next`, computed in `f64`
/// ([`crate::determinism`] rules 3 and 5 — written out, never `mul_add`).
fn is_collinear(previous: Point, here: Point, next: Point) -> bool {
    let ax = f64::from(here.x) - f64::from(previous.x);
    let ay = f64::from(here.y) - f64::from(previous.y);
    let bx = f64::from(next.x) - f64::from(here.x);
    let by = f64::from(next.y) - f64::from(here.y);
    let cross = ax * by - ay * bx;
    let span = (ax * ax + ay * ay).sqrt().max((bx * bx + by * by).sqrt());
    if span <= 0.0 {
        // Two coincident vertices: the middle one carries nothing.
        return true;
    }
    (cross / span).abs() <= f64::from(COLLINEAR_TOLERANCE)
}

// ---------------------------------------------------------------------------
// Districts (PRD §9)
// ---------------------------------------------------------------------------

/// The district a file belongs to — its containing directory, the repository
/// root for a file at top level.
///
/// The one definition of "district" the layout uses. PRD §3: a district is a
/// directory; PRD §9: the directory tree, and nothing else, decides where a file
/// goes.
#[must_use]
pub fn district_of(path: &LogicalPath) -> LogicalPath {
    path.parent().unwrap_or_else(LogicalPath::root)
}

/// Where each district's files sit in city space.
///
/// Built from the attractor scatter — which [`crate::roads`] derives from the
/// directory tree and git growth order and nothing else — so assigning a block
/// to the district of its nearest file is a purely tree-driven placement, as
/// PRD §9 requires. There is deliberately no constructor that takes an import
/// graph.
#[derive(Debug, Clone, Default)]
pub struct DistrictSites {
    /// Districts, sorted and unique. Indices into this are the owner ids.
    districts: Vec<LogicalPath>,
    /// Mean site position per district, parallel to [`Self::districts`].
    centres: Vec<Point>,
    /// One site per file, in canonical `(district, x, y)` order.
    points: Vec<Point>,
    /// `owner[i]` — index into [`Self::districts`] for `points[i]`.
    owner: Vec<u32>,
    /// Point indices per grid cell, for the nearest-site query.
    grid: BTreeMap<(i32, i32), Vec<u32>>,
    /// Cell size of [`Self::grid`].
    cell: f32,
}

/// Widest Chebyshev cell radius [`DistrictSites::nearest`] will search before
/// giving up on the grid and scanning every site.
///
/// A grid walk costs `(2r + 1)²` map probes, so an unbounded radius turns a
/// query far from every file — which a caller is entitled to make — into
/// millions of lookups to avoid one linear pass. Eight rings is 289 probes and
/// covers every query that lands anywhere near the road network, which is where
/// every block centroid is by construction.
const MAX_GRID_RADIUS: i32 = 8;

impl DistrictSites {
    /// Sites from arbitrary `(district, position)` pairs.
    ///
    /// The input is sorted into a canonical order before anything else happens,
    /// so a caller that hands over a `HashMap`'s iteration order gets the same
    /// answer as one that hands over a sorted `Vec` — PRD §7.4, enforced here
    /// rather than trusted upstream.
    #[must_use]
    pub fn from_points(points: impl IntoIterator<Item = (LogicalPath, Point)>) -> Self {
        let mut pairs: Vec<(LogicalPath, Point)> =
            points.into_iter().filter(|(_, p)| p.is_finite()).collect();
        pairs.sort_by(|(pa, a), (pb, b)| {
            pa.cmp(pb)
                .then_with(|| a.x.total_cmp(&b.x))
                .then_with(|| a.y.total_cmp(&b.y))
        });

        let mut districts: Vec<LogicalPath> = Vec::new();
        let mut owner = Vec::with_capacity(pairs.len());
        let mut coordinates = Vec::with_capacity(pairs.len());
        for (district, point) in pairs {
            if districts.last() != Some(&district) {
                districts.push(district);
            }
            owner.push(u32::try_from(districts.len() - 1).unwrap_or(u32::MAX));
            coordinates.push(point);
        }
        debug_assert_canonical_order(districts.iter(), "DistrictSites::districts");

        let cell = if SITE_CELL > 0.0 { SITE_CELL } else { 1.0 };
        let mut grid: BTreeMap<(i32, i32), Vec<u32>> = BTreeMap::new();
        for (index, point) in coordinates.iter().enumerate() {
            grid.entry(cell_of(*point, cell))
                .or_default()
                .push(u32::try_from(index).unwrap_or(u32::MAX));
        }

        // Centres are summed in canonical site order, so the mean is
        // bit-identical however the caller ordered the input.
        let mut sums = vec![(0.0_f64, 0.0_f64, 0_u32); districts.len()];
        for (index, owner) in owner.iter().enumerate() {
            let slot = &mut sums[*owner as usize];
            slot.0 += f64::from(coordinates[index].x);
            slot.1 += f64::from(coordinates[index].y);
            slot.2 += 1;
        }
        let centres = sums
            .into_iter()
            .map(|(x, y, count)| {
                let n = f64::from(count.max(1));
                Point::new(narrow(x / n), narrow(y / n))
            })
            .collect();

        Self {
            districts,
            centres,
            points: coordinates,
            owner,
            grid,
            cell,
        }
    }

    /// Sites from a repository tree and the scatter [`crate::roads`] grew from
    /// it.
    ///
    /// `attractors` must be `roads::scatter_attractors(tree, extent)` for the
    /// same tree: it emits one attractor per file in git growth order, and this
    /// pairs them back up with the files in that same order. A shorter slice
    /// leaves the tail of the tree without sites, which is visible as districts
    /// with no blocks rather than as a panic.
    #[must_use]
    pub fn from_scatter(tree: &RepoTree, attractors: &[Attractor]) -> Self {
        let ordered = files_in_growth_order(tree);
        debug_assert_eq!(
            ordered.len(),
            attractors.len(),
            "scatter_attractors emits one attractor per file, in growth order"
        );
        Self::from_points(
            ordered
                .into_iter()
                .zip(attractors)
                .map(|(path, attractor)| (district_of(path), attractor.at)),
        )
    }

    /// Districts with at least one site, in path order.
    #[must_use]
    pub fn districts(&self) -> &[LogicalPath] {
        &self.districts
    }

    /// Number of sites — one per file.
    #[must_use]
    pub fn len(&self) -> usize {
        self.points.len()
    }

    /// True when nothing was placed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    /// The district whose nearest file sits closest to `at`.
    ///
    /// Ties break towards the lower canonical site index, which is
    /// `(district path, x, y)` order — so two files at the same point in two
    /// districts always resolve to the same one.
    #[must_use]
    pub fn district_at(&self, at: Point) -> Option<&LogicalPath> {
        let index = self.nearest(at)?;
        self.districts.get(self.owner[index as usize] as usize)
    }

    /// Mean of a district's site positions — a label anchor and the centre
    /// [`crate::District`] wants.
    #[must_use]
    pub fn centre(&self, district: &LogicalPath) -> Option<Point> {
        let id = self
            .districts
            .iter()
            .position(|candidate| candidate == district)?;
        self.centres.get(id).copied()
    }

    /// Index of the site nearest `at`.
    ///
    /// Doubling Chebyshev rings over the grid. After scanning radius `r`, every
    /// point within `r * cell` of `at` has been seen — `at` lies inside its own
    /// cell, so a point that close cannot be more than `r` cells away on either
    /// axis. The answer is accepted only once the best distance is inside that
    /// guarantee, which is what keeps the grid an index rather than an
    /// approximation; past [`MAX_GRID_RADIUS`] a full scan is cheaper than the
    /// probes that would certify it.
    fn nearest(&self, at: Point) -> Option<u32> {
        if self.points.is_empty() {
            return None;
        }
        if at.is_finite() {
            let (cx, cy) = cell_of(at, self.cell);
            let mut radius = 1_i32;
            while radius <= MAX_GRID_RADIUS {
                let mut best: Option<(u32, f32)> = None;
                for y in cy - radius..=cy + radius {
                    for x in cx - radius..=cx + radius {
                        let Some(bucket) = self.grid.get(&(x, y)) else {
                            continue;
                        };
                        for index in bucket {
                            let d2 = self.points[*index as usize].distance_squared(at);
                            best = closer(best, *index, d2);
                        }
                    }
                }
                if let Some((index, d2)) = best {
                    let reach = f64::from(radius) * f64::from(self.cell);
                    if f64::from(d2) <= reach * reach {
                        return Some(index);
                    }
                }
                radius = radius.saturating_mul(2);
            }
        }
        // Far from every cell, or a non-finite query. A full scan always
        // answers, with the same tie rule, so the result is the same function
        // of the input either way.
        let mut best: Option<(u32, f32)> = None;
        for (index, point) in self.points.iter().enumerate() {
            let d2 = point.distance_squared(at);
            best = closer(best, u32::try_from(index).unwrap_or(u32::MAX), d2);
        }
        best.map(|(index, _)| index)
    }
}

/// Keeps the nearer of two sites, ties resolved towards the lower canonical
/// index.
///
/// [`f32::total_cmp`] rather than `<`: a `NaN` distance — which a caller
/// produces simply by asking about a `NaN` point — compares false against
/// everything, so a `<` chain silently returns whichever candidate happened to
/// be first. Under a total order the `NaN` sorts above every real distance and
/// the tie rule still picks the lowest index, which is an answer rather than a
/// coin flip.
fn closer(best: Option<(u32, f32)>, index: u32, d2: f32) -> Option<(u32, f32)> {
    match best {
        None => Some((index, d2)),
        Some((best_index, best_d2)) => match d2.total_cmp(&best_d2) {
            Ordering::Less => Some((index, d2)),
            Ordering::Equal if index < best_index => Some((index, d2)),
            _ => best,
        },
    }
}

/// Grid cell of a point, clamped so a huge coordinate cannot wrap the key.
fn cell_of(point: Point, cell: f32) -> (i32, i32) {
    #[allow(clippy::cast_possible_truncation)] // clamped into i32 range first
    fn axis(value: f32, cell: f32) -> i32 {
        if !value.is_finite() {
            return 0;
        }
        (value / cell).floor().clamp(-1.0e9, 1.0e9) as i32
    }
    (axis(point.x, cell), axis(point.y, cell))
}

/// The files of a tree in git growth order (PRD §7.1), the order
/// [`crate::roads::scatter_attractors`] emits attractors in.
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

/// Assigns every block to the district whose files are nearest to it.
///
/// > The tree determines placement. (PRD §9)
///
/// The block's area centroid picks the district, not a vote over its corners:
/// corners are road nodes, which sit *between* districts by construction, so a
/// corner vote systematically pulls blocks towards whichever neighbour happens
/// to have more junctions on the shared boundary. [`Polygon::centroid`] falls
/// back to the vertex average for a degenerate polygon and never returns `NaN`,
/// so there is no unplaced case to handle.
///
/// Returns how many blocks were assigned. Zero when `sites` is empty — a
/// repository with no files — in which case every block keeps
/// [`LogicalPath::root`].
pub fn assign_districts(blocks: &mut [Block], sites: &DistrictSites) -> usize {
    if sites.is_empty() {
        return 0;
    }
    let mut assigned = 0;
    for block in blocks.iter_mut() {
        if let Some(district) = sites.district_at(block.boundary.centroid()) {
            block.district = district.clone();
            assigned += 1;
        }
    }
    assigned
}

/// Blocks grouped by district, in path order, each list in block-index order.
///
/// What [`crate::lots`] needs to size a district's subdivision, and what
/// [`crate::District::blocks`] is filled from.
#[must_use]
pub fn group_by_district(blocks: &[Block]) -> BTreeMap<LogicalPath, Vec<BlockId>> {
    let mut out: BTreeMap<LogicalPath, Vec<BlockId>> = BTreeMap::new();
    for block in blocks {
        out.entry(block.district.clone())
            .or_default()
            .push(block.id);
    }
    out
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
    use crate::roads::{grow, scatter_attractors, suggested_extent, GrowthParams};
    use crate::terrain::TerrainField;
    use crate::RoadClass;

    fn lp(text: &str) -> LogicalPath {
        LogicalPath::new(text).expect("valid logical path")
    }

    /// A graph built from explicit positions and index pairs, with snapping
    /// switched off so the fixture is exactly what it says it is.
    fn graph_of(points: &[(f32, f32)], edges: &[(u32, u32)]) -> RoadGraph {
        let mut graph = RoadGraph::new(0.0);
        graph.nodes = points
            .iter()
            .map(|(x, y)| crate::RoadNode {
                position: Point::new(*x, *y),
            })
            .collect();
        for (a, b) in edges {
            graph.add_segment(NodeId(*a), NodeId(*b), RoadClass::Street);
        }
        graph
    }

    /// One unit square, the simplest thing with a face.
    fn square() -> RoadGraph {
        graph_of(
            &[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)],
            &[(0, 1), (1, 2), (2, 3), (3, 0)],
        )
    }

    // -----------------------------------------------------------------------
    // The angle order
    // -----------------------------------------------------------------------

    /// [`crate::determinism`] rule 7: a function whose output reaches the layout
    /// gets literal expected values. The face walk's whole correctness is this
    /// ordering.
    #[test]
    fn pseudo_angle_is_pinned() {
        assert_eq!(pseudo_angle(Vec2::new(1.0, 0.0)), 0.0);
        assert_eq!(pseudo_angle(Vec2::new(1.0, 1.0)), 0.5);
        assert_eq!(pseudo_angle(Vec2::new(0.0, 1.0)), 1.0);
        assert_eq!(pseudo_angle(Vec2::new(-1.0, 1.0)), 1.5);
        assert_eq!(pseudo_angle(Vec2::new(-1.0, 0.0)), 2.0);
        assert_eq!(pseudo_angle(Vec2::new(-1.0, -1.0)), -1.5);
        assert_eq!(pseudo_angle(Vec2::new(0.0, -1.0)), -1.0);
        assert_eq!(pseudo_angle(Vec2::new(1.0, -1.0)), -0.5);
        // Degenerate inputs are total, not panics or `NaN`.
        assert_eq!(pseudo_angle(Vec2::ZERO), 0.0);
        assert_eq!(pseudo_angle(Vec2::new(f32::NAN, 1.0)), 0.0);
        assert_eq!(pseudo_angle(Vec2::new(f32::INFINITY, 0.0)), 0.0);
    }

    #[test]
    fn pseudo_angle_orders_like_atan2() {
        let mut previous = f64::NEG_INFINITY;
        for step in 0..720 {
            let radians =
                f64::from(step) * std::f64::consts::TAU / 720.0 - std::f64::consts::PI + 1.0e-4;
            #[allow(clippy::cast_possible_truncation)] // fixture only
            let v = Vec2::new(radians.cos() as f32, radians.sin() as f32);
            let key = pseudo_angle(v);
            assert!(
                key > previous,
                "not monotone at {radians}: {key} <= {previous}"
            );
            previous = key;
        }
    }

    #[test]
    fn incident_edges_come_back_counter_clockwise() {
        // A hub with four spokes, added in a deliberately scrambled order.
        let graph = graph_of(
            &[
                (0.0, 0.0),
                (0.0, -10.0),
                (-10.0, 0.0),
                (10.0, 0.0),
                (0.0, 10.0),
            ],
            &[(0, 1), (0, 2), (0, 3), (0, 4)],
        );
        // Increasing angle over `(-π, π]`: south, east, north, west. Only the
        // cyclic order is load-bearing; the cut point is fixed, not meaningful.
        assert_eq!(
            sorted_incident(&graph, NodeId(0)),
            vec![NodeId(1), NodeId(3), NodeId(4), NodeId(2)]
        );
        assert_eq!(sorted_incident(&graph, NodeId(3)), vec![NodeId(0)]);
        assert_eq!(sorted_incident(&graph, NodeId(99)), Vec::new());
    }

    /// Snapping merges junctions constantly, and a merged junction is exactly
    /// where two neighbours end up on the same ray.
    #[test]
    fn a_tie_in_angle_breaks_to_the_lower_node_index() {
        let graph = graph_of(&[(0.0, 0.0), (20.0, 0.0), (10.0, 0.0)], &[(0, 1), (0, 2)]);
        assert_eq!(
            sorted_incident(&graph, NodeId(0)),
            vec![NodeId(1), NodeId(2)],
            "identical angles resolve by node index, not by segment order"
        );
    }

    // -----------------------------------------------------------------------
    // Faces on hand-built graphs with known answers
    // -----------------------------------------------------------------------

    #[test]
    fn a_square_has_one_block_and_one_outer_face() {
        let (blocks, report) = extract_with_report(&square());
        assert_eq!(report.traversals, 2, "one inside, one outside");
        assert_eq!(report.outer_faces, 1);
        assert_eq!(report.components, 1);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].id, BlockId(0));
        assert_eq!(blocks[0].boundary.area(), 100.0);
        assert!(
            blocks[0].boundary.is_ccw(),
            "a block is wound counter-clockwise so subdivision is orientation-independent"
        );
        assert_eq!(report.kept(), 1);
    }

    #[test]
    fn the_outer_face_is_excluded() {
        let all = faces(&square());
        assert_eq!(all.len(), 2);
        let outer: Vec<&Face> = all.iter().filter(|f| is_outer_face(&f.boundary)).collect();
        assert_eq!(outer.len(), 1);
        assert!(outer[0].boundary.signed_area_x2() < 0.0);
        assert_eq!(outer[0].boundary.area(), 100.0, "same loop, other winding");
        // And it is not in the blocks.
        assert!(extract(&square())
            .iter()
            .all(|b| !is_outer_face(&b.boundary)));
    }

    /// Two squares sharing one corner node: the classic figure-eight, and the
    /// case a walk that takes the wrong neighbour merges into one face.
    #[test]
    fn a_figure_eight_has_two_blocks() {
        let graph = graph_of(
            &[
                (0.0, 0.0),
                (10.0, 0.0),
                (10.0, 10.0),
                (0.0, 10.0),
                (20.0, 10.0),
                (20.0, 20.0),
                (10.0, 20.0),
            ],
            &[
                (0, 1),
                (1, 2),
                (2, 3),
                (3, 0),
                (2, 4),
                (4, 5),
                (5, 6),
                (6, 2),
            ],
        );
        let (blocks, report) = extract_with_report(&graph);
        assert_eq!(report.components, 1, "the shared node joins them");
        assert_eq!(report.outer_faces, 1);
        assert_eq!(blocks.len(), 2);
        for block in &blocks {
            assert_eq!(block.boundary.area(), 100.0);
            assert_eq!(block.boundary.len(), 4);
            assert!(block.boundary.is_ccw());
        }
        // Distinct blocks, not the same face twice.
        assert_ne!(blocks[0].boundary, blocks[1].boundary);
    }

    /// A square inside a square, sharing nothing. Two components, and the outer
    /// component's inner face is an annulus that no simple polygon can express.
    #[test]
    fn nested_disjoint_loops_keep_the_enclosing_face_and_drop_the_enclosed_one() {
        let graph = graph_of(
            &[
                (0.0, 0.0),
                (100.0, 0.0),
                (100.0, 100.0),
                (0.0, 100.0),
                (40.0, 40.0),
                (60.0, 40.0),
                (60.0, 60.0),
                (40.0, 60.0),
            ],
            &[
                (0, 1),
                (1, 2),
                (2, 3),
                (3, 0),
                (4, 5),
                (5, 6),
                (6, 7),
                (7, 4),
            ],
        );
        let (blocks, report) = extract_with_report(&graph);
        assert_eq!(report.components, 2);
        assert_eq!(report.outer_faces, 2, "one per component, not one overall");
        assert_eq!(report.enclosed_dropped, 1);
        assert_eq!(blocks.len(), 1);
        assert_eq!(
            blocks[0].boundary.area(),
            10_000.0,
            "the big face survives; a hole in the city would be worse than a stray loop inside it"
        );
    }

    /// The same two loops side by side are two independent components and two
    /// perfectly good blocks — the enclosure rule must not fire on those.
    #[test]
    fn disjoint_side_by_side_loops_are_both_blocks() {
        let graph = graph_of(
            &[
                (0.0, 0.0),
                (10.0, 0.0),
                (10.0, 10.0),
                (0.0, 10.0),
                (30.0, 0.0),
                (40.0, 0.0),
                (40.0, 10.0),
                (30.0, 10.0),
            ],
            &[
                (0, 1),
                (1, 2),
                (2, 3),
                (3, 0),
                (4, 5),
                (5, 6),
                (6, 7),
                (7, 4),
            ],
        );
        let (blocks, report) = extract_with_report(&graph);
        assert_eq!(report.components, 2);
        assert_eq!(report.outer_faces, 2);
        assert_eq!(report.enclosed_dropped, 0);
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn a_dangling_edge_does_not_reach_the_block_boundary() {
        // The square, plus a stub growing out of node 1 and a second stub off
        // the stub — the shape space colonisation leaves behind.
        let graph = graph_of(
            &[
                (0.0, 0.0),
                (10.0, 0.0),
                (10.0, 10.0),
                (0.0, 10.0),
                (18.0, 0.0),
                (24.0, 4.0),
            ],
            &[(0, 1), (1, 2), (2, 3), (3, 0), (1, 4), (4, 5)],
        );
        let (blocks, report) = extract_with_report(&graph);
        assert_eq!(report.dangling_nodes, 1, "only the far tip has degree one");
        assert_eq!(blocks.len(), 1);
        assert_eq!(
            blocks[0].boundary.len(),
            4,
            "the stub is walked twice and pruned away, not left as a spike"
        );
        assert_eq!(blocks[0].boundary.area(), 100.0);
    }

    #[test]
    fn a_tree_has_no_blocks_at_all() {
        let graph = graph_of(
            &[(0.0, 0.0), (10.0, 0.0), (20.0, 6.0), (20.0, -6.0)],
            &[(0, 1), (1, 2), (1, 3)],
        );
        let (blocks, report) = extract_with_report(&graph);
        assert!(blocks.is_empty(), "a tree encloses nothing");
        assert_eq!(report.traversals, 1);
        assert_eq!(report.degenerate, 1);
        assert_eq!(report.dangling_nodes, 3);
    }

    #[test]
    fn isolated_nodes_and_an_empty_graph_are_not_a_panic() {
        let mut graph = RoadGraph::new(0.0);
        graph.nodes = vec![
            crate::RoadNode {
                position: Point::ORIGIN,
            },
            crate::RoadNode {
                position: Point::new(5.0, 5.0),
            },
        ];
        let (blocks, report) = extract_with_report(&graph);
        assert!(blocks.is_empty());
        assert_eq!(report.isolated_nodes, 2);
        assert_eq!(report.components, 2);
        assert_eq!(report.traversals, 0);

        let (empty, report) = extract_with_report(&RoadGraph::new(2.0));
        assert!(empty.is_empty());
        assert_eq!(report, ExtractReport::default());
        assert!(faces(&RoadGraph::new(2.0)).is_empty());
    }

    #[test]
    fn a_malformed_segment_vector_survives() {
        let mut graph = square();
        // A self-loop, a duplicate, a reversed duplicate, an out-of-range
        // endpoint and a zero-length segment: all reachable through the public
        // `segments` field, none of them a face.
        graph.segments.push(crate::RoadSegment {
            from: NodeId(0),
            to: NodeId(0),
            class: RoadClass::Alley,
        });
        graph.segments.push(crate::RoadSegment {
            from: NodeId(0),
            to: NodeId(1),
            class: RoadClass::Alley,
        });
        graph.segments.push(crate::RoadSegment {
            from: NodeId(1),
            to: NodeId(0),
            class: RoadClass::Alley,
        });
        graph.segments.push(crate::RoadSegment {
            from: NodeId(0),
            to: NodeId(77),
            class: RoadClass::Alley,
        });
        graph.nodes.push(crate::RoadNode {
            position: Point::ORIGIN,
        });
        graph.segments.push(crate::RoadSegment {
            from: NodeId(0),
            to: NodeId(4),
            class: RoadClass::Alley,
        });
        let blocks = extract(&graph);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].boundary.area(), 100.0);
    }

    #[test]
    fn collinear_runs_collapse_to_one_edge() {
        // A square whose bottom side is split into four collinear nodes.
        let graph = graph_of(
            &[
                (0.0, 0.0),
                (3.0, 0.0),
                (7.0, 0.0),
                (10.0, 0.0),
                (10.0, 10.0),
                (0.0, 10.0),
            ],
            &[(0, 1), (1, 2), (2, 3), (3, 4), (4, 5), (5, 0)],
        );
        let blocks = extract(&graph);
        assert_eq!(blocks.len(), 1);
        assert_eq!(
            blocks[0].boundary.len(),
            4,
            "lots split perpendicular to the longest edge; a fragmented side hides it"
        );
        assert_eq!(blocks[0].boundary.area(), 100.0);
        let (index, length) = blocks[0].boundary.longest_edge().expect("an edge");
        assert_eq!(length, 10.0);
        assert!(index < 4);
    }

    #[test]
    fn slivers_are_dropped_and_the_survivors_renumbered() {
        let mut blocks = vec![
            Block {
                id: BlockId(0),
                boundary: Polygon::new(vec![
                    Point::new(0.0, 0.0),
                    Point::new(0.1, 0.0),
                    Point::new(0.0, 0.1),
                ]),
                district: LogicalPath::root(),
            },
            Block {
                id: BlockId(1),
                boundary: Polygon::new(vec![
                    Point::new(0.0, 0.0),
                    Point::new(10.0, 0.0),
                    Point::new(10.0, 10.0),
                ]),
                district: LogicalPath::root(),
            },
        ];
        assert_eq!(drop_slivers(&mut blocks, MIN_BLOCK_AREA), 1);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].id, BlockId(0), "ids track the index or lots move");
    }

    #[test]
    fn spur_pruning_handles_the_seam() {
        let n = |i: u32| NodeId(i);
        // Interior spur.
        assert_eq!(
            prune_spurs(&[n(0), n(1), n(9), n(1), n(2)]),
            vec![n(0), n(1), n(2)]
        );
        // Spur tip is the first element, its neighbours are the last and second.
        assert_eq!(
            prune_spurs(&[n(9), n(0), n(1), n(2), n(0)]),
            vec![n(0), n(1), n(2)]
        );
        // Duplicated seam.
        assert_eq!(
            prune_spurs(&[n(0), n(1), n(2), n(0)]),
            vec![n(0), n(1), n(2)]
        );
        // A tree prunes to nothing.
        assert!(prune_spurs(&[n(0), n(1), n(2), n(1)]).is_empty());
        assert!(prune_spurs(&[]).is_empty());
    }

    // -----------------------------------------------------------------------
    // Determinism
    // -----------------------------------------------------------------------

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

    /// A real grown graph, which is the only input with enough snapped
    /// junctions, stubs and slivers to exercise the walk properly.
    fn grown(districts: usize, per_district: usize) -> (RepoTree, RoadGraph) {
        let tree = synthetic_tree(districts, per_district);
        let extent = suggested_extent(&tree);
        let terrain = TerrainField::generate(fnv1a64(b"blocks test fixture"), extent);
        let attractors = scatter_attractors(&tree, extent);
        let graph = grow(
            &terrain,
            &attractors,
            GrowthParams::default(),
            RoadClass::Street,
        );
        (tree, graph)
    }

    fn digest(blocks: &[Block]) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        let mut eat = |bytes: &[u8]| hash = fnv1a64_chain(hash, bytes);
        for block in blocks {
            eat(&block.id.0.to_le_bytes());
            eat(block.district.as_str().as_bytes());
            for vertex in &block.boundary.vertices {
                eat(&crate::determinism::quantize(vertex.x).to_le_bytes());
                eat(&crate::determinism::quantize(vertex.y).to_le_bytes());
            }
        }
        hash
    }

    fn fnv1a64_chain(mut hash: u64, bytes: &[u8]) -> u64 {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }

    fn reference_digest() -> u64 {
        let (tree, graph) = grown(6, 9);
        let extent = suggested_extent(&tree);
        let mut blocks = extract(&graph);
        let sites = DistrictSites::from_scatter(&tree, &scatter_attractors(&tree, extent));
        assign_districts(&mut blocks, &sites);
        digest(&blocks)
    }

    #[test]
    fn a_grown_graph_yields_real_blocks() {
        let (_, graph) = grown(6, 9);
        let (blocks, report) = extract_with_report(&graph);
        assert!(
            blocks.len() > 10,
            "space colonisation with snapping must close loops: {report:?}"
        );
        for block in &blocks {
            assert!(block.boundary.is_valid(), "{block:?}");
            assert!(block.boundary.is_ccw());
            assert!(block.boundary.area() >= MIN_BLOCK_AREA);
        }
        // Euler's formula on the surviving planar subdivision: V - E + F = 1 + C,
        // over the components that actually have an edge. An isolated node is
        // its own component with no dart, so the walk never emits its face and
        // it has to come out of both sides.
        let planar = Planar::build(&graph);
        let vertices = planar.neighbours.len() - report.isolated_nodes;
        let edges = planar.darts() / 2;
        let components = report.components - report.isolated_nodes;
        assert_eq!(
            vertices + report.traversals,
            edges + 1 + components,
            "the walk found a different number of faces than the graph has"
        );
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
    /// Decide whether the change to block set was intended. If it was, every
    /// golden layout file in the repository is invalidated **on purpose** —
    /// update this literal and regenerate them together. If it was not, the
    /// diff that caused it is the bug.
    #[test]
    fn the_block_digest_is_pinned() {
        {
            assert_eq!(
                format!("{:016x}", reference_digest()),
                "b2a0829b88b6fccc",
                "the block set moved"
            );
        }
    }

    #[test]
    fn two_runs_in_one_process_are_identical() {
        assert_eq!(reference_digest(), reference_digest());
    }

    #[test]
    fn face_order_does_not_depend_on_segment_order() {
        let (_, graph) = grown(4, 6);
        let straight = extract(&graph);

        // Reverse the segment vector and flip every segment's direction. Same
        // geometry, completely different dart numbering.
        let mut shuffled = RoadGraph::new(graph.snap_tolerance);
        shuffled.nodes = graph.nodes.clone();
        shuffled.segments = graph
            .segments
            .iter()
            .rev()
            .map(|s| crate::RoadSegment {
                from: s.to,
                to: s.from,
                class: s.class,
            })
            .collect();

        let reversed = extract(&shuffled);
        assert_eq!(digest(&straight), digest(&reversed));
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
                "blocks::tests::print_reference_digest",
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
            .find_map(|line| line.strip_prefix("POLIS_BLOCKS_DIGEST="))
            .unwrap_or_else(|| panic!("child printed no digest:\n{stdout}"));
        u64::from_str_radix(line.trim(), 16).expect("hex digest")
    }

    /// The child half of [`two_fresh_processes_are_identical`].
    #[test]
    #[ignore = "child process of two_fresh_processes_are_identical"]
    fn print_reference_digest() {
        println!("POLIS_BLOCKS_DIGEST={:016x}", reference_digest());
    }

    // -----------------------------------------------------------------------
    // Districts
    // -----------------------------------------------------------------------

    #[test]
    fn a_block_joins_the_district_whose_files_are_nearest() {
        let sites = DistrictSites::from_points([
            (lp("src/auth"), Point::new(0.0, 0.0)),
            (lp("src/auth"), Point::new(4.0, 0.0)),
            (lp("tests"), Point::new(100.0, 0.0)),
        ]);
        assert_eq!(sites.len(), 3);
        assert_eq!(sites.districts(), &[lp("src/auth"), lp("tests")]);

        let mut blocks = vec![
            Block {
                id: BlockId(0),
                boundary: Polygon::new(vec![
                    Point::new(0.0, 0.0),
                    Point::new(6.0, 0.0),
                    Point::new(6.0, 6.0),
                    Point::new(0.0, 6.0),
                ]),
                district: LogicalPath::root(),
            },
            Block {
                id: BlockId(1),
                boundary: Polygon::new(vec![
                    Point::new(96.0, 0.0),
                    Point::new(104.0, 0.0),
                    Point::new(104.0, 8.0),
                    Point::new(96.0, 8.0),
                ]),
                district: LogicalPath::root(),
            },
        ];
        assert_eq!(assign_districts(&mut blocks, &sites), 2);
        assert_eq!(blocks[0].district, lp("src/auth"));
        assert_eq!(blocks[1].district, lp("tests"));

        let grouped = group_by_district(&blocks);
        assert_eq!(grouped[&lp("src/auth")], vec![BlockId(0)]);
        assert_eq!(grouped[&lp("tests")], vec![BlockId(1)]);
    }

    /// The grid is an index, not an approximation: it must agree with a full
    /// scan for every query, including ones far outside the populated cells.
    #[test]
    fn the_site_grid_agrees_with_a_full_scan() {
        let (tree, _) = grown(5, 7);
        let extent = suggested_extent(&tree);
        let attractors = scatter_attractors(&tree, extent);
        let sites = DistrictSites::from_scatter(&tree, &attractors);
        assert_eq!(sites.len(), tree.files.len());

        let mut rng = crate::determinism::SeededRng::for_seed(7, "site probe");
        for _ in 0..400 {
            let at = Point::new(
                rng.range_f32(-extent * 1.5, extent * 1.5),
                rng.range_f32(-extent * 1.5, extent * 1.5),
            );
            let indexed = sites.district_at(at).cloned();
            let scanned = sites
                .points
                .iter()
                .enumerate()
                .min_by(|(ia, a), (ib, b)| {
                    a.distance_squared(at)
                        .total_cmp(&b.distance_squared(at))
                        .then_with(|| ia.cmp(ib))
                })
                .map(|(index, _)| sites.districts[sites.owner[index] as usize].clone());
            assert_eq!(indexed, scanned, "grid and scan disagree at {at:?}");
        }
        // Far outside every cell.
        assert!(sites.district_at(Point::new(1.0e7, -1.0e7)).is_some());
        assert!(sites.district_at(Point::new(f32::NAN, 0.0)).is_some());
        assert!(DistrictSites::default()
            .district_at(Point::ORIGIN)
            .is_none());
    }

    /// PRD §7.4: the caller's iteration order must not reach the layout.
    #[test]
    fn site_construction_is_independent_of_input_order() {
        let mut pairs: Vec<(LogicalPath, Point)> = (0..60)
            .map(|i| {
                #[allow(clippy::cast_precision_loss)] // fixture only
                let f = i as f32;
                (
                    lp(&format!("d{}/f.rs", i % 7)),
                    Point::new(f * 3.0, f * -2.0),
                )
            })
            .collect();
        let forward = DistrictSites::from_points(pairs.clone());
        pairs.reverse();
        let backward = DistrictSites::from_points(pairs.clone());
        // A rotation as well, so the test does not only cover one permutation.
        pairs.rotate_left(23);
        let rotated = DistrictSites::from_points(pairs);

        for at in [
            Point::ORIGIN,
            Point::new(50.0, -30.0),
            Point::new(-500.0, 500.0),
            Point::new(91.0, -61.0),
        ] {
            assert_eq!(forward.district_at(at), backward.district_at(at));
            assert_eq!(forward.district_at(at), rotated.district_at(at));
        }
        assert_eq!(forward.districts(), backward.districts());
        assert_eq!(forward.centre(&lp("d3")), rotated.centre(&lp("d3")));
    }

    #[test]
    fn a_district_centre_is_the_mean_of_its_sites() {
        let sites = DistrictSites::from_points([
            (lp("a"), Point::new(0.0, 0.0)),
            (lp("a"), Point::new(4.0, 8.0)),
            (lp("b"), Point::new(100.0, 0.0)),
        ]);
        assert_eq!(sites.centre(&lp("a")), Some(Point::new(2.0, 4.0)));
        assert_eq!(sites.centre(&lp("b")), Some(Point::new(100.0, 0.0)));
        assert_eq!(sites.centre(&lp("missing")), None);
    }

    #[test]
    fn a_top_level_file_belongs_to_the_root_district() {
        assert!(district_of(&lp("README.md")).is_root());
        assert_eq!(district_of(&lp("src/main.rs")), lp("src"));
        assert_eq!(district_of(&lp("a/b/c.rs")), lp("a/b"));
    }
}
