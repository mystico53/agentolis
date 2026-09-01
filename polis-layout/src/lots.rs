//! Step 4 — lots (PRD §7.2).
//!
//! > **Lots** by recursive subdivision of each block along its longest axis
//! > until lot area falls under target. Irregular blocks give irregular lots for
//! > free.

use serde::{Deserialize, Serialize};

use crate::blocks::Block;
use crate::Point;

/// The parcel a building sits on (PRD §3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lot {
    /// Parcel boundary.
    pub boundary: Vec<Point>,
    /// Parcel area.
    pub area: f32,
}

/// Subdivides one block into lots.
///
/// The split axis is the block's longest, and the split position is drawn from a
/// path-seeded generator, so the same block always subdivides the same way
/// (PRD §7.4).
pub fn subdivide(block: &Block, target_area: f32, seed: u64) -> Vec<Lot> {
    let _ = (block, target_area, seed);
    todo!("PRD §7.2 step 4 — recursive split along the longest axis")
}
