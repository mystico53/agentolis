//! Territory inference (PRD §6).
//!
//! > A main agent has no meaningful point location — it delegates rather than
//! > edits. Computing a centroid of its workers is actively wrong: an
//! > orchestrator with workers in `src/auth` and `tests/` gets a centroid in the
//! > empty gap between them, which is the one place nothing is happening.
//!
//! > A territory is a **density field over the layout**, estimated from sparse
//! > path observations.
//!
//! Everything the PRD wants falls out of that single choice: bandwidth is the
//! uncertainty knob, multi-lobed shapes come free, overlap is field addition
//! (which is exactly the contention signal), and hysteresis is temporal
//! smoothing on the kernel weights.

use std::time::{Duration, Instant};

use polis_events::LogicalPath;
use polis_layout::Point;

use crate::Observation;

/// Kernel weight half-life (PRD §6.3).
///
/// > **Contract slowly**: kernel weights decay with a half-life of 90s. Nothing
/// > is removed abruptly.
pub const DECAY_HALF_LIFE: Duration = Duration::from_secs(90);

/// Consecutive outside observations required before the centre of mass may move
/// districts (PRD §6.3).
///
/// > **Move only on sustained evidence** […] One read elsewhere moves nothing.
pub const DRIFT_CONFIRMATIONS: u32 = 8;

/// Fraction of trimmed weight mass that must sit inside the lowest common
/// ancestor before a territory is emitted (PRD §6.2).
pub const CONVERGENCE_MASS: f32 = 0.7;

/// Fraction of the lightest observations trimmed before taking the ancestor.
///
/// > Untrimmed LCA is fragile: one stray read in `docs/` promotes the territory
/// > to repo root and claims the entire city.
pub const TRIM_FRACTION: f32 = 0.2;

/// The inferred scope of a main agent (PRD §3, §6).
#[derive(Debug, Clone, Default)]
pub struct Territory {
    /// Live kernels. Empty until convergence.
    pub kernels: Vec<Kernel>,
    /// The claimed ancestor directory, once [`Territory::has_converged`].
    pub claim: Option<LogicalPath>,
    /// Centre of mass over the last 60 s, and its predecessor, from which the
    /// drift vector is computed.
    pub centre_of_mass: Option<Point>,
    /// Consecutive observations that fell outside the current claim.
    pub outside_streak: u32,
}

impl Territory {
    /// Adds an observation, applying tool weight and the ubiquity discount.
    ///
    /// Weights come from [`polis_events::ToolKind::evidence_weight`] and are
    /// scaled by `polis_repo::corpus::Corpus::ubiquity_discount`.
    pub fn observe(&mut self, obs: &Observation, ubiquity_discount: f32, at: Point) {
        let _ = (obs, ubiquity_discount, at);
        todo!("PRD §6.1 — weighted kernel drop at the path's layout position")
    }

    /// Decays kernel weights toward `now` (PRD §6.3).
    pub fn decay(&mut self, now: Instant) {
        let _ = now;
        todo!("PRD §6.3 — 90 s half-life; expand readily, contract slowly")
    }

    /// Whether the observations **agree** yet (PRD §6.2).
    ///
    /// > Do not emit a territory after a fixed number of reads. Emit when the
    /// > observations agree […] Sometimes that is two observations, sometimes
    /// > twelve. Until then the thread renders with no cloud — an unplaced
    /// > marker in the status rail.
    pub fn has_converged(&self) -> bool {
        todo!("PRD §6.2 — trimmed LCA with depth >= 2 and mass > 0.7")
    }

    /// The current bandwidth — the uncertainty knob (PRD §6.4).
    ///
    /// `base * (1 / sqrt(effective_n))`, clamped: wide and diffuse with three
    /// observations, tightening as evidence accumulates.
    pub fn bandwidth(&self) -> f32 {
        todo!("PRD §6.4 — bandwidth = base / sqrt(effective_n), clamped")
    }

    /// The drift vector over the last 60 s (PRD §10.4).
    ///
    /// > A territory whose centre of mass is migrating out of `src/auth` toward
    /// > `tests/` has a scope that is changing, and it is visible *while it is
    /// > happening* — well before contention fires. […] This is the redirect
    /// > signal, and it is the one thing here that no existing tool provides.
    ///
    /// `None` below the magnitude threshold, so a still territory draws no
    /// leading-edge mark at all.
    pub fn drift(&self) -> Option<Point> {
        todo!("PRD §10.4 — centre of mass over 60 s, thresholded")
    }
}

/// One Gaussian kernel in the density field (PRD §6.4).
#[derive(Debug, Clone, Copy)]
pub struct Kernel {
    /// Where, in city space.
    pub centre: Point,
    /// Current weight, after decay.
    pub weight: f32,
    /// Radius, from [`Territory::bandwidth`].
    pub radius: f32,
}

/// Chooses which territories get a cloud this frame (PRD §10.4).
///
/// > **Cap the number of visible clouds.** Forty threads means forty systems and
/// > the map vanishes under haze. Render clouds only for threads with an active
/// > attention state or in the top N by recent activity; let dormant territories
/// > dissipate entirely.
///
/// PRD §17 lists the right cap as an open question, so this is a policy function
/// with one caller rather than a constant sprinkled through the renderer.
pub fn visible_clouds<'a>(
    territories: &'a [(&'a crate::Thread, &'a Territory)],
    cap: usize,
) -> Vec<&'a Territory> {
    let _ = (territories, cap);
    todo!("PRD §10.4 — attention state first, then top N by recent activity")
}
