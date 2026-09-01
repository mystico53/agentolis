//! Step 3 — blocks (PRD §7.2).
//!
//! > **Blocks** are the closed loops (faces) in the resulting planar road graph.
//!
//! Face extraction, not polygon guessing: the block boundary *is* the road loop,
//! which is why irregular road growth yields irregular blocks for free.

use serde::{Deserialize, Serialize};

use crate::roads::RoadGraph;
use crate::Point;

/// A closed loop in the road graph. Contains lots (PRD §3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Block {
    /// Boundary in city space, wound consistently so subdivision is
    /// orientation-independent.
    pub boundary: Vec<Point>,
    /// Cached polygon area, used to clamp footprints (PRD §7.3).
    pub area: f32,
}

/// Extracts every face of the planar road graph.
///
/// Faces must be enumerated in a deterministic order — sort the half-edges
/// rather than relying on insertion order — or PRD §7.4 is violated in a way
/// that only surfaces as a golden-file diff on somebody else's machine.
pub fn extract(graph: &RoadGraph) -> Vec<Block> {
    let _ = graph;
    todo!("PRD §7.2 step 3 — planar face extraction, deterministically ordered")
}
