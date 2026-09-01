//! Step 5 — buildings (PRD §7.2, §7.3).
//!
//! > **Buildings** are lots inset by a setback, with a small random rotation
//! > (±4°).
//!
//! # Attributes
//!
//! * **Footprint area** proportional to `sqrt(file_size_bytes)`, clamped to
//!   `[min_lot, block_area * 0.6]`.
//! * **Height** proportional to uncommitted diff lines. The city rises as agents
//!   work and settles when you merge; the tallest thing on the map is the
//!   biggest unreviewed pile, which directly serves "where do I need to look".
//! * **Silhouette variety** carries most of the organic reading and costs
//!   nothing: vary roof form by a hash of the path.
//!
//! Height's input already exists three ways in the transcript and must not be
//! re-derived by diffing the working tree — see
//! [`polis_repo::git::diff_line_counts`] and ADR-0004.

use serde::{Deserialize, Serialize};

use polis_events::LogicalPath;

use crate::lots::Lot;
use crate::Point;

/// A file, rendered (PRD §3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Building {
    /// Footprint, ready for `lyon` to triangulate once and cache in a vertex
    /// buffer (PRD §13).
    pub footprint: Vec<Point>,
    /// Current height. Tweened between updates rather than snapped — that
    /// interpolation is "the entire difference between alive and steppy"
    /// (PRD §13).
    pub height: f32,
    /// Roof form, from a hash of the path.
    pub roof: RoofForm,
    /// Rotation in radians, within ±4°.
    pub rotation: f32,
}

/// Silhouette variety (PRD §7.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RoofForm {
    /// Flat.
    Flat,
    /// Stepped.
    Stepped,
    /// Pitched.
    Pitched,
}

/// Places a building on a lot.
///
/// Every draw is seeded from `path`, never from a shared stream (PRD §7.4).
pub fn place(lot: &Lot, path: &LogicalPath, size_bytes: u64, block_area: f32) -> Building {
    let _ = (lot, path, size_bytes, block_area);
    todo!("PRD §7.3 — inset by setback, area from sqrt(size), roof and rotation from the path")
}

/// Height for a given uncommitted diff size (PRD §7.3).
///
/// Open question 4 in PRD §17: the city flattens on merge, which is satisfying
/// but may destroy the "recently active" reading. If a slow-decay ghost is added,
/// it belongs here rather than in the renderer, so the golden-file test covers it.
pub fn height_for_diff_lines(diff_lines: u32) -> f32 {
    let _ = diff_lines;
    todo!("PRD §7.3 — height proportional to uncommitted diff lines")
}
