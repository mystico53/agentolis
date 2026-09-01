//! `polis-layout` — city generation (PRD §7, §14).
//!
//! > The look comes from treating this as a growth process under constraints,
//! > not a layout algorithm. Irregularity should be the residue of history, not
//! > noise sprinkled on a grid.
//!
//! # The pipeline order is non-negotiable
//!
//! Roads → blocks → lots → buildings ([`terrain`], [`roads`], [`blocks`],
//! [`lots`], [`buildings`]). This is the Parish & Müller ordering.
//!
//! > Place buildings first and connect them afterward and you get suburbia or a
//! > circuit board, every time.
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
//! which PRD §16 calls the most important test in the suite. [`determinism`]
//! holds the rules and the seeded generator; use `BTreeMap` everywhere iteration
//! order can reach the output.

pub mod blocks;
pub mod buildings;
pub mod determinism;
pub mod lots;
pub mod roads;
pub mod terrain;

use std::collections::BTreeMap;

use polis_events::LogicalPath;
use polis_repo::{RepoDelta, RepoTree};

/// A point in city space. City space is unitless and stable across runs.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Point {
    /// Horizontal.
    pub x: f32,
    /// Vertical.
    pub y: f32,
}

/// The generated city (PRD §5, `World::layout`).
///
/// Changes rarely, and is serialised whole for the golden-file test.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct CityLayout {
    /// The road graph. Its faces are the blocks.
    pub roads: roads::RoadGraph,
    /// Closed loops in the road graph.
    pub blocks: Vec<blocks::Block>,
    /// One building per file, keyed by logical path so a worktree adds no
    /// geometry (PRD §7.6).
    pub buildings: BTreeMap<LogicalPath, buildings::Building>,
    /// Districts — directories, rendered as regions. The district skeleton must
    /// stay readable at every zoom even while the street level tangles (PRD §8).
    pub districts: BTreeMap<LogicalPath, District>,
    /// Lots whose file was deleted. They go to seed rather than vanishing
    /// (PRD §7.5).
    pub vacant: Vec<lots::Lot>,
}

/// A directory, rendered as a region (PRD §3).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct District {
    /// The directory this district is.
    pub path: LogicalPath,
    /// Boundary polygon in city space.
    pub boundary: Vec<Point>,
    /// Centroid, for label placement and for the territory kernel positions
    /// PRD §6.4 splats.
    pub centre: Point,
}

/// Runs and incrementally maintains the layout.
///
/// > **Layout runs off the render thread**, incrementally, never inside a frame.
/// > (PRD §13)
#[derive(Debug)]
pub struct LayoutEngine {
    _private: (),
}

impl LayoutEngine {
    /// Generates a city from scratch by replaying the growth sequence.
    ///
    /// PRD §13.1 budgets cold start → first frame under 3 s for a 5 000-file
    /// repo; PRD §16 requires this to be byte-identical across two runs and two
    /// operating systems before any live data is wired in (milestone M1).
    pub fn generate(tree: &RepoTree) -> CityLayout {
        let _ = tree;
        todo!("PRD §7.2 — terrain, roads, blocks, lots, buildings, in that order")
    }

    /// Applies one growth step for a repository delta.
    ///
    /// Budget: under 50 ms, off-thread (PRD §13.1).
    pub fn grow(&mut self, layout: &mut CityLayout, delta: &RepoDelta) {
        let _ = (layout, delta);
        todo!("PRD §7.4 — one growth step per new file, never a regeneration")
    }
}

/// Batches layout changes so the ground never moves under the operator's eye.
///
/// > **Never move the ground while the operator is looking at it.** Batch layout
/// > changes, apply them on a slow tween (≥800ms), and prefer to defer until the
/// > camera has been still for a moment. A map that rearranges mid-read is worse
/// > than one that is slightly stale. (PRD §7.7)
#[derive(Debug, Default)]
pub struct DeformationLimiter {
    _private: (),
}

impl DeformationLimiter {
    /// Queues a change and reports whether it may be applied now.
    pub fn submit(&mut self, delta: RepoDelta, camera_still_for: std::time::Duration) -> bool {
        let _ = (delta, camera_still_for);
        todo!("PRD §7.7 — ≥800 ms tween, prefer a still camera")
    }
}
