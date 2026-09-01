//! Step 1 — the terrain field (PRD §7.2).
//!
//! > Low-frequency simplex noise, biased by directory depth (deeper = "higher
//! > ground"). **Never rendered directly.** Its only job is to give roads
//! > contours to follow, so curvature looks justified rather than randomly
//! > wiggled. Cheapest source of organic-ness available.
//!
//! Layer 1 of PRD §10.3 is "terrain / vacant lots — barely visible"; that is the
//! vacant lots, not this field. The field itself is an input to [`crate::roads`]
//! and nothing else.

use crate::Point;

/// A scalar height field over city space.
#[derive(Debug, Clone, Default)]
pub struct TerrainField {
    _private: (),
}

impl TerrainField {
    /// Builds the field for a repository.
    ///
    /// Seeded from the repository root path, never from the clock (PRD §7.4).
    pub fn generate(seed: u64, extent: f32) -> Self {
        let _ = (seed, extent);
        todo!("PRD §7.2 — low-frequency simplex, biased by directory depth")
    }

    /// Height at a point.
    pub fn height(&self, at: Point) -> f32 {
        let _ = at;
        todo!("PRD §7.2")
    }

    /// Downhill gradient at a point.
    ///
    /// Road segments follow this where the slope exceeds a threshold, which is
    /// what makes the curvature read as justified rather than decorative.
    pub fn gradient(&self, at: Point) -> Point {
        let _ = at;
        todo!("PRD §7.2 — segments follow the gradient above a slope threshold")
    }
}
