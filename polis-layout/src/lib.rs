//! `polis-layout` — city generation (PRD §7, §14).
//!
//! > The look comes from treating this as a growth process under constraints,
//! > not a layout algorithm. Irregularity should be the residue of history, not
//! > noise sprinkled on a grid.
//!
//! # The pipeline order is non-negotiable
//!
//! Roads → blocks → lots → buildings ([`terrain`], [`roads`], [`blocks`],
//! [`lots`], [`buildings`]), driven by [`city`].
//!
//! > Place buildings first and connect them afterward and you get suburbia or a
//! > circuit board, every time.
//!
//! | Module | Step |
//! |---|---|
//! | [`determinism`] | the seeded generator, the simplex noise, and quantisation |
//! | [`age`] | 0 — real commit time to the age of the ground (PRD §7.1, ADR-0062) |
//! | `geom` | the `f64` planar geometry the middle of the pipeline runs in (ADR-0053) |
//! | [`terrain`] | 1 — the height field the growth follows |
//! | `territory` | 2 — one polygon per district, from the directory tree (ADR-0056) |
//! | `districts` | the two rules that keep a district's ground one piece (ADR-0058) |
//! | `accrete` | 3 — the settlement, replayed in git commit order (PRD §7.1) |
//! | `voronoi`, [`roads`] | 4 — the boundary network of the settled ground (ADR-0052) |
//! | [`blocks`] | 5 — the closed faces of that network |
//! | [`lots`] | 6 — recursive subdivision along the longest axis |
//! | [`buildings`] | 7 — lots inset by a setback |
//! | [`city`] | the pipeline, the growth step, and the serialized [`CityLayout`] |
//!
//! # The roads are the boundary network of the settled ground
//!
//! PRD §7.2 step 2 specifies space colonisation. Implemented literally it
//! produced exactly the failure the same paragraph warns about — "without
//! snapping you get a tree, and trees read as artificial" — because snapping can
//! only bridge two things that are already close, and the attractor clouds were
//! spatially isolated. **A road here is instead the line where one parcel's
//! territory stops and the next one's begins**: the Voronoi diagram of a set of
//! plots accreted one file at a time in commit order. That is planar, connected
//! and full of closed faces by construction rather than by tuning a radius.
//! ADR-0052 records the decision and what it costs.
//!
//! # Determinism is a hard requirement, not a nice-to-have
//!
//! > **Every random draw is seeded from a hash of the logical path.** Never from
//! > wall clock, never from a global RNG, never from iteration order of a
//! > `HashMap`. (PRD §7.4)
//!
//! Three consequences the product depends on: the same repo produces the same
//! city on every launch and every machine (spatial memory is the entire point);
//! growth is genuinely incremental; and the layout becomes golden-file testable,
//! which PRD §16 calls the most important test in the suite.
//!
//! Practically, that means four rules in this crate:
//!
//! 1. `BTreeMap`/sorted `Vec` everywhere iteration order can reach the output.
//! 2. Every random draw comes from [`determinism::SeededRng::for_path`], seeded
//!    per *draw site* so adding a draw cannot shift every later one.
//! 3. Noise is written out in [`determinism`], not taken from a crate whose
//!    output could change between releases (ADR-0050).
//! 4. `rayon` is allowed only where results are collected in index order.
//!    `par_iter().map(…).collect::<Vec<_>>()` is deterministic; folding into a
//!    shared map or pushing through a `Mutex` is not.
//!
//! # Ownership of the shared surface
//!
//! Every type in this file is the crate's **cross-module contract**, and
//! [`CityLayout`] is also `polis-render`'s and `polis-world`'s input. The
//! geometry primitives are implemented here rather than stubbed, precisely
//! because a `Vec2::length` invented separately in `roads.rs` and `lots.rs` is
//! how a fan-out produces two subtly different cities. The *algorithms* are
//! stubbed; the vocabulary they share is not.

pub(crate) mod accrete;
pub mod age;
pub mod blocks;
pub mod buildings;
pub mod city;
pub mod determinism;
pub(crate) mod districts;
pub(crate) mod geom;
pub mod lots;
pub(crate) mod memo;
pub(crate) mod regions;
pub mod roads;
pub mod terrain;
pub(crate) mod territory;
pub(crate) mod voronoi;

use std::collections::BTreeMap;
use std::ops::{Add, Div, Mul, Neg, Sub};

use polis_events::LogicalPath;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------

/// A position in city space (PRD §7).
///
/// City space is unitless, has its origin at the civic square, and is stable
/// across runs. Distinct from [`Vec2`] on purpose: adding two positions is
/// meaningless and subtracting them gives a displacement, and having the type
/// system say so removes a whole class of geometry bug from five modules that
/// are being written in parallel.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Point {
    /// Horizontal.
    pub x: f32,
    /// Vertical.
    pub y: f32,
}

/// A displacement in city space.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Vec2 {
    /// Horizontal component.
    pub x: f32,
    /// Vertical component.
    pub y: f32,
}

impl Point {
    /// The origin — the civic square, PRD §8's "recognisable open space at the
    /// historic centre".
    pub const ORIGIN: Self = Self { x: 0.0, y: 0.0 };

    /// A position.
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    /// Distance to another position.
    pub fn distance(self, other: Self) -> f32 {
        (other - self).length()
    }

    /// Squared distance. Preferred inside the road-growth kill-radius and
    /// snap-radius loops, which run per candidate per step.
    pub fn distance_squared(self, other: Self) -> f32 {
        (other - self).length_squared()
    }

    /// Linear interpolation, `t` clamped to `[0, 1]`.
    #[must_use]
    pub fn lerp(self, other: Self, t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        Self::new(
            self.x + (other.x - self.x) * t,
            self.y + (other.y - self.y) * t,
        )
    }

    /// As a displacement from the origin.
    pub fn to_vec(self) -> Vec2 {
        Vec2::new(self.x, self.y)
    }

    /// True when both coordinates are finite.
    ///
    /// A `NaN` coordinate propagates silently through every later stage and only
    /// surfaces as a missing building, so the pipeline asserts this at each
    /// stage boundary rather than at the end.
    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }
}

impl Vec2 {
    /// The zero displacement.
    pub const ZERO: Self = Self { x: 0.0, y: 0.0 };

    /// A displacement.
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    /// A unit displacement at `radians` from the positive x axis.
    pub fn from_angle(radians: f32) -> Self {
        Self::new(radians.cos(), radians.sin())
    }

    /// Euclidean length.
    pub fn length(self) -> f32 {
        self.length_squared().sqrt()
    }

    /// Squared length, avoiding the square root.
    pub fn length_squared(self) -> f32 {
        self.x.mul_add(self.x, self.y * self.y)
    }

    /// Dot product.
    pub fn dot(self, other: Self) -> f32 {
        self.x.mul_add(other.x, self.y * other.y)
    }

    /// 2-D cross product — the z component of the 3-D cross product.
    ///
    /// Its sign is the winding of a turn, which is what the planar face walk in
    /// [`blocks`] and the convexity checks in [`lots`] are built on.
    pub fn cross(self, other: Self) -> f32 {
        self.x.mul_add(other.y, -(self.y * other.x))
    }

    /// Rotated a quarter turn counter-clockwise. The outward normal of a
    /// clockwise-wound edge, and the setback direction in [`buildings`].
    #[must_use]
    pub fn perp(self) -> Self {
        Self::new(-self.y, self.x)
    }

    /// Unit vector in the same direction, or [`Vec2::ZERO`] for a zero-length
    /// input.
    ///
    /// Returning zero rather than `NaN` matters: a degenerate segment is a
    /// normal outcome of road growth — two attractors at the same point — and a
    /// `NaN` here would silently delete a whole district.
    #[must_use]
    pub fn normalized(self) -> Self {
        let len = self.length();
        if len > f32::EPSILON {
            self / len
        } else {
            Self::ZERO
        }
    }

    /// Angle from the positive x axis, in radians, in `(-π, π]`.
    pub fn angle(self) -> f32 {
        self.y.atan2(self.x)
    }

    /// As a position offset from the origin.
    pub fn to_point(self) -> Point {
        Point::new(self.x, self.y)
    }
}

impl Sub for Point {
    type Output = Vec2;
    fn sub(self, rhs: Self) -> Vec2 {
        Vec2::new(self.x - rhs.x, self.y - rhs.y)
    }
}

impl Add<Vec2> for Point {
    type Output = Self;
    fn add(self, rhs: Vec2) -> Self {
        Self::new(self.x + rhs.x, self.y + rhs.y)
    }
}

impl Sub<Vec2> for Point {
    type Output = Self;
    fn sub(self, rhs: Vec2) -> Self {
        Self::new(self.x - rhs.x, self.y - rhs.y)
    }
}

impl Add for Vec2 {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        Self::new(self.x + rhs.x, self.y + rhs.y)
    }
}

impl Sub for Vec2 {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        Self::new(self.x - rhs.x, self.y - rhs.y)
    }
}

impl Mul<f32> for Vec2 {
    type Output = Self;
    fn mul(self, rhs: f32) -> Self {
        Self::new(self.x * rhs, self.y * rhs)
    }
}

impl Div<f32> for Vec2 {
    type Output = Self;
    fn div(self, rhs: f32) -> Self {
        Self::new(self.x / rhs, self.y / rhs)
    }
}

impl Neg for Vec2 {
    type Output = Self;
    fn neg(self) -> Self {
        Self::new(-self.x, -self.y)
    }
}

/// A closed polygon in city space.
///
/// The one boundary representation the whole crate uses: block boundaries, lot
/// parcels, building footprints and district outlines are all this. The closing
/// edge is implicit — `vertices` does **not** repeat the first point — because a
/// representation that sometimes repeats it and sometimes does not is how a
/// shoelace area comes out doubled in one module and correct in another.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Polygon {
    /// Boundary vertices. The last connects back to the first.
    pub vertices: Vec<Point>,
}

impl Polygon {
    /// A polygon from its vertices.
    pub fn new(vertices: Vec<Point>) -> Self {
        Self { vertices }
    }

    /// Number of vertices, which is also the number of edges.
    pub fn len(&self) -> usize {
        self.vertices.len()
    }

    /// True when there are no vertices.
    pub fn is_empty(&self) -> bool {
        self.vertices.is_empty()
    }

    /// True when the polygon has enough vertices to enclose area.
    pub fn is_valid(&self) -> bool {
        self.vertices.len() >= 3 && self.vertices.iter().all(|p| p.is_finite())
    }

    /// Each edge as an ordered pair, including the closing edge.
    pub fn edges(&self) -> impl Iterator<Item = (Point, Point)> + '_ {
        let n = self.vertices.len();
        (0..n).map(move |i| (self.vertices[i], self.vertices[(i + 1) % n]))
    }

    /// Twice the signed area (the shoelace sum).
    ///
    /// Positive for counter-clockwise winding. Exposed alongside [`area`] and
    /// [`is_ccw`] so a caller that needs the sign does not recompute it.
    ///
    /// [`area`]: Self::area
    /// [`is_ccw`]: Self::is_ccw
    pub fn signed_area_x2(&self) -> f32 {
        self.edges()
            .map(|(a, b)| a.x.mul_add(b.y, -(b.x * a.y)))
            .sum()
    }

    /// Unsigned area. Zero for a degenerate polygon rather than a negative
    /// number, so a clamp against it cannot invert.
    pub fn area(&self) -> f32 {
        if self.vertices.len() < 3 {
            return 0.0;
        }
        (self.signed_area_x2() * 0.5).abs()
    }

    /// True when the vertices wind counter-clockwise.
    pub fn is_ccw(&self) -> bool {
        self.signed_area_x2() > 0.0
    }

    /// Reverses the winding in place.
    pub fn reverse(&mut self) {
        self.vertices.reverse();
    }

    /// Area centroid, or the vertex average for a degenerate polygon.
    ///
    /// The vertex-average fallback matters: a lot subdivided down to a sliver
    /// still needs a label position, and a `NaN` centroid removes the building
    /// from the map with no error anywhere.
    pub fn centroid(&self) -> Point {
        if self.vertices.is_empty() {
            return Point::ORIGIN;
        }
        let a2 = self.signed_area_x2();
        if a2.abs() <= f32::EPSILON {
            // Counted as an `f32` rather than casting `len()`, which is a lossy
            // conversion the compiler is right to complain about.
            let mut sum = Vec2::ZERO;
            let mut count = 0.0_f32;
            for p in &self.vertices {
                sum = sum + p.to_vec();
                count += 1.0;
            }
            return (sum / count).to_point();
        }
        let (mut cx, mut cy) = (0.0_f32, 0.0_f32);
        for (a, b) in self.edges() {
            let cross = a.x.mul_add(b.y, -(b.x * a.y));
            cx += (a.x + b.x) * cross;
            cy += (a.y + b.y) * cross;
        }
        Point::new(cx / (3.0 * a2), cy / (3.0 * a2))
    }

    /// Axis-aligned bounds as `(min, max)`, or `None` when empty.
    pub fn bounds(&self) -> Option<(Point, Point)> {
        let mut it = self.vertices.iter().copied();
        let first = it.next()?;
        let (mut lo, mut hi) = (first, first);
        for p in it {
            lo = Point::new(lo.x.min(p.x), lo.y.min(p.y));
            hi = Point::new(hi.x.max(p.x), hi.y.max(p.y));
        }
        Some((lo, hi))
    }

    /// The longest edge as `(index, length)` — PRD §7.2's subdivision axis.
    ///
    /// Ties break towards the **lowest index**, deterministically, because "the
    /// longest axis" is otherwise decided by floating-point noise and PRD §16
    /// compares golden files across two operating systems.
    pub fn longest_edge(&self) -> Option<(usize, f32)> {
        self.edges()
            .map(|(a, b)| (b - a).length())
            .enumerate()
            .fold(None, |best, (i, len)| match best {
                Some((_, bl)) if bl >= len => best,
                _ => Some((i, len)),
            })
    }

    /// Winding number containment test.
    ///
    /// Winding rather than ray casting because block boundaries from a planar
    /// face walk can be non-convex and can touch themselves at a snapped
    /// intersection, where a crossing count is ambiguous and a winding number is
    /// not.
    pub fn contains(&self, p: Point) -> bool {
        let mut winding = 0_i32;
        for (a, b) in self.edges() {
            if a.y <= p.y {
                if b.y > p.y && (b - a).cross(p - a) > 0.0 {
                    winding += 1;
                }
            } else if b.y <= p.y && (b - a).cross(p - a) < 0.0 {
                winding -= 1;
            }
        }
        winding != 0
    }
}

// ---------------------------------------------------------------------------
// Roads
// ---------------------------------------------------------------------------

/// An index into [`RoadGraph::nodes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(pub u32);

/// A road intersection or endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RoadNode {
    /// Where it is.
    pub position: Point,
}

/// A road segment between two nodes.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RoadSegment {
    /// One end.
    pub from: NodeId,
    /// The other end.
    pub to: NodeId,
    /// Rendering class, which sets width and decluttering priority.
    pub class: RoadClass,
}

/// How prominent a road is (PRD §8, §12).
///
/// Not the same thing as a street (PRD §9): a [`RoadClass`] is a property of the
/// grown road network, whereas a street is an import relation drawn *over* it.
/// The district-level skeleton must stay readable at every zoom even while the
/// street level tangles, and this is the field the renderer declutters on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum RoadClass {
    /// Between districts. Survives decluttering at every zoom.
    Arterial,
    /// Within a district.
    Street,
    /// Block-internal access. First thing dropped when zoomed out.
    Alley,
}

/// A planar road graph. Its faces are the blocks (PRD §7.2).
///
/// > **The organic signature:** when a new segment's endpoint lands within
/// > `snap_radius` of an existing intersection, snap to it rather than creating
/// > a new node. Those irregular four- and five-way junctions are what the eye
/// > reads as "grown." Without snapping you get a tree, and trees read as
/// > artificial.
///
/// Snapping is not an optimisation and not optional; it is the single rule that
/// makes the whole look work. The tolerance therefore lives **on the graph**
/// rather than in a growth-parameter struct that gets passed around, so it
/// cannot differ between the initial generation and a later incremental step —
/// which would put a five-way junction in one run and two nodes a hair apart in
/// the next, and fail PRD §16's golden-file test in a way that looks like noise.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoadGraph {
    /// Intersections and endpoints.
    pub nodes: Vec<RoadNode>,
    /// Segments, as index pairs into [`RoadGraph::nodes`].
    pub segments: Vec<RoadSegment>,
    /// The snapping tolerance. An endpoint landing within this distance of an
    /// existing node becomes that node.
    pub snap_tolerance: f32,
}

impl Default for RoadGraph {
    fn default() -> Self {
        Self::new(DEFAULT_SNAP_TOLERANCE)
    }
}

/// The default snapping tolerance, in city-space units (PRD §7.2).
///
/// Roughly a fifth of a segment; large enough that near-misses become junctions,
/// small enough that distinct intersections stay distinct.
pub const DEFAULT_SNAP_TOLERANCE: f32 = 2.0;

impl RoadGraph {
    /// An empty graph with a given snapping tolerance.
    pub fn new(snap_tolerance: f32) -> Self {
        Self {
            nodes: Vec::new(),
            segments: Vec::new(),
            snap_tolerance,
        }
    }

    /// The position of a node.
    pub fn position(&self, id: NodeId) -> Option<Point> {
        self.nodes.get(id.0 as usize).map(|n| n.position)
    }

    /// The nearest existing node within [`snap_tolerance`], if any.
    ///
    /// Ties break towards the **lowest node index**, which is what makes the
    /// result independent of how the vector happened to be filled.
    ///
    /// [`snap_tolerance`]: Self::snap_tolerance
    pub fn snap_target(&self, at: Point) -> Option<NodeId> {
        let limit = self.snap_tolerance * self.snap_tolerance;
        let mut best: Option<(usize, f32)> = None;
        for (i, node) in self.nodes.iter().enumerate() {
            let d2 = node.position.distance_squared(at);
            if d2 <= limit && best.is_none_or(|(_, bd)| d2 < bd) {
                best = Some((i, d2));
            }
        }
        best.map(|(i, _)| NodeId(u32::try_from(i).unwrap_or(u32::MAX)))
    }

    /// Adds a node, snapping to an existing one where PRD §7.2 says to.
    ///
    /// **This is the organic signature.** Growth code should call this rather
    /// than pushing onto [`RoadGraph::nodes`] directly; a module that pushes
    /// directly produces a tree, and trees read as artificial.
    pub fn add_node_snapped(&mut self, at: Point) -> NodeId {
        if let Some(existing) = self.snap_target(at) {
            return existing;
        }
        let id = NodeId(u32::try_from(self.nodes.len()).unwrap_or(u32::MAX));
        self.nodes.push(RoadNode { position: at });
        id
    }

    /// Adds a segment, ignoring a self-loop and an exact duplicate.
    ///
    /// Returns whether a segment was actually added. Snapping makes both cases
    /// routine rather than exceptional: two segments growing towards the same
    /// junction from the same neighbour is exactly what produces one.
    pub fn add_segment(&mut self, from: NodeId, to: NodeId, class: RoadClass) -> bool {
        if from == to {
            return false;
        }
        let exists = self
            .segments
            .iter()
            .any(|s| (s.from == from && s.to == to) || (s.from == to && s.to == from));
        if exists {
            return false;
        }
        self.segments.push(RoadSegment { from, to, class });
        true
    }

    /// How many segments meet at a node. A degree of four or five is the
    /// junction PRD §7.2 is asking for.
    pub fn degree(&self, id: NodeId) -> usize {
        self.segments
            .iter()
            .filter(|s| s.from == id || s.to == id)
            .count()
    }
}

// ---------------------------------------------------------------------------
// Blocks, lots, buildings
// ---------------------------------------------------------------------------

/// An index into [`CityLayout::blocks`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BlockId(pub u32);

/// An index into [`CityLayout::lots`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LotId(pub u32);

/// A closed loop in the road graph. Contains lots (PRD §3, §7.2 step 3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Block {
    /// Its own index, so a block passed on its own stays identifiable.
    pub id: BlockId,
    /// Boundary in city space, wound counter-clockwise so subdivision is
    /// orientation-independent.
    pub boundary: Polygon,
    /// The district this block belongs to — the directory whose files it holds.
    pub district: LogicalPath,
}

/// The parcel a building sits on (PRD §3, §7.2 step 4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lot {
    /// Its own index.
    pub id: LotId,
    /// The block it was subdivided out of.
    pub block: BlockId,
    /// Parcel boundary.
    pub boundary: Polygon,
    /// The file that occupies it, or `None` for a vacant lot — a file that was
    /// deleted, whose lot goes to seed rather than vanishing (PRD §7.5).
    pub occupant: Option<LogicalPath>,
}

impl Lot {
    /// True for a lot whose file was deleted (PRD §7.5).
    pub fn is_vacant(&self) -> bool {
        self.occupant.is_none()
    }
}

/// A file, rendered (PRD §3, §7.2 step 5, §7.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Building {
    /// The file this is. Also the seed for every draw about it (PRD §7.4).
    pub path: LogicalPath,
    /// The lot it stands on.
    pub lot: LotId,
    /// Footprint, ready for `lyon` to triangulate once and cache in a vertex
    /// buffer (PRD §13). The lot inset by a setback and rotated.
    pub footprint: Polygon,
    /// Height, proportional to uncommitted diff lines. The city rises as agents
    /// work and settles when you merge; the tallest thing on the map is the
    /// biggest unreviewed pile (PRD §7.3).
    ///
    /// Tweened between updates by the renderer rather than snapped — that
    /// interpolation is "the entire difference between alive and steppy".
    pub height: f32,
    /// Roof form, from a hash of the path. Silhouette variety carries most of
    /// the organic reading and costs nothing (PRD §7.3).
    pub roof: RoofForm,
    /// Rotation in radians, within ±4° (PRD §7.2 step 5).
    pub rotation: f32,
}

/// Silhouette variety (PRD §7.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum RoofForm {
    /// Flat.
    Flat,
    /// Stepped.
    Stepped,
    /// Pitched.
    Pitched,
}

impl RoofForm {
    /// Every form, in a fixed order. Selection is
    /// `ALL[seed % ALL.len()]` from a path-seeded draw, never a global RNG.
    pub const ALL: [Self; 3] = [Self::Flat, Self::Stepped, Self::Pitched];
}

/// A directory, rendered as a region (PRD §3, §8).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct District {
    /// The directory this district is.
    pub path: LogicalPath,
    /// Boundary polygon in city space. Part of the wayfinding layer, which must
    /// survive when everything else is decluttered away (PRD §8).
    pub boundary: Polygon,
    /// Centroid, for label placement and for the territory kernel positions
    /// PRD §6.4 splats.
    pub centre: Point,
    /// Blocks inside this district.
    pub blocks: Vec<BlockId>,
}

// ---------------------------------------------------------------------------
// The layout
// ---------------------------------------------------------------------------

/// Schema version of the serialized [`CityLayout`].
///
/// PRD §16's golden files are the most important test in the suite, and a golden
/// file with no version is indistinguishable from a stale one. Bump on any
/// change to the serialized shape; a bump invalidates every golden file **on
/// purpose**, which is the signal, not a nuisance (ADR-0029).
pub const LAYOUT_SCHEMA: u32 = 1;

/// The generated city (PRD §5, `World::layout`).
///
/// Changes rarely, and is serialized whole for PRD §16's golden-file test —
/// which is what makes every field here layout-visible and therefore bound by
/// PRD §7.4. `BTreeMap` for the keyed collections; index-ordered `Vec` for the
/// rest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CityLayout {
    /// [`LAYOUT_SCHEMA`] at the time of generation.
    pub schema: u32,
    /// Half-width of the square the city occupies, in city-space units. The
    /// terrain field and the initial attractor scatter are both sized from this.
    pub extent: f32,
    /// The road graph. Its faces are the blocks.
    pub roads: RoadGraph,
    /// Closed loops in the road graph, indexed by [`BlockId`].
    pub blocks: Vec<Block>,
    /// Parcels, indexed by [`LotId`]. Includes vacant ones (PRD §7.5).
    pub lots: Vec<Lot>,
    /// One building per file, keyed by logical path so a worktree adds no
    /// geometry (PRD §7.6).
    pub buildings: BTreeMap<LogicalPath, Building>,
    /// Districts — directories, rendered as regions. The district skeleton must
    /// stay readable at every zoom even while the street level tangles (PRD §8).
    pub districts: BTreeMap<LogicalPath, District>,
    /// Cross-district import relations, drawn over the roads (PRD §9). Sorted,
    /// because their order is serialized.
    pub streets: Vec<StreetLine>,
}

impl Default for CityLayout {
    fn default() -> Self {
        Self {
            schema: LAYOUT_SCHEMA,
            extent: 0.0,
            roads: RoadGraph::default(),
            blocks: Vec::new(),
            lots: Vec::new(),
            buildings: BTreeMap::new(),
            districts: BTreeMap::new(),
            streets: Vec::new(),
        }
    }
}

impl CityLayout {
    /// A block by index.
    pub fn block(&self, id: BlockId) -> Option<&Block> {
        self.blocks.get(id.0 as usize)
    }

    /// A lot by index.
    pub fn lot(&self, id: LotId) -> Option<&Lot> {
        self.lots.get(id.0 as usize)
    }

    /// Every vacant lot — a deleted file's parcel, gone to seed (PRD §7.5).
    pub fn vacant_lots(&self) -> impl Iterator<Item = &Lot> + '_ {
        self.lots.iter().filter(|l| l.is_vacant())
    }

    /// The building for a file.
    pub fn building(&self, path: &LogicalPath) -> Option<&Building> {
        self.buildings.get(path)
    }
}

/// A drawn street: a cross-district import relation (PRD §9).
///
/// The geometry, not the graph. `polis-repo` produces the relation; this is
/// where it lands on the map, so the renderer never has to look at an import
/// graph to draw one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreetLine {
    /// District the edges leave.
    pub from: LogicalPath,
    /// District they arrive at.
    pub to: LogicalPath,
    /// Distinct import edges carried. **Width is proportional to this**
    /// (PRD §9).
    pub edge_count: u32,
    /// The drawn path, district centre to district centre, following the road
    /// network where one exists.
    pub polyline: Vec<Point>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square() -> Polygon {
        // Counter-clockwise unit square scaled by 10.
        Polygon::new(vec![
            Point::new(0.0, 0.0),
            Point::new(10.0, 0.0),
            Point::new(10.0, 10.0),
            Point::new(0.0, 10.0),
        ])
    }

    #[test]
    fn point_and_vec_are_different_kinds_of_thing() {
        let a = Point::new(1.0, 2.0);
        let b = Point::new(4.0, 6.0);
        let d: Vec2 = b - a;
        assert_eq!(d, Vec2::new(3.0, 4.0));
        assert!((d.length() - 5.0).abs() < 1e-6);
        assert_eq!(a + d, b);
        assert_eq!(b - d, a);
        assert!((a.distance(b) - 5.0).abs() < 1e-6);
        assert!((a.distance_squared(b) - 25.0).abs() < 1e-4);
    }

    #[test]
    fn vec_ops_agree_with_their_definitions() {
        let a = Vec2::new(3.0, 4.0);
        let b = Vec2::new(-4.0, 3.0);
        assert!((a.dot(b)).abs() < 1e-6, "perpendicular vectors dot to zero");
        assert_eq!(a.perp(), b);
        assert!((a.cross(b) - 25.0).abs() < 1e-4);
        assert!((a.normalized().length() - 1.0).abs() < 1e-6);
        assert_eq!(-a, Vec2::new(-3.0, -4.0));
        assert_eq!(a * 2.0, Vec2::new(6.0, 8.0));
        assert_eq!(a / 2.0, Vec2::new(1.5, 2.0));
        assert_eq!(a + b, Vec2::new(-1.0, 7.0));
        assert_eq!(a - b, Vec2::new(7.0, 1.0));
    }

    /// A degenerate segment is a normal outcome of road growth, and a `NaN`
    /// here would silently delete a district.
    #[test]
    fn normalizing_a_zero_vector_yields_zero_not_nan() {
        let n = Vec2::ZERO.normalized();
        assert_eq!(n, Vec2::ZERO);
        assert!(n.x.is_finite() && n.y.is_finite());
    }

    #[test]
    fn from_angle_and_angle_round_trip() {
        for deg in [0.0_f32, 30.0, 90.0, 179.0, -45.0] {
            let r = deg.to_radians();
            let v = Vec2::from_angle(r);
            assert!((v.length() - 1.0).abs() < 1e-6);
            assert!((v.angle() - r).abs() < 1e-5, "{deg}");
        }
    }

    #[test]
    fn polygon_area_centroid_and_winding() {
        let p = square();
        assert!(p.is_valid());
        assert_eq!(p.len(), 4);
        assert!((p.area() - 100.0).abs() < 1e-3);
        assert!(p.is_ccw());

        let c = p.centroid();
        assert!((c.x - 5.0).abs() < 1e-3 && (c.y - 5.0).abs() < 1e-3);

        // Reversing flips the winding but not the unsigned area.
        let mut r = p.clone();
        r.reverse();
        assert!(!r.is_ccw());
        assert!((r.area() - 100.0).abs() < 1e-3);
    }

    #[test]
    fn a_degenerate_polygon_has_zero_area_and_a_finite_centroid() {
        let line = Polygon::new(vec![Point::new(0.0, 0.0), Point::new(4.0, 0.0)]);
        assert!(!line.is_valid());
        assert!(line.area().abs() < f32::EPSILON);
        let mid = line.centroid();
        assert!(mid.is_finite(), "a sliver lot still needs a label position");
        assert!((mid.x - 2.0).abs() < 1e-6);

        assert_eq!(Polygon::default().centroid(), Point::ORIGIN);
        assert!(Polygon::default().is_empty());
        assert_eq!(Polygon::default().bounds(), None);
    }

    #[test]
    fn bounds_and_longest_edge_are_deterministic() {
        let p = Polygon::new(vec![
            Point::new(0.0, 0.0),
            Point::new(10.0, 0.0),
            Point::new(10.0, 4.0),
            Point::new(0.0, 4.0),
        ]);
        let (lo, hi) = p.bounds().unwrap();
        assert_eq!((lo.x, lo.y, hi.x, hi.y), (0.0, 0.0, 10.0, 4.0));

        // Edges 0 and 2 are both length 10; the tie must break to the lower index
        // or the subdivision axis is decided by floating-point noise.
        let (i, len) = p.longest_edge().unwrap();
        assert_eq!(i, 0);
        assert!((len - 10.0).abs() < 1e-6);
    }

    #[test]
    fn containment_uses_winding_so_concave_blocks_work() {
        // An L shape: a ray cast through the notch would be ambiguous.
        let l = Polygon::new(vec![
            Point::new(0.0, 0.0),
            Point::new(10.0, 0.0),
            Point::new(10.0, 4.0),
            Point::new(4.0, 4.0),
            Point::new(4.0, 10.0),
            Point::new(0.0, 10.0),
        ]);
        assert!(l.contains(Point::new(1.0, 1.0)));
        assert!(l.contains(Point::new(9.0, 1.0)));
        assert!(l.contains(Point::new(1.0, 9.0)));
        assert!(!l.contains(Point::new(9.0, 9.0)), "the notch is outside");
        assert!(!l.contains(Point::new(-1.0, 5.0)));
    }

    /// PRD §7.2's organic signature. Without snapping the graph is a tree, and
    /// trees read as artificial.
    #[test]
    fn an_endpoint_within_tolerance_snaps_to_the_existing_intersection() {
        let mut graph = RoadGraph::new(2.0);
        let origin = graph.add_node_snapped(Point::new(0.0, 0.0));
        let east = graph.add_node_snapped(Point::new(10.0, 0.0));
        assert_eq!(graph.nodes.len(), 2);

        // Within tolerance of `east`: same node, no new intersection.
        let near_east = graph.add_node_snapped(Point::new(11.0, 0.5));
        assert_eq!(near_east, east);
        assert_eq!(graph.nodes.len(), 2);

        // Outside tolerance: a genuinely new node.
        let far_east = graph.add_node_snapped(Point::new(13.0, 0.0));
        assert_ne!(far_east, east);
        assert_eq!(graph.nodes.len(), 3);

        assert_eq!(graph.position(origin), Some(Point::new(0.0, 0.0)));
        assert_eq!(graph.position(NodeId(99)), None);
    }

    #[test]
    fn snapping_ties_break_to_the_lowest_node_index() {
        let mut graph = RoadGraph::new(5.0);
        graph.add_node_snapped(Point::new(-1.0, 0.0));
        // Would be a second node without the tie rule, since the first snapped it.
        graph.nodes.push(RoadNode {
            position: Point::new(1.0, 0.0),
        });
        assert_eq!(graph.snap_target(Point::ORIGIN), Some(NodeId(0)));
    }

    #[test]
    fn segments_reject_self_loops_and_duplicates_in_either_direction() {
        let mut graph = RoadGraph::new(1.0);
        let hub = graph.add_node_snapped(Point::new(0.0, 0.0));
        let east = graph.add_node_snapped(Point::new(10.0, 0.0));
        let north = graph.add_node_snapped(Point::new(0.0, 10.0));

        assert!(graph.add_segment(hub, east, RoadClass::Street));
        assert!(!graph.add_segment(hub, east, RoadClass::Alley), "duplicate");
        assert!(
            !graph.add_segment(east, hub, RoadClass::Alley),
            "reversed duplicate"
        );
        assert!(!graph.add_segment(hub, hub, RoadClass::Alley), "self-loop");
        assert!(graph.add_segment(hub, north, RoadClass::Arterial));

        assert_eq!(graph.segments.len(), 2);
        assert_eq!(graph.degree(hub), 2);
        assert_eq!(graph.degree(east), 1);
    }

    #[test]
    fn an_empty_layout_carries_its_schema_version() {
        let layout = CityLayout::default();
        assert_eq!(layout.schema, LAYOUT_SCHEMA);
        assert!(layout.buildings.is_empty());
        assert_eq!(layout.vacant_lots().count(), 0);
        assert_eq!(layout.block(BlockId(0)), None);
        assert_eq!(layout.lot(LotId(0)), None);
        // The snapping tolerance is on the graph, so it survives a round trip
        // and cannot differ between generation and an incremental step.
        assert!((layout.roads.snap_tolerance - DEFAULT_SNAP_TOLERANCE).abs() < f32::EPSILON);
    }

    #[test]
    fn a_layout_round_trips_through_json() {
        let mut layout = CityLayout {
            extent: 512.0,
            ..CityLayout::default()
        };
        let path = polis_events::LogicalPath::new("src/main.rs").unwrap();
        layout.lots.push(Lot {
            id: LotId(0),
            block: BlockId(0),
            boundary: square(),
            occupant: Some(path.clone()),
        });
        layout.lots.push(Lot {
            id: LotId(1),
            block: BlockId(0),
            boundary: square(),
            occupant: None,
        });
        layout.buildings.insert(
            path.clone(),
            Building {
                path: path.clone(),
                lot: LotId(0),
                footprint: square(),
                height: 3.5,
                roof: RoofForm::Stepped,
                rotation: 0.05,
            },
        );

        let json = serde_json::to_string(&layout).expect("serializes");
        let back: CityLayout = serde_json::from_str(&json).expect("parses");
        assert_eq!(back.schema, LAYOUT_SCHEMA);
        assert!((back.extent - 512.0).abs() < f32::EPSILON);
        assert_eq!(back.vacant_lots().count(), 1);
        assert_eq!(
            back.building(&path).map(|b| b.roof),
            Some(RoofForm::Stepped)
        );
    }
}
