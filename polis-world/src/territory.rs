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
//!
//! # What is here and what is M4
//!
//! This module maintains the **evidence**: weighted, decaying kernels, the
//! convergence test, the claim and its hysteresis, and the drift vector. It does
//! not rasterise anything. The R16F density texture and its iso-contour bands
//! are PRD §10.4 and milestone M4, and they live in `polis-render` (ADR-0020
//! already fixed their thresholds); everything they need is
//! [`Territory::kernels`] plus [`Territory::bandwidth`].
//!
//! # The two gates, and which one binds
//!
//! PRD §6.2 emits on `depth(A) >= 2` **and** trimmed mass inside `A` above 0.7.
//! Both are implemented. In practice the depth gate is the one that bites: `A`
//! is the ancestor of the *retained* observations, so retained mass is inside it
//! by construction and the mass ratio starts at `1 - TRIM_FRACTION`. The mass
//! gate is a guard for skewed weights and for small `n`, where the trim count
//! rounds to zero and `A` is the untrimmed ancestor. Keeping both means retuning
//! [`TRIM_FRACTION`] cannot silently disable the check.

use std::collections::VecDeque;
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

/// Minimum ancestor depth before a territory may be emitted (PRD §6.2).
pub const MIN_CLAIM_DEPTH: usize = 2;

/// Fewest observations that can agree.
///
/// > Sometimes that is two observations, sometimes twelve.
///
/// One observation cannot *agree* with anything, and its own directory would
/// otherwise satisfy both gates instantly.
pub const MIN_OBSERVATIONS: usize = 2;

/// How many observations feed the convergence test.
///
/// The `k` of PRD §6.2's "last `k` weighted observations". Bounded so a
/// long-running thread's territory tracks what it is doing now rather than what
/// it did an hour ago; the decay already handles the rest.
pub const OBSERVATION_WINDOW: usize = 128;

/// Base kernel bandwidth as a fraction of [`polis_layout::CityLayout::extent`].
///
/// A fraction rather than an absolute, because the city's extent scales with the
/// repository and a fixed radius would be a haze on a hamlet and a dot on a
/// metropolis.
pub const BANDWIDTH_FRACTION: f32 = 0.08;

/// Narrowest and widest the bandwidth may get, as multiples of the base
/// (PRD §6.4's "clamped").
pub const BANDWIDTH_CLAMP: (f32, f32) = (0.25, 2.0);

/// Window the drift vector is measured over (PRD §10.4).
pub const DRIFT_WINDOW: Duration = Duration::from_secs(60);

/// How far the centre of mass must move, as a fraction of the current
/// bandwidth, before a leading-edge mark is drawn (PRD §10.4).
///
/// > render a leading-edge mark when its magnitude exceeds a threshold
pub const DRIFT_THRESHOLD: f32 = 0.5;

/// Weight below which a kernel stops contributing and is dropped.
const MIN_KERNEL_WEIGHT: f32 = 0.01;

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
    /// The weighted path evidence PRD §6.2's ancestor is taken over.
    ///
    /// Separate from [`Territory::kernels`] because an observation whose path
    /// has no building — a file outside the repository, a directory the layout
    /// has not seen — still counts as evidence of *scope* even though it has
    /// nowhere to splat.
    pub evidence: VecDeque<Evidence>,
    /// Bandwidth at one effective observation, in city units. Set by the world
    /// from the city's extent; zero means "not yet placed on a city".
    pub base_bandwidth: f32,
    /// Total observations ever made, undecayed. The denominator an operator
    /// reads as "how much does Polis actually know about this thread".
    pub observations: u64,
    /// When [`Territory::decay`] last ran.
    last_decay: Option<Instant>,
    /// Centre of mass over the drift window.
    com_history: VecDeque<(Instant, Point)>,
}

impl Territory {
    /// A territory sized to a city of the given extent.
    pub fn for_extent(extent: f32) -> Self {
        Self {
            base_bandwidth: extent * BANDWIDTH_FRACTION,
            ..Self::default()
        }
    }

    /// Adds an observation, applying tool weight and the ubiquity discount.
    ///
    /// Weights come from [`polis_events::ToolKind::evidence_weight`] (or
    /// [`Observation::weight`] for evidence with no row in PRD §6.1's table) and
    /// are scaled by `polis_repo::corpus::Corpus::ubiquity_discount`.
    ///
    /// `at` is `None` when the path has no geometry in the current city. The
    /// observation still counts toward convergence and toward the claim — it is
    /// evidence about *scope*, which is a statement about paths — but it drops
    /// no kernel, because inventing a position would put density where nothing
    /// is happening, which is the exact failure PRD §6 opens by rejecting.
    pub fn observe(&mut self, obs: &Observation, ubiquity_discount: f32, at: Option<Point>) {
        let weight = obs.base_weight() * ubiquity_discount;
        if weight <= 0.0 || !weight.is_finite() {
            // A tool with no path signal contributes nothing and must not drop a
            // zero-weight kernel.
            return;
        }
        self.decay(obs.at);
        self.observations = self.observations.saturating_add(1);

        let claim_path = obs.claim_path();
        // Hysteresis bookkeeping happens against the claim as it stands *before*
        // this observation, so an observation inside the claim resets the streak
        // and one outside extends it (PRD §6.3).
        match &self.claim {
            Some(claim) if claim_path.starts_with(claim) => self.outside_streak = 0,
            Some(_) => self.outside_streak = self.outside_streak.saturating_add(1),
            None => {}
        }

        self.evidence.push_back(Evidence {
            path: claim_path,
            weight,
            at: obs.at,
        });
        while self.evidence.len() > OBSERVATION_WINDOW {
            self.evidence.pop_front();
        }

        if let Some(centre) = at {
            let radius = self.bandwidth();
            self.kernels.push(Kernel {
                centre,
                weight,
                radius,
            });
            // The kernel list is bounded by the evidence window for the same
            // reason: a thread that has run for an hour must not carry an
            // unbounded splat list into a frame.
            while self.kernels.len() > OBSERVATION_WINDOW {
                self.kernels.remove(0);
            }
        }

        self.refresh_claim();
        self.refresh_centre_of_mass(obs.at);
    }

    /// Decays kernel weights toward `now` (PRD §6.3).
    ///
    /// Expand readily, contract slowly: nothing is removed abruptly, and a
    /// kernel only disappears once its weight has fallen below the point where
    /// it could tint a pixel.
    pub fn decay(&mut self, now: Instant) {
        let Some(last) = self.last_decay else {
            self.last_decay = Some(now);
            return;
        };
        let dt = now.saturating_duration_since(last);
        if dt.is_zero() {
            return;
        }
        self.last_decay = Some(now);
        let factor = decay_factor(dt);
        for k in &mut self.kernels {
            k.weight *= factor;
        }
        self.kernels.retain(|k| k.weight >= MIN_KERNEL_WEIGHT);
        for e in &mut self.evidence {
            e.weight *= factor;
        }
        self.evidence.retain(|e| e.weight >= MIN_KERNEL_WEIGHT);
        if self.evidence.is_empty() {
            // Nothing is left to claim. The thread returns to an unplaced
            // marker rather than keeping a stale district forever.
            self.claim = None;
            self.centre_of_mass = None;
            self.com_history.clear();
            self.outside_streak = 0;
        } else {
            self.refresh_centre_of_mass(now);
        }
    }

    /// Whether the observations **agree** yet (PRD §6.2).
    ///
    /// > Do not emit a territory after a fixed number of reads. Emit when the
    /// > observations agree […] Sometimes that is two observations, sometimes
    /// > twelve. Until then the thread renders with no cloud — an unplaced
    /// > marker in the status rail.
    pub fn has_converged(&self) -> bool {
        self.converged_claim().is_some()
    }

    /// The ancestor the current evidence supports, or `None` if it does not
    /// agree yet.
    ///
    /// Public because "why has this thread no cloud" is a question the
    /// drill-down layer has to be able to answer, and the honest answer is the
    /// pair of gates in [`Territory::convergence`].
    pub fn converged_claim(&self) -> Option<LogicalPath> {
        self.convergence().claim
    }

    /// The full state of PRD §6.2's test, for the drill-down layer.
    pub fn convergence(&self) -> Convergence {
        let n = self.evidence.len();
        let total: f32 = self.evidence.iter().map(|e| e.weight).sum();
        if n < MIN_OBSERVATIONS || total <= 0.0 {
            return Convergence {
                claim: None,
                observations: n,
                depth: 0,
                mass_ratio: 0.0,
                trimmed: 0,
            };
        }

        // Trim the lightest `TRIM_FRACTION` by count. Untrimmed, "one stray read
        // in `docs/` promotes the territory to repo root and claims the entire
        // city".
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by(|a, b| {
            self.evidence[*a]
                .weight
                .partial_cmp(&self.evidence[*b].weight)
                .unwrap_or(std::cmp::Ordering::Equal)
                // Ties break on path so the trim is deterministic (PRD §7.4).
                .then_with(|| self.evidence[*a].path.cmp(&self.evidence[*b].path))
        });
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        // n <= OBSERVATION_WINDOW = 128 and TRIM_FRACTION is positive, so the
        // product is exact in f32 and never negative.
        let trim = ((n as f32) * TRIM_FRACTION) as usize;
        let trim = trim.min(n.saturating_sub(MIN_OBSERVATIONS));
        let retained = &order[trim..];

        let mut ancestor: Option<LogicalPath> = None;
        for idx in retained {
            let path = &self.evidence[*idx].path;
            ancestor = Some(match ancestor {
                None => path.clone(),
                Some(a) => a.common_ancestor(path),
            });
        }
        let Some(ancestor) = ancestor else {
            return Convergence {
                claim: None,
                observations: n,
                depth: 0,
                mass_ratio: 0.0,
                trimmed: trim,
            };
        };

        // Mass is measured over *every* observation, trimmed ones included, so
        // the ratio answers "how much of what this thread did is inside the
        // ancestor" rather than restating the trim.
        let inside: f32 = self
            .evidence
            .iter()
            .filter(|e| e.path.starts_with(&ancestor))
            .map(|e| e.weight)
            .sum();
        let mass_ratio = inside / total;
        let depth = ancestor.depth();
        let claim = (depth >= MIN_CLAIM_DEPTH && mass_ratio > CONVERGENCE_MASS).then_some(ancestor);
        Convergence {
            claim,
            observations: n,
            depth,
            mass_ratio,
            trimmed: trim,
        }
    }

    /// The current bandwidth — the uncertainty knob (PRD §6.4).
    ///
    /// `base * (1 / sqrt(effective_n))`, clamped: wide and diffuse with three
    /// observations, tightening as evidence accumulates.
    ///
    /// `effective_n` is Kish's effective sample size, `(Σw)² / Σw²`, so one
    /// `Grep` at weight 5 does not count as five reads: it is one strong
    /// observation, and the uncertainty it removes is the uncertainty of one
    /// observation.
    pub fn bandwidth(&self) -> f32 {
        let base = if self.base_bandwidth > 0.0 {
            self.base_bandwidth
        } else {
            1.0
        };
        let n = self.effective_n();
        if n <= 0.0 {
            return base * BANDWIDTH_CLAMP.1;
        }
        let raw = base / n.sqrt();
        raw.clamp(base * BANDWIDTH_CLAMP.0, base * BANDWIDTH_CLAMP.1)
    }

    /// Kish's effective sample size over the live evidence.
    pub fn effective_n(&self) -> f32 {
        let sum: f32 = self.evidence.iter().map(|e| e.weight).sum();
        let sq: f32 = self.evidence.iter().map(|e| e.weight * e.weight).sum();
        if sq <= 0.0 {
            0.0
        } else {
            sum * sum / sq
        }
    }

    /// The drift vector over the last 60 s (PRD §10.4).
    ///
    /// > A territory whose centre of mass is migrating out of `src/auth` toward
    /// > `tests/` has a scope that is changing, and it is visible *while it is
    /// > happening* — well before contention fires. […] This is the redirect
    /// > signal, and it is the one thing here that no existing tool provides.
    ///
    /// `None` below the magnitude threshold, so a still territory draws no
    /// leading-edge mark at all. The returned [`Point`] is an offset in city
    /// units, not a position.
    pub fn drift(&self) -> Option<Point> {
        let now = self.centre_of_mass?;
        let (_, then) = *self.com_history.front()?;
        let dx = now.x - then.x;
        let dy = now.y - then.y;
        let magnitude = dx.hypot(dy);
        (magnitude > DRIFT_THRESHOLD * self.bandwidth()).then_some(Point::new(dx, dy))
    }

    /// Total live kernel weight — the field's mass, and the ranking key for
    /// PRD §10.4's cloud cap.
    pub fn mass(&self) -> f32 {
        self.kernels.iter().map(|k| k.weight).sum()
    }

    /// Applies PRD §6.3's hysteresis to the converged ancestor.
    ///
    /// * **Expand readily** — a candidate that contains the current claim, or is
    ///   contained by it, takes effect immediately.
    /// * **Move only on sustained evidence** — a candidate in a different
    ///   district waits for [`DRIFT_CONFIRMATIONS`] consecutive outside
    ///   observations. One read elsewhere moves nothing.
    fn refresh_claim(&mut self) {
        let Some(candidate) = self.converged_claim() else {
            return;
        };
        match self.claim.clone() {
            None => self.claim = Some(candidate),
            Some(current) => {
                if candidate == current {
                    return;
                }
                // Expanding into, or out of, the current claim is not a move: it
                // is the same district getting wider or narrower, and PRD §6.3
                // says that takes effect on the next update.
                let expanding = candidate.starts_with(&current) || current.starts_with(&candidate);
                if expanding || self.outside_streak >= DRIFT_CONFIRMATIONS {
                    self.claim = Some(candidate);
                    self.outside_streak = 0;
                }
            }
        }
    }

    /// Recomputes the weighted centre of mass and rolls the drift window.
    fn refresh_centre_of_mass(&mut self, now: Instant) {
        let total: f32 = self.kernels.iter().map(|k| k.weight).sum();
        if total <= 0.0 {
            self.centre_of_mass = None;
            return;
        }
        let x: f32 = self.kernels.iter().map(|k| k.centre.x * k.weight).sum();
        let y: f32 = self.kernels.iter().map(|k| k.centre.y * k.weight).sum();
        let com = Point::new(x / total, y / total);
        self.centre_of_mass = Some(com);
        self.com_history.push_back((now, com));
        while self
            .com_history
            .front()
            .is_some_and(|(t, _)| now.saturating_duration_since(*t) > DRIFT_WINDOW)
        {
            // Keep exactly one sample at or beyond the window edge so the drift
            // vector always spans the full 60 s once the thread is that old.
            if self
                .com_history
                .get(1)
                .is_some_and(|(t, _)| now.saturating_duration_since(*t) > DRIFT_WINDOW)
            {
                self.com_history.pop_front();
            } else {
                break;
            }
        }
    }
}

/// The state of PRD §6.2's convergence test, for the drill-down layer.
///
/// > **Fuzzy above, exact below.** Soft cloud edges are honest for the ambient
/// > layer and useless when acting. (PRD §12)
#[derive(Debug, Clone)]
pub struct Convergence {
    /// The ancestor, when both gates pass.
    pub claim: Option<LogicalPath>,
    /// How many observations are live.
    pub observations: usize,
    /// `depth(A)`. Must reach [`MIN_CLAIM_DEPTH`].
    pub depth: usize,
    /// Mass inside `A` over total. Must exceed [`CONVERGENCE_MASS`].
    pub mass_ratio: f32,
    /// How many observations the trim dropped.
    pub trimmed: usize,
}

/// One weighted path observation, after the ubiquity discount and any decay.
#[derive(Debug, Clone)]
pub struct Evidence {
    /// The **directory** claimed: a file observation is folded to its parent so
    /// a single file cannot satisfy `depth(A) >= 2` on its own.
    pub path: LogicalPath,
    /// Current weight, after decay.
    pub weight: f32,
    /// When it was observed.
    pub at: Instant,
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

/// The multiplier a weight loses over `dt` at a 90 s half-life.
pub fn decay_factor(dt: Duration) -> f32 {
    #[allow(clippy::cast_precision_loss)] // seconds of a live session; far inside f32
    let half_lives = dt.as_secs_f32() / DECAY_HALF_LIFE.as_secs_f32();
    0.5_f32.powf(half_lives)
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
///
/// A territory that has not converged is never returned: until then the thread
/// renders as an unplaced marker in the status rail, not as a cloud (PRD §6.2).
pub fn visible_clouds<'a>(
    territories: &'a [(&'a crate::Thread, &'a Territory)],
    cap: usize,
) -> Vec<&'a Territory> {
    let mut ranked: Vec<&(&crate::Thread, &Territory)> = territories
        .iter()
        .filter(|(_, t)| t.claim.is_some() && !t.kernels.is_empty())
        .collect();
    ranked.sort_by(|(ta, _), (tb, _)| {
        // An active attention state first — a waiting thread is the one the
        // operator is looking for — then most recent activity.
        ta.status
            .rail_rank()
            .cmp(&tb.status.rail_rank())
            .then(tb.last_activity.cmp(&ta.last_activity))
            .then(ta.id.cmp(&tb.id))
    });
    ranked.into_iter().take(cap).map(|(_, t)| *t).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Observation, PathScope};
    use polis_events::{SessionId, ThreadId, ToolKind};

    fn lp(s: &str) -> LogicalPath {
        LogicalPath::new(s).expect("test path")
    }

    fn obs(path: &str, tool: ToolKind, at: Instant) -> Observation {
        Observation {
            thread: ThreadId::of_session(SessionId::new("s")),
            worker: None,
            path: lp(path),
            scope: PathScope::for_tool(&tool),
            tool,
            at,
            weight: None,
        }
    }

    #[allow(clippy::unnecessary_wraps)] // every call site passes an `Option<Point>`
    fn place(n: u32) -> Option<Point> {
        #[allow(clippy::cast_precision_loss)] // small test indices
        Some(Point::new(n as f32, 0.0))
    }

    #[test]
    fn one_observation_is_not_agreement() {
        let mut t = Territory::for_extent(1000.0);
        let now = Instant::now();
        t.observe(
            &obs("src/auth/token.rs", ToolKind::Read, now),
            1.0,
            place(0),
        );
        assert!(
            !t.has_converged(),
            "one observation cannot agree with anything"
        );
        assert!(t.claim.is_none(), "and a thread with no claim has no cloud");
    }

    #[test]
    fn two_agreeing_observations_converge() {
        // "Sometimes that is two observations, sometimes twelve."
        let mut t = Territory::for_extent(1000.0);
        let now = Instant::now();
        t.observe(
            &obs("src/auth/token.rs", ToolKind::Read, now),
            1.0,
            place(0),
        );
        t.observe(
            &obs("src/auth/session.rs", ToolKind::Edit, now),
            1.0,
            place(1),
        );
        assert!(t.has_converged());
        assert_eq!(t.claim.as_ref().map(LogicalPath::as_str), Some("src/auth"));
    }

    #[test]
    fn a_shallow_ancestor_does_not_emit() {
        // depth(A) >= 2, so two files in different top-level directories claim
        // the root and must not emit.
        let mut t = Territory::for_extent(1000.0);
        let now = Instant::now();
        t.observe(&obs("src/a.rs", ToolKind::Read, now), 1.0, place(0));
        t.observe(&obs("docs/b.md", ToolKind::Read, now), 1.0, place(1));
        assert!(!t.has_converged());
        assert_eq!(t.convergence().depth, 0);
    }

    #[test]
    fn trimming_stops_one_stray_read_from_claiming_the_city() {
        // The failure PRD §6.2 names explicitly: "one stray read in `docs/`
        // promotes the territory to repo root and claims the entire city".
        let mut t = Territory::for_extent(1000.0);
        let now = Instant::now();
        for i in 0..9 {
            t.observe(
                &obs(&format!("src/auth/f{i}.rs"), ToolKind::Edit, now),
                1.0,
                place(i),
            );
        }
        // The stray is the lightest observation on the list (Bash, 0.5).
        t.observe(&obs("docs", ToolKind::Bash, now), 1.0, place(99));
        let c = t.convergence();
        assert!(c.trimmed >= 1, "the stray must be trimmed");
        assert_eq!(c.claim.as_ref().map(LogicalPath::as_str), Some("src/auth"));
        assert!(c.mass_ratio > CONVERGENCE_MASS);
    }

    #[test]
    fn bandwidth_is_the_uncertainty_knob() {
        let mut t = Territory::for_extent(1000.0);
        let base = 1000.0 * BANDWIDTH_FRACTION;
        let now = Instant::now();
        assert!(
            (t.bandwidth() - base * BANDWIDTH_CLAMP.1).abs() < 1e-3,
            "with no evidence the field is as wide as it is allowed to be"
        );
        for i in 0..3 {
            t.observe(
                &obs(&format!("src/auth/f{i}.rs"), ToolKind::Read, now),
                1.0,
                place(i),
            );
        }
        let wide = t.bandwidth();
        for i in 3..40 {
            t.observe(
                &obs(&format!("src/auth/f{i}.rs"), ToolKind::Read, now),
                1.0,
                place(i),
            );
        }
        let tight = t.bandwidth();
        assert!(
            tight < wide,
            "more evidence tightens the field: {tight} < {wide}"
        );
        assert!(tight >= base * BANDWIDTH_CLAMP.0, "and the clamp holds");
    }

    #[test]
    fn one_strong_observation_is_one_observation() {
        // Kish, not a weight sum: a single Grep at weight 5 must not pretend to
        // be five reads' worth of certainty.
        let mut a = Territory::for_extent(1000.0);
        let mut b = Territory::for_extent(1000.0);
        let now = Instant::now();
        a.observe(&obs("src/auth", ToolKind::Grep, now), 1.0, place(0));
        a.observe(&obs("src/auth", ToolKind::Grep, now), 1.0, place(1));
        for i in 0..2 {
            b.observe(
                &obs(&format!("src/auth/f{i}.rs"), ToolKind::Read, now),
                1.0,
                place(i),
            );
        }
        assert!((a.effective_n() - b.effective_n()).abs() < 1e-3);
    }

    #[test]
    fn weights_halve_over_the_half_life() {
        let mut t = Territory::for_extent(1000.0);
        let t0 = Instant::now();
        t.observe(&obs("src/auth/a.rs", ToolKind::Edit, t0), 1.0, place(0));
        t.observe(&obs("src/auth/b.rs", ToolKind::Edit, t0), 1.0, place(1));
        let before = t.mass();
        t.decay(t0 + DECAY_HALF_LIFE);
        let after = t.mass();
        assert!(
            (after / before - 0.5).abs() < 0.01,
            "90 s half-life: {before} -> {after}"
        );
        // Contract slowly: nothing was removed abruptly.
        assert_eq!(t.kernels.len(), 2);
        assert!(t.claim.is_some());
    }

    #[test]
    fn a_dormant_territory_dissipates_entirely() {
        let mut t = Territory::for_extent(1000.0);
        let t0 = Instant::now();
        t.observe(&obs("src/auth/a.rs", ToolKind::Edit, t0), 1.0, place(0));
        t.observe(&obs("src/auth/b.rs", ToolKind::Edit, t0), 1.0, place(1));
        assert!(t.claim.is_some());
        t.decay(t0 + DECAY_HALF_LIFE * 12);
        assert!(t.kernels.is_empty());
        assert!(t.claim.is_none(), "back to an unplaced marker");
    }

    #[test]
    fn expanding_is_immediate_and_moving_district_needs_sustained_evidence() {
        let mut t = Territory::for_extent(1000.0);
        let now = Instant::now();
        for i in 0..8 {
            t.observe(
                &obs(&format!("src/auth/f{i}.rs"), ToolKind::Edit, now),
                1.0,
                place(i),
            );
        }
        assert_eq!(t.claim.as_ref().map(LogicalPath::as_str), Some("src/auth"));

        // One read elsewhere moves nothing.
        t.observe(&obs("tests/a.rs", ToolKind::Edit, now), 1.0, place(50));
        assert_eq!(
            t.claim.as_ref().map(LogicalPath::as_str),
            Some("src/auth"),
            "one read elsewhere moves nothing"
        );
        assert_eq!(t.outside_streak, 1);

        // Sustained evidence does. Enough of it to also carry the mass gate.
        for i in 0..40 {
            t.observe(
                &obs(&format!("tests/unit/f{i}.rs"), ToolKind::Edit, now),
                1.0,
                place(100 + i),
            );
        }
        assert_eq!(
            t.claim.as_ref().map(LogicalPath::as_str),
            Some("tests/unit"),
            "sustained evidence moves the district"
        );
    }

    #[test]
    fn an_observation_with_no_geometry_still_counts_as_scope() {
        let mut t = Territory::for_extent(1000.0);
        let now = Instant::now();
        t.observe(&obs("src/auth/a.rs", ToolKind::Edit, now), 1.0, None);
        t.observe(&obs("src/auth/b.rs", ToolKind::Edit, now), 1.0, None);
        assert!(t.has_converged(), "scope is a statement about paths");
        assert!(t.kernels.is_empty(), "but nothing is splatted");
        assert!(t.centre_of_mass.is_none());
        assert!(t.drift().is_none());
    }

    #[test]
    fn a_zero_weight_tool_drops_no_kernel() {
        let mut t = Territory::for_extent(1000.0);
        let now = Instant::now();
        // `WebSearch` has no path signal and weight 0.0.
        t.observe(
            &obs("src/auth/a.rs", ToolKind::WebSearch, now),
            1.0,
            place(0),
        );
        assert!(t.kernels.is_empty());
        assert_eq!(t.observations, 0);
    }

    #[test]
    fn drift_appears_only_once_the_centre_actually_moves() {
        let mut t = Territory::for_extent(100.0);
        let t0 = Instant::now();
        for i in 0..6 {
            t.observe(
                &obs(&format!("src/auth/f{i}.rs"), ToolKind::Edit, t0),
                1.0,
                place(0),
            );
        }
        assert!(t.drift().is_none(), "a still territory drifts nowhere");
        let later = t0 + Duration::from_secs(30);
        for i in 0..40 {
            t.observe(
                &obs(&format!("src/auth/g{i}.rs"), ToolKind::Edit, later),
                1.0,
                Some(Point::new(400.0, 0.0)),
            );
        }
        let d = t.drift().expect("the centre of mass has moved");
        assert!(d.x > 0.0, "and the leading edge points where it went");
    }

    #[test]
    fn clouds_are_capped_and_waiting_threads_come_first() {
        use crate::{Thread, ThreadStatus};
        let now = Instant::now();
        let mk = |name: &str, status: ThreadStatus, activity: Instant| {
            let mut th = Thread::new(
                ThreadId::of_session(SessionId::new(name)),
                SessionId::new(name),
                now,
            );
            th.status = status;
            th.last_activity = activity;
            th.territory = Territory::for_extent(1000.0);
            for i in 0..4 {
                th.territory.observe(
                    &obs(&format!("src/{name}/f{i}.rs"), ToolKind::Edit, now),
                    1.0,
                    place(i),
                );
            }
            th
        };
        let busy = mk("busy", ThreadStatus::Working, now + Duration::from_secs(9));
        let waiting = mk("waiting", ThreadStatus::Waiting, now);
        let quiet = mk("quiet", ThreadStatus::Idle, now);
        let pairs = [
            (&busy, &busy.territory),
            (&waiting, &waiting.territory),
            (&quiet, &quiet.territory),
        ];
        let visible = visible_clouds(&pairs, 2);
        assert_eq!(visible.len(), 2, "the cap holds");
        assert_eq!(
            visible[0].claim.as_ref().map(LogicalPath::as_str),
            Some("src/waiting"),
            "an active attention state outranks recent activity"
        );

        // An unconverged territory is never a cloud.
        let mut blank = mk("blank", ThreadStatus::Working, now);
        blank.territory = Territory::for_extent(1000.0);
        let pairs = [(&blank, &blank.territory)];
        assert!(visible_clouds(&pairs, 4).is_empty());
    }
}
