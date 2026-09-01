//! Step 2 — road growth by space colonization (PRD §7.2).
//!
//! > Scatter attraction points weighted by where files need to be reachable.
//! > Grow segments toward unclaimed points, consuming points within a kill
//! > radius. Segments follow the terrain gradient where the slope exceeds a
//! > threshold.
//!
//! # The organic signature
//!
//! > when a new segment's endpoint lands within `snap_radius` of an existing
//! > intersection, snap to it rather than creating a new node. Those irregular
//! > four- and five-way junctions are what the eye reads as "grown." **Without
//! > snapping you get a tree, and trees read as artificial.**
//!
//! Snapping is therefore not an optimisation and not optional: it is the single
//! rule that makes the whole look work.

use serde::{Deserialize, Serialize};

use crate::terrain::TerrainField;
use crate::Point;

/// A planar road graph. Its faces are the blocks.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RoadGraph {
    /// Intersections.
    pub nodes: Vec<Point>,
    /// Segments, as index pairs into [`RoadGraph::nodes`].
    pub edges: Vec<(u32, u32)>,
}

impl RoadGraph {
    /// Grows the network over a terrain field.
    ///
    /// `attractors` are weighted by where files need to be reachable, so the
    /// network follows the directory tree's shape without the import graph
    /// fighting it for position (PRD §9).
    pub fn grow(terrain: &TerrainField, attractors: &[(Point, f32)], params: GrowthParams) -> Self {
        let _ = (terrain, attractors, params);
        todo!("PRD §7.2 — space colonization with intersection snapping")
    }

    /// Runs one incremental growth step for newly-added attractors.
    pub fn grow_incremental(&mut self, attractors: &[(Point, f32)], params: GrowthParams) {
        let _ = (attractors, params);
        todo!("PRD §7.4 — a new file runs one growth step, not a regeneration")
    }
}

/// Tunables for road growth. Grouped so the golden-file test can pin them.
#[derive(Debug, Clone, Copy)]
pub struct GrowthParams {
    /// Distance at which an attractor is consumed.
    pub kill_radius: f32,
    /// **The organic signature.** Endpoints landing within this of an existing
    /// intersection snap to it instead of creating a node.
    pub snap_radius: f32,
    /// Length of one grown segment.
    pub segment_length: f32,
    /// Above this terrain slope, segments follow the gradient rather than the
    /// straight line to their attractor.
    pub slope_threshold: f32,
}
