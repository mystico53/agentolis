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

/// Coarsest spacing between drift samples.
///
/// The trace exists to say whether the offset has been *pointing the same way*,
/// which needs tens of samples, not thousands. Without this, a burst at PRD
/// §13.1's 500 events/sec budget would push 30 000 samples per thread into a
/// 60 s window to answer a question that needs sixty.
pub const DRIFT_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// How far the last minute's work must sit from the established field, as a
/// multiple of the current bandwidth, before a leading-edge mark is drawn
/// (PRD §10.4).
///
/// > render a leading-edge mark when its magnitude exceeds a threshold
///
/// **Measured, not chosen**, over sixteen real sessions of the operator's
/// busiest repository — and the measurement is not flattering. See
/// [`DRIFT_VERDICT`] before relying on this mark for anything.
///
/// One is the value that maximised measured lift; it puts the mark up for 7.0 %
/// of the samples where a territory has a cloud at all, against 10.2 % at 0.25
/// and 2.3 % at 4.0.
pub const DRIFT_THRESHOLD: f32 = 1.0;

/// What the drift mark is actually worth, measured (PRD §17).
///
/// > **Test for every visual element: does it change a decision?** If not, cut
/// > it. (PRD §17)
///
/// `polis-world/tests/drift_on_real_sessions.rs` runs that test over sixteen of
/// the operator's real sessions in `C:\coding\qurio-toolset`, replayed at 5 s
/// resolution, 7 994 samples with a converged territory. It asks: when the mark
/// is lit, does the agent's district actually change within the next five
/// minutes?
///
/// | ratio | base rate | precision | firing samples | lift |
/// |---|---|---|---|---|
/// | 0.25 | 30.4 % | 27.9 % | 567 | 0.92 |
/// | 0.50 | 30.4 % | 31.7 % | 482 | 1.04 |
/// | **1.00** | 30.4 % | **34.6 %** | 361 | **1.14** |
/// | 2.00 | 30.4 % | 30.7 % | 179 | 1.01 |
/// | 4.00 | 30.4 % | 29.2 % | 106 | 0.96 |
///
/// Re-run over the six largest sessions alone it is 1.05 (base 32.0 %, precision
/// 33.7 %, 193 firing samples), so 1.14 is not even stable.
///
/// **A lift of 1.14, on autocorrelated samples, is not a signal.** The mark is
/// right about where the newest work is — that part is arithmetic — but it does
/// not predict a redirect, and PRD §10.4 calls it *"the redirect signal"*.
///
/// The reason is in the base rate, and it is the more interesting finding: on
/// this operator's real sessions an agent's district changes laterally within
/// any five-minute window **30 % of the time**. Scope change is the normal state
/// of a Claude Code session, not an event. An alarm cannot be rare and
/// consequential about something that common.
///
/// Two things the measurement does *not* establish, and which would change the
/// verdict:
///
/// * PRD §10.4's actual claim is that drift fires *"well before contention
///   fires"*. Contention never fired on this corpus — single sessions, and the
///   `ClaimTable` is keyed by thread — so the comparison the PRD makes could not
///   be run at all.
/// * The corpus is single-agent replays. The scenario the PRD describes is an
///   orchestrator with workers fanning out, which is what the cap and the
///   attention layer are for and which no recording here contains.
///
/// It also records what the redefinition bought. With the first reading of
/// §10.4 — the centre of mass now against the centre of mass 60 s ago — the mark
/// arrived *after* the territory's own claim had moved (`f47bd4e7`: claim
/// `src/config → src/components/Minimap` at 9 490 s, first drift episode
/// 9 575 s; `dfa8cd66`: claim move 26 173 s, first episode 26 231 s). Under
/// recent-against-established the same sessions fire before the move
/// (`f47bd4e7`: 890 s before; `1c7dbd60`: 240 s and 223 s before two of its
/// five moves). The direction is right; the discrimination is not there.
pub const DRIFT_VERDICT: &str = "lift 1.14 over a 30.4% base rate; not a predictor (PRD §17)";

/// How consistently the offset must point one way before it counts as a
/// migration.
///
/// [`Drift::coherence`] is the length of the mean unit offset over the trace —
/// one when every sample pointed the same way, near zero when the offset has
/// been swinging around the field. An agent ping-ponging between two directories
/// produces a large offset that alternates direction; that is thrash, PRD §12
/// already draws it on the trail, and a redirect signal that fires on it is
/// noise.
pub const DRIFT_COHERENCE: f32 = 0.6;

/// Effective observations the recent window must hold before its centre means
/// anything.
///
/// One `Glob` at weight 5 in `docs/` would otherwise drag the recent centre
/// across the map on its own for a full minute. Kish's effective sample size, so
/// that one strong observation counts as one — the same argument
/// [`Territory::bandwidth`] makes.
pub const DRIFT_MIN_RECENT: f32 = 2.0;

/// Weight below which a kernel stops contributing and is dropped.
const MIN_KERNEL_WEIGHT: f32 = 0.01;

/// The inferred scope of a main agent (PRD §3, §6).
#[derive(Debug, Clone, Default)]
pub struct Territory {
    /// Live kernels. Empty until convergence.
    pub kernels: Vec<Kernel>,
    /// The claimed ancestor directory, once [`Territory::has_converged`].
    pub claim: Option<LogicalPath>,
    /// Weighted centre of the whole live field. The drift vector is measured
    /// against this.
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
    /// Trace of the recent-work offset from the centre of mass, subsampled at
    /// [`DRIFT_SAMPLE_INTERVAL`] and windowed to [`DRIFT_WINDOW`]. Feeds
    /// [`Drift::coherence`] and nothing else.
    drift_trace: VecDeque<(Instant, Point)>,
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
    ///
    /// An observation whose claim path is the **repository root** is counted and
    /// then dropped, for the reason in [`Territory::observe`]'s body.
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
        // The repository root is the absorbing element of PRD §6.2's ancestor:
        // `common_ancestor(root, anything)` is the root, so **one** live
        // root-scoped observation pins `depth(A)` at 0 and the depth gate can
        // never pass again while it lives. A shell call's only path evidence is
        // its `cwd`, and a `cwd` at the checkout root resolves to the root, so
        // this is the common case rather than the corner one: measured over the
        // three recorded sessions, 119, 108 and 6 of 128 live evidence entries
        // were the root, and in the two where it dominated the territory never
        // converged at all — no claim, and therefore no cloud.
        //
        // It is also PRD §6's opening failure in its other form. The root
        // district's centre is the middle of the map, so every one of those
        // observations stacked a kernel on one point in the empty middle: 93 %
        // of the field's mass in one session sat on a single pixel. `place`'s
        // rung 2 already excludes a root `cwd` for exactly this reason — *the
        // whole city is not a location* — and scope is the same argument about
        // the same evidence.
        //
        // Nothing else is taken away: the operation still draws its own mark,
        // still steps the trail, and still counts in `observations`. It just
        // says nothing about *where*.
        if claim_path.is_root() {
            return;
        }
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
                at: obs.at,
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
            self.drift_trace.clear();
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

    /// The raw drift measurement, **unthresholded** (PRD §10.4).
    ///
    /// This is the number, not the decision. [`Territory::drift_mark`] applies
    /// the threshold; this exists so the threshold can be *measured* against real
    /// sessions rather than asserted, and so the drill-down layer can say how
    /// close a still territory is to moving.
    ///
    /// # Why this is recent-against-established and not then-against-now
    ///
    /// The obvious reading of PRD §10.4 — *"a drift vector from the centre of
    /// mass over the last 60s"* — is the displacement of the centre of mass
    /// across sixty seconds, and that was the first implementation. Measured
    /// against real sessions it **lags**: the centre of mass is a decayed
    /// weighted mean over the whole field, so it cannot move until PRD §6.3's
    /// 90 s half-life has eaten the evidence that was holding it, and the mark
    /// arrived 30–90 s *after* the territory's own claim had already jumped.
    /// (Named runs: `f47bd4e7`, claim `src/config → src/components/Minimap` at
    /// 9 490 s, first drift episode 9 575 s; `dfa8cd66`, claim move at 26 173 s,
    /// first episode 26 231 s.) A signal that confirms a change after it has
    /// been made is not a redirect signal, and PRD §10.4 asks for one that is
    /// visible *"while it is happening"*.
    ///
    /// So the vector runs from the field's own centre of mass to the centre of
    /// the **last [`DRIFT_WINDOW`] of work**. That fires on the first minute of
    /// work in a new place, before decay has moved anything, and it clears
    /// itself: once the field catches up, the two centres coincide again and the
    /// mark goes out. It is the same two ingredients the PRD names — the centre
    /// of mass, and a sixty-second window — read as a comparison rather than as
    /// a difference in time.
    ///
    /// `None` when the territory has no field, or when the recent window holds
    /// less than [`DRIFT_MIN_RECENT`] effective observations.
    pub fn drift_state(&self) -> Option<Drift> {
        let now = self.last_decay?;
        let (to, recent_n) = weighted_centre(&self.kernels, |k| {
            now.saturating_duration_since(k.at) <= DRIFT_WINDOW
        })?;
        // The baseline is the field **outside** the window, not the whole field:
        // including the recent kernels in their own baseline dilutes the offset
        // by exactly the thing being measured, and for the first minute of a
        // thread's life it cancels it out completely. No older kernels means no
        // established scope to be drifting away from, which is the correct
        // answer and not a missing measurement.
        let (from, _) = weighted_centre(&self.kernels, |k| {
            now.saturating_duration_since(k.at) > DRIFT_WINDOW
        })?;
        let vector = Point::new(to.x - from.x, to.y - from.y);
        let magnitude = vector.x.hypot(vector.y);
        let bandwidth = self.bandwidth();
        // Circular coherence over the trace: one if the offset has pointed the
        // same way at every sample, near zero if it has been swinging.
        let (mut ux, mut uy, mut n) = (0.0f32, 0.0f32, 0u32);
        for (_, offset) in &self.drift_trace {
            let m = offset.x.hypot(offset.y);
            if m > 0.0 {
                ux += offset.x / m;
                uy += offset.y / m;
                n += 1;
            }
        }
        #[allow(clippy::cast_precision_loss)] // n <= DRIFT_WINDOW / SAMPLE_INTERVAL + 1
        let coherence = if n == 0 { 0.0 } else { ux.hypot(uy) / n as f32 };
        let span = self
            .drift_trace
            .front()
            .zip(self.drift_trace.back())
            .map_or(Duration::ZERO, |((a, _), (b, _))| {
                b.saturating_duration_since(*a)
            });
        Some(Drift {
            from,
            to,
            vector,
            magnitude,
            bandwidth,
            ratio: if bandwidth > 0.0 {
                magnitude / bandwidth
            } else {
                0.0
            },
            recent_n,
            coherence,
            span,
            samples: self.drift_trace.len(),
        })
    }

    /// The drift vector over the last 60 s (PRD §10.4).
    ///
    /// > A territory whose centre of mass is migrating out of `src/auth` toward
    /// > `tests/` has a scope that is changing, and it is visible *while it is
    /// > happening* — well before contention fires. […] This is the redirect
    /// > signal, and it is the one thing here that no existing tool provides.
    ///
    /// `None` below the threshold, so a still territory draws no leading-edge
    /// mark at all. The returned [`Point`] is an offset in city units, not a
    /// position.
    pub fn drift(&self) -> Option<Point> {
        self.drift_mark().map(|m| m.vector)
    }

    /// The leading-edge mark, or `None` when the territory is not migrating
    /// (PRD §10.4).
    ///
    /// Three gates:
    ///
    /// * *"when its magnitude exceeds a threshold"* — [`DRIFT_THRESHOLD`]
    ///   multiples of the current bandwidth, so the test is in units of the
    ///   territory's own uncertainty rather than in city units. A diffuse
    ///   three-observation field has to move a long way to have moved at all; a
    ///   tight, well-evidenced one has not moved far before it means something.
    /// * *"migrating out of `src/auth` toward `tests/`"* — a **direction**, so
    ///   the offset has to have been pointing one way: [`DRIFT_COHERENCE`].
    ///   Thrash is not a scope change.
    /// * Enough recent evidence to mean anything: [`DRIFT_MIN_RECENT`].
    ///
    /// The mark runs from the field's centre of mass to the centre of the last
    /// minute's work, and it is drawn at the **far** end. The centre of mass is
    /// where the thread has been; the recent centre is where it is going. That
    /// is the whole point of the signal, and it is why no extrapolation is
    /// needed to place the mark — the leading edge is a measured position, not a
    /// projected one.
    pub fn drift_mark(&self) -> Option<DriftMark> {
        let drift = self.drift_state()?;
        if drift.recent_n < DRIFT_MIN_RECENT {
            return None;
        }
        if drift.magnitude <= DRIFT_THRESHOLD * drift.bandwidth {
            return None;
        }
        if drift.coherence < DRIFT_COHERENCE {
            return None;
        }
        Some(DriftMark {
            tail: drift.from,
            tip: drift.to,
            vector: drift.vector,
            ratio: drift.ratio,
            coherence: drift.coherence,
        })
    }

    /// Total live kernel weight — the field's mass, and the ranking key for
    /// PRD §10.4's cloud cap.
    pub fn mass(&self) -> f32 {
        self.kernels.iter().map(|k| k.weight).sum()
    }

    /// How many samples the drift trace holds.
    ///
    /// Bounded by [`DRIFT_WINDOW`] over [`DRIFT_SAMPLE_INTERVAL`] whatever the
    /// event rate, which is the property PRD §13.1's 500 events/sec budget needs
    /// and the one a test can assert without arranging a drift.
    pub fn drift_samples(&self) -> usize {
        self.drift_trace.len()
    }

    /// How long since the newest live observation (PRD §10.4's dormancy).
    ///
    /// No `now` argument on purpose. [`Territory::decay`] runs from
    /// `World::tick` every tick, so `last_decay` **is** now, and a policy that
    /// took its own clock could disagree with the field it is judging. `None`
    /// before the first observation.
    pub fn quiet_for(&self) -> Option<Duration> {
        let now = self.last_decay?;
        let newest = self.evidence.iter().map(|e| e.at).max()?;
        Some(now.saturating_duration_since(newest))
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

    /// Recomputes the weighted centre of mass and rolls the drift trace.
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

        // The trace holds the **offset** of the last minute's work from the
        // field's centre, not the centre itself, because the only question it
        // answers is whether that offset has been pointing one way
        // ([`Drift::coherence`]). Subsampled: coherence needs tens of samples,
        // not the thousands a burst would push.
        let offset = match (
            weighted_centre(&self.kernels, |k| {
                now.saturating_duration_since(k.at) <= DRIFT_WINDOW
            }),
            weighted_centre(&self.kernels, |k| {
                now.saturating_duration_since(k.at) > DRIFT_WINDOW
            }),
        ) {
            (Some((recent, _)), Some((established, _))) => {
                Point::new(recent.x - established.x, recent.y - established.y)
            }
            _ => Point::new(0.0, 0.0),
        };
        match self.drift_trace.back_mut() {
            Some((t, p)) if now.saturating_duration_since(*t) < DRIFT_SAMPLE_INTERVAL => {
                *p = offset;
            }
            _ => self.drift_trace.push_back((now, offset)),
        }
        while self
            .drift_trace
            .front()
            .is_some_and(|(t, _)| now.saturating_duration_since(*t) > DRIFT_WINDOW)
        {
            // Keep exactly one sample at or beyond the window edge so the drift
            // vector always spans the full 60 s once the thread is that old.
            if self
                .drift_trace
                .get(1)
                .is_some_and(|(t, _)| now.saturating_duration_since(*t) > DRIFT_WINDOW)
            {
                self.drift_trace.pop_front();
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

/// The raw drift measurement over [`DRIFT_WINDOW`] (PRD §10.4).
///
/// Unthresholded on purpose: this is what a threshold gets chosen *from*.
#[derive(Debug, Clone, Copy)]
pub struct Drift {
    /// Weighted centre of the kernels **older** than [`DRIFT_WINDOW`] — the
    /// established scope.
    pub from: Point,
    /// Weighted centre of the kernels inside [`DRIFT_WINDOW`] — the last
    /// minute's work.
    pub to: Point,
    /// `to - from`, in city units.
    pub vector: Point,
    /// `|vector|`.
    pub magnitude: f32,
    /// The bandwidth the magnitude is judged against.
    pub bandwidth: f32,
    /// `magnitude / bandwidth` — the scale-free number, and the one the
    /// threshold is stated in. A ratio of 1 means the last minute's work sits a
    /// full field radius off the field's centre, which is roughly "the new work
    /// is where the fringe was".
    pub ratio: f32,
    /// Kish's effective observation count inside the recent window. Below
    /// [`DRIFT_MIN_RECENT`] the recent centre is one stray read and means
    /// nothing.
    pub recent_n: f32,
    /// Length of the mean unit offset over the trace, in `[0, 1]`. One is an
    /// offset that has pointed the same way throughout; near zero is a thread
    /// ping-ponging around its own centre.
    pub coherence: f32,
    /// How much of the window the samples span. Below [`DRIFT_WINDOW`] for a
    /// thread younger than a minute.
    pub span: Duration,
    /// Samples in the trace.
    pub samples: usize,
}

/// The leading-edge mark (PRD §10.4) — the redirect signal.
///
/// > Compute a drift vector from the centre of mass over the last 60s and render
/// > a leading-edge mark when its magnitude exceeds a threshold. **This is the
/// > redirect signal, and it is the one thing here that no existing tool
/// > provides.**
///
/// Positions are in city units. The renderer projects them; it decides nothing.
#[derive(Debug, Clone, Copy)]
pub struct DriftMark {
    /// The established scope — where the thread's older work is.
    pub tail: Point,
    /// The centre of the last [`DRIFT_WINDOW`] of work. **This is where the
    /// mark goes**: the centre of mass is where the thread has been.
    pub tip: Point,
    /// `tip - tail`.
    pub vector: Point,
    /// [`Drift::ratio`], for weighting the mark. At the threshold it is
    /// [`DRIFT_THRESHOLD`]; twice that is a thread that has left.
    pub ratio: f32,
    /// [`Drift::coherence`], for the drill-down layer.
    pub coherence: f32,
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
    /// When the observation that dropped it was made.
    ///
    /// Carried on the kernel rather than derived, because PRD §10.4's drift
    /// compares the centre of the **last minute's** kernels against the centre
    /// of all of them, and there is no other way to know which is which: the
    /// evidence list is not one-to-one with the kernels (an observation with no
    /// geometry is evidence about scope and drops no kernel).
    pub at: Instant,
}

/// The weighted centre of the kernels a predicate selects, and Kish's effective
/// sample size over them.
///
/// `None` when nothing is selected, which is how "there is no established
/// scope yet" and "nothing has happened lately" both reach [`Territory::drift`]
/// as an absent signal rather than as a zero.
fn weighted_centre(kernels: &[Kernel], keep: impl Fn(&Kernel) -> bool) -> Option<(Point, f32)> {
    let (mut sx, mut sy, mut sw, mut sq) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    for k in kernels.iter().filter(|k| keep(k)) {
        sx += k.centre.x * k.weight;
        sy += k.centre.y * k.weight;
        sw += k.weight;
        sq += k.weight * k.weight;
    }
    if sw <= 0.0 || sq <= 0.0 {
        return None;
    }
    Some((Point::new(sx / sw, sy / sw), sw * sw / sq))
}

/// The multiplier a weight loses over `dt` at a 90 s half-life.
pub fn decay_factor(dt: Duration) -> f32 {
    #[allow(clippy::cast_precision_loss)] // seconds of a live session; far inside f32
    let half_lives = dt.as_secs_f32() / DECAY_HALF_LIFE.as_secs_f32();
    0.5_f32.powf(half_lives)
}

// ---------------------------------------------------------------------------
// The cloud cap (PRD §10.4, §17 open question 1)
// ---------------------------------------------------------------------------

/// The most clouds drawn at once (PRD §10.4).
///
/// > **Cap the number of visible clouds.** Forty threads means forty systems and
/// > the map vanishes under haze.
///
/// PRD §17's first open question asks what the right number is. This is the
/// measured answer, from `polis-render/tests/cloud_cap_policy.rs`: forty of the
/// operator's real sessions replayed **into one world**, each aligned so its
/// busiest minute lands on the same instant, then the same moment rendered at
/// every cap and the pixels counted. 38 threads, 11 with a converged field.
///
/// | cap | clouds | cloud ink | contested ink | mean crowd |
/// |---|---|---|---|---|
/// | 1 | 1 | 0.82 % | 0.0 % | 0.99 |
/// | 2 | 2 | 0.94 % | 29.4 % | 1.25 |
/// | 3 | 3 | 1.96 % | 24.6 % | 1.21 |
/// | 4 | 4 | 1.99 % | 30.3 % | 1.40 |
/// | **5** | 5 | 2.18 % | **37.7 %** | **1.50** |
/// | 6 | 6 | 2.37 % | **58.3 %** | 1.67 |
/// | 8 | 8 | 2.57 % | 57.7 % | 1.86 |
/// | 10 | 10 | 3.19 % | 50.3 % | 1.99 |
///
/// **Fog is not what breaks first, and that is the surprise.** PRD §10.4's fear
/// — *"forty threads means forty systems and the map vanishes under haze"* —
/// does not happen: ten simultaneous territories ink 3.2 % of the frame, because
/// PRD §6.4's bandwidth is `base / sqrt(n)` clamped and a well-evidenced
/// territory is therefore *small*, and because the bands are drawn as contour
/// and hatch rather than as fills. Uncapped, the map is legible.
///
/// What breaks is **whose**. `mean crowd` is how many territories reach fringe
/// level at the average inked cell, and it crosses 1.5 at five clouds: past
/// that, the average piece of cloud belongs to two threads and answers *"work is
/// happening here"* while refusing to answer *"whose work"* — PRD §17's
/// aggregate illusion exactly. The cliff is between five and six, where
/// contested ink jumps 37.7 % → 58.3 % for one extra cloud while every other
/// step adds at most thirteen points.
///
/// Five is therefore the largest cap at which contested ground is still a
/// **minority** of the cloud ink, which is what PRD §11.3 needs it to be: it
/// makes territory overlap *"a distinct, quieter signal"* rather than the
/// background.
///
/// One fleet, one moment, one repository. Re-run the harness before trusting it
/// on a different shape of work.
pub const CLOUD_CAP: usize = 5;

/// How long a converged territory may sit untouched before it dissipates
/// (PRD §10.4).
///
/// > let dormant territories dissipate entirely
///
/// Two [`DECAY_HALF_LIFE`]s, which takes a territory's evidence to a quarter —
/// the point at which its bands have collapsed to a fringe and it is
/// contributing haze rather than location.
///
/// Swept on the same forty-session fleet as [`CLOUD_CAP`], with the cap lifted
/// so the window is the only thing acting:
///
/// | window | clouds shown | dissipated | cloud ink |
/// |---|---|---|---|
/// | 30 s | 8 | 3 | 3.14 % |
/// | 90 s | 8 | 3 | 3.14 % |
/// | **180 s** | 10 | 1 | 3.19 % |
/// | 600 s | 11 | 0 | 3.25 % |
/// | none | 11 | 0 | 3.25 % |
///
/// Two things fall out. The window is a **small** effect — it retires one to
/// three territories of eleven and about 3 % of the cloud ink — because PRD
/// §6.3's decay is already doing the work: at 600 s the gate has nothing left to
/// catch, since a kernel's weight is under the drop floor after about ten
/// minutes and the territory has dissipated on its own. And 30 s buys nothing
/// over 180 s but would strip the cloud off any thread that pauses half a minute
/// to think, which is a map that flickers.
///
/// PRD §17's open question — *dissipate entirely or leave a faint residue?* — is
/// therefore answered twice over. **Dissipate**, for the reason in
/// [`CloudSelection::dormant`]; and note that dissipating entirely is what the
/// system already does, so a residue would have to be *added* by pinning a floor
/// under the decay. Nothing in the measurement argues for paying that.
pub const DORMANT_AFTER: Duration = Duration::from_secs(180);

/// PRD §10.4's cloud policy, in one place.
///
/// A struct rather than two loose arguments because the cap and the dormancy
/// window trade against each other — a short dormancy window makes a large cap
/// safe, and a large cap makes a short window necessary — and a caller that can
/// set one without seeing the other will get that trade wrong.
#[derive(Debug, Clone, Copy)]
pub struct CloudPolicy {
    /// The most clouds drawn at once.
    pub cap: usize,
    /// How long a territory may sit untouched before it draws nothing.
    pub dormant_after: Duration,
}

impl Default for CloudPolicy {
    fn default() -> Self {
        Self {
            cap: CLOUD_CAP,
            dormant_after: DORMANT_AFTER,
        }
    }
}

impl CloudPolicy {
    /// The same policy with a different cap, for the measurement harness and for
    /// the operator's `cloud_cap` setting.
    #[must_use]
    pub fn with_cap(mut self, cap: usize) -> Self {
        self.cap = cap;
        self
    }
}

/// What the cap did to this frame's threads.
///
/// The counts are not diagnostics: a cap that silently hides a territory is a
/// map that lies, so the status rail has to be able to say *"6 clouds, 3 quiet,
/// 12 more not shown"*. PRD §6.2 already requires the same of an unplaced
/// thread.
#[derive(Debug, Clone)]
pub struct CloudSelection<'a> {
    /// The territories to splat, in rank order.
    pub visible: Vec<&'a Territory>,
    /// Threads whose evidence has not converged yet, so they have no claim and
    /// no cloud — *"an unplaced marker in the status rail"* (PRD §6.2).
    pub unplaced: usize,
    /// Converged territories that have been quiet longer than
    /// [`CloudPolicy::dormant_after`] and were dropped **entirely**.
    ///
    /// PRD §17's open question 1 asks whether these should leave a faint
    /// residue. They should not, and the reason is PRD §17's own test — *does it
    /// change a decision?* A residue answers "somebody worked here earlier",
    /// which is what `git log`, the trail layer and the operation marks already
    /// answer, and it answers it *permanently*: on this machine one repository
    /// holds 151 recorded sessions, so a residue that never fully clears turns
    /// the map into a heat map of the year rather than a picture of now. The
    /// cost is not hypothetical either — a cloud is the only layer drawn
    /// underneath the district labels (PRD §10.3), so residue is paid for in
    /// exactly the contrast the live layers need.
    pub dormant: usize,
    /// Territories that would have been drawn but lost to the cap. This is the
    /// number that says whether the cap is set right.
    pub capped: usize,
}

/// Chooses which territories get a cloud this frame (PRD §10.4).
///
/// > Render clouds only for threads with an active attention state or in the top
/// > N by recent activity; let dormant territories dissipate entirely.
///
/// Three gates, in the order the PRD states them:
///
/// 1. **Converged, or nothing.** A territory with no claim has not agreed with
///    itself yet and renders as a rail entry, never as a cloud (PRD §6.2).
/// 2. **Dormant territories dissipate entirely.** Quiet for longer than
///    [`CloudPolicy::dormant_after`] and it is gone, not faded — see
///    [`CloudSelection::dormant`].
/// 3. **Rank, then cap.** An active attention state first, in PRD §11.1's order
///    (`contention > needs-decision`), then most recent activity. `done` is
///    deliberately *not* a promoting state: PRD §11.1 says done costs nothing,
///    and a fleet where thirty threads have finished would otherwise spend the
///    whole cap on clouds over finished work.
///
/// `attention` is the world's current attention list. Pass an empty slice to
/// rank on thread status alone.
pub fn select_clouds<'a>(
    territories: &'a [(&'a crate::Thread, &'a Territory)],
    attention: &[crate::attention::Attention],
    policy: CloudPolicy,
) -> CloudSelection<'a> {
    let mut unplaced = 0;
    let mut dormant = 0;
    let mut ranked: Vec<(u8, &(&crate::Thread, &Territory))> = Vec::new();
    for pair in territories {
        let (thread, territory) = pair;
        if territory.claim.is_none() || territory.kernels.is_empty() {
            unplaced += 1;
            continue;
        }
        if territory
            .quiet_for()
            .is_some_and(|q| q > policy.dormant_after)
        {
            dormant += 1;
            continue;
        }
        ranked.push((cloud_rank(thread, attention), pair));
    }
    ranked.sort_by(|(ra, (ta, _)), (rb, (tb, _))| {
        ra.cmp(rb)
            .then(tb.last_activity.cmp(&ta.last_activity))
            .then(ta.id.cmp(&tb.id))
    });
    let capped = ranked.len().saturating_sub(policy.cap);
    let visible = ranked
        .into_iter()
        .take(policy.cap)
        .map(|(_, (_, t))| *t)
        .collect();
    CloudSelection {
        visible,
        unplaced,
        dormant,
        capped,
    }
}

/// Rank tier for the cap: lower is drawn first.
///
/// PRD §10.4 says *"threads with an active attention state **or** in the top N
/// by recent activity"*, and PRD §11.1 orders the states
/// `contention > needs-decision > done`. Contention and needs-decision promote;
/// `done` does not, because §11.1's own reason for ranking it last — *"Done
/// costs nothing"* — is exactly the reason it should not spend a cap slot.
fn cloud_rank(thread: &crate::Thread, attention: &[crate::attention::Attention]) -> u8 {
    use crate::attention::AttentionKind;
    if thread.status != crate::ThreadStatus::Done {
        for mark in attention {
            if mark.thread() != &thread.id {
                continue;
            }
            match &mark.kind {
                AttentionKind::Contention(_) => return 0,
                AttentionKind::NeedsDecision { .. } => return 1,
                AttentionKind::Done { .. } => {}
            }
        }
    }
    // No promoting mark: fall back to the status rail's own order, offset so it
    // can never outrank one.
    2 + thread.status.rail_rank()
}

/// PRD §10.4's cap with the default policy and no attention list.
///
/// Kept because most callers have neither an attention list nor an opinion about
/// dormancy; the policy still applies, so the two paths cannot disagree.
pub fn visible_clouds<'a>(
    territories: &'a [(&'a crate::Thread, &'a Territory)],
    cap: usize,
) -> Vec<&'a Territory> {
    select_clouds(territories, &[], CloudPolicy::default().with_cap(cap)).visible
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

    /// The regression that emptied the sky: a shell call at the checkout root
    /// is scoped to the root, `common_ancestor` with the root is the root, and
    /// one of them was enough to hold `depth(A)` at 0 forever. Real sessions run
    /// hundreds, so no territory converged and no replay drew a cloud.
    #[test]
    fn shell_calls_at_the_repository_root_do_not_erase_the_claim() {
        let mut t = Territory::for_extent(1000.0);
        let now = Instant::now();
        for i in 0..9 {
            t.observe(
                &obs(&format!("src/auth/f{i}.rs"), ToolKind::Edit, now),
                1.0,
                place(i),
            );
        }
        assert_eq!(t.claim.as_ref().map(LogicalPath::as_str), Some("src/auth"));

        // `cargo test` from the checkout root, over and over, which is what an
        // agent actually spends its calls on. Enough of them to outnumber and
        // outweigh the edits.
        let root = Observation {
            thread: ThreadId::of_session(SessionId::new("s")),
            worker: None,
            path: LogicalPath::root(),
            scope: PathScope::Directory,
            tool: ToolKind::Bash,
            at: now,
            weight: None,
        };
        for _ in 0..40 {
            // `place(0)` is the root district's centre — the middle of the map,
            // where the old code stacked every one of these.
            t.observe(&root, 1.0, place(0));
        }

        let c = t.convergence();
        assert_eq!(
            c.claim.as_ref().map(LogicalPath::as_str),
            Some("src/auth"),
            "the root says nothing about scope and must not annihilate one"
        );
        assert_eq!(c.observations, 9, "and it leaves no evidence behind");
        assert_eq!(
            t.kernels.len(),
            9,
            "nor a stack of kernels on one pixel in the empty middle"
        );
        assert_eq!(
            t.observations, 49,
            "it is still counted: the thread did do that work"
        );
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
        // Past the window, so the first six are the *established* scope the
        // next minute's work is judged against.
        let later = t0 + DRIFT_WINDOW + Duration::from_secs(5);
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
    fn thrash_is_not_drift() {
        // PRD §10.4 asks for a territory "migrating out of `src/auth` toward
        // `tests/`" — a direction. A thread bouncing between two directories
        // covers ground and ends where it started; PRD §12 already draws that
        // as thrashing on the trail, and a redirect signal that fires on it is
        // the noise PRD §17 warns about.
        let mut t = Territory::for_extent(100.0);
        let mut at = Instant::now();
        for i in 0..60 {
            let here = if i % 2 == 0 { 0.0 } else { 60.0 };
            t.observe(
                &obs(&format!("src/auth/f{i}.rs"), ToolKind::Edit, at),
                1.0,
                Some(Point::new(here, 0.0)),
            );
            at += Duration::from_secs(2);
        }
        let d = t.drift_state().expect("there is a field");
        assert!(
            d.coherence < DRIFT_COHERENCE,
            "the offset swung rather than pointed: coherence {}",
            d.coherence
        );
        assert!(
            t.drift_mark().is_none(),
            "ping-ponging is thrash, not a scope change"
        );
    }

    #[test]
    fn the_mark_is_at_the_leading_edge_not_the_centre_of_mass() {
        let mut t = Territory::for_extent(100.0);
        let t0 = Instant::now();
        for i in 0..6 {
            t.observe(
                &obs(&format!("src/auth/f{i}.rs"), ToolKind::Edit, t0),
                1.0,
                place(0),
            );
        }
        let mut at = t0 + DRIFT_WINDOW + Duration::from_secs(1);
        for i in 0..40 {
            at += Duration::from_secs(1);
            #[allow(clippy::cast_precision_loss)] // small test indices
            let x = 10.0 * i as f32;
            t.observe(
                &obs(&format!("tests/unit/g{i}.rs"), ToolKind::Edit, at),
                1.0,
                Some(Point::new(x, 0.0)),
            );
        }
        let mark = t.drift_mark().expect("the scope moved");
        assert!(mark.vector.x > 0.0, "and it moved to the right");
        assert!(
            mark.tip.x > mark.tail.x,
            "the mark leads the mass: tip {} vs mass {}",
            mark.tip.x,
            mark.tail.x
        );
        assert!(
            mark.tail.x.abs() < 1e-3,
            "the tail is the established scope, which never left the origin: {}",
            mark.tail.x
        );
    }

    /// The regression the whole redefinition exists for. Measured on real
    /// sessions, a then-against-now drift vector arrived 30–90 s *after* the
    /// territory's claim had already moved, because the centre of mass cannot
    /// move until PRD §6.3's 90 s half-life has eaten the evidence holding it.
    /// Recent-against-established fires on the first minute of work elsewhere.
    #[test]
    fn drift_fires_before_decay_has_moved_the_centre_of_mass() {
        let mut t = Territory::for_extent(100.0);
        let t0 = Instant::now();
        // A well-established territory: twenty edits in one place.
        for i in 0..20 {
            t.observe(
                &obs(&format!("src/auth/f{i}.rs"), ToolKind::Edit, t0),
                1.0,
                Some(Point::new(0.0, 0.0)),
            );
        }
        let before = t.centre_of_mass.expect("a field");
        assert!(
            t.drift_mark().is_none(),
            "a settled territory drifts nowhere"
        );

        // Four edits somewhere else, over twenty seconds — far less mass than
        // the twenty already there, and far too soon for decay to matter.
        let mut at = t0 + DRIFT_WINDOW;
        for i in 0..4 {
            at += Duration::from_secs(5);
            t.observe(
                &obs(&format!("tests/unit/g{i}.rs"), ToolKind::Edit, at),
                1.0,
                Some(Point::new(60.0, 0.0)),
            );
        }
        let after = t.centre_of_mass.expect("a field");
        let moved = (after.x - before.x).abs();
        assert!(
            moved < 60.0 * 0.35,
            "the centre of mass has barely moved yet: {moved}"
        );
        let mark = t
            .drift_mark()
            .expect("but the last twenty seconds of work are plainly elsewhere");
        assert!(mark.tip.x > mark.tail.x + 20.0, "and the mark points at it");
    }

    #[test]
    fn the_drift_trace_stays_bounded_under_a_burst() {
        // PRD §13.1 budgets 500 events/sec sustained. A per-observation trace
        // would put 30 000 samples in a 60 s window to answer a question that
        // needs sixty.
        let mut t = Territory::for_extent(1000.0);
        let mut at = Instant::now();
        for i in 0..5_000 {
            t.observe(
                &obs(&format!("src/auth/f{i}.rs"), ToolKind::Edit, at),
                1.0,
                place(i % 32),
            );
            at += Duration::from_millis(2);
        }
        assert!(
            u64::try_from(t.drift_samples()).unwrap_or(u64::MAX) <= 1 + DRIFT_WINDOW.as_secs(),
            "{} samples for a 60 s window",
            t.drift_samples()
        );
        // The kernel list is capped at `OBSERVATION_WINDOW` too, so a burst
        // this fast leaves nothing older than the window and there is no
        // established scope to drift away from. That is the honest answer.
        assert!(t.drift_state().is_none());
    }

    #[test]
    fn a_dormant_territory_draws_no_cloud() {
        use crate::{Thread, ThreadStatus};
        let t0 = Instant::now();
        let mut thread = Thread::new(
            ThreadId::of_session(SessionId::new("quiet")),
            SessionId::new("quiet"),
            t0,
        );
        thread.status = ThreadStatus::Idle;
        thread.territory = Territory::for_extent(1000.0);
        for i in 0..8 {
            thread.territory.observe(
                &obs(&format!("src/auth/f{i}.rs"), ToolKind::Edit, t0),
                1.0,
                place(i),
            );
        }
        let pairs = [(&thread, &thread.territory)];
        let sel = select_clouds(&pairs, &[], CloudPolicy::default());
        assert_eq!(sel.visible.len(), 1, "fresh work has a cloud");
        assert_eq!(sel.dormant, 0);

        // Quiet past the window. The evidence is still there — PRD §6.3 removes
        // nothing abruptly — but the map stops spending ink on it.
        let mut quiet = thread.clone();
        quiet
            .territory
            .decay(t0 + DORMANT_AFTER + Duration::from_secs(1));
        assert!(
            quiet.territory.claim.is_some(),
            "the claim survives; only the cloud goes"
        );
        let pairs = [(&quiet, &quiet.territory)];
        let sel = select_clouds(&pairs, &[], CloudPolicy::default());
        assert!(sel.visible.is_empty(), "dormant territories dissipate");
        assert_eq!(sel.dormant, 1, "and the rail can say so");
    }

    #[test]
    fn an_attention_state_outranks_recent_activity_but_done_does_not() {
        use crate::attention::{Attention, AttentionKind, DecisionSource};
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
        let blocked = mk("blocked", ThreadStatus::Working, now);
        let finished = mk("finished", ThreadStatus::Done, now + Duration::from_secs(9));

        let marks = vec![
            Attention::new(
                AttentionKind::NeedsDecision {
                    thread: blocked.id.clone(),
                    at: None,
                    source: DecisionSource::PermissionRequest,
                },
                now,
            ),
            Attention::new(
                AttentionKind::Done {
                    thread: finished.id.clone(),
                    verified: false,
                },
                now,
            ),
        ];

        let pairs = [
            (&busy, &busy.territory),
            (&blocked, &blocked.territory),
            (&finished, &finished.territory),
        ];
        let sel = select_clouds(&pairs, &marks, CloudPolicy::default().with_cap(1));
        assert_eq!(
            sel.visible[0].claim.as_ref().map(LogicalPath::as_str),
            Some("src/blocked"),
            "a thread blocked on a human outranks a busier one"
        );
        assert_eq!(sel.capped, 2);

        // PRD §11.1: "Done costs nothing". A finished thread must not spend a
        // cap slot ahead of a live one, however recently it finished.
        let sel = select_clouds(&pairs, &marks, CloudPolicy::default().with_cap(2));
        let shown: Vec<&str> = sel
            .visible
            .iter()
            .filter_map(|t| t.claim.as_ref().map(LogicalPath::as_str))
            .collect();
        assert_eq!(shown, ["src/blocked", "src/busy"]);
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
