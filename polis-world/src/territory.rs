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
//! The paragraph above is the whole specification for **where a thread's mark
//! goes**, and for a long time this module contradicted it: every drawn mark
//! read [`Territory::centre_of_mass`], which is a weighted arithmetic mean, and
//! a mean of `src/auth` and `tests/` is the empty gap. [`Territory::anchor`] is
//! the field's *mode* — the kernel centre where the density is highest — and it
//! is what honours the sentence now. The mean stays, under its own name, for the
//! two consumers that want a first moment.
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
//!
//! How rarely the mass gate binds is worth knowing before anyone tunes it: the
//! investigation behind [`RESTING_WEIGHT`]'s current unit found
//! [`Convergence::mass_ratio`] reading **1.000 for every converged thread in
//! every session it sampled**. The mechanism is [`Territory::observe`]'s root
//! drop — the only class of evidence that routinely falls outside the trimmed
//! ancestor is a shell call scoped to the repository root, and those never reach
//! the evidence list at all. What is left outside is a genuinely stray read, and
//! a stray read is light, so the trim usually takes it before the ratio sees it.
//! That number was **not re-taken for this change** and the harness that took it
//! is not in the tree; it is recorded because a reader who tunes
//! [`CONVERGENCE_MASS`] expecting it to bite will be tuning a gate that cannot
//! fire.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use polis_events::LogicalPath;
use polis_layout::Point;

use crate::Observation;

/// Kernel weight half-life (PRD §6.3).
///
/// > **Contract slowly**: kernel weights decay with a half-life of 90s. Nothing
/// > is removed abruptly.
pub const DECAY_HALF_LIFE: Duration = Duration::from_secs(90);

/// The **field peak** a converged territory never decays below.
///
/// `Territory::rest` explains why a floor exists at all. This doc is about the
/// *unit*, because the unit was wrong and that is what made the constant miss.
///
/// `polis_render::live::CLOUD_ISO` thresholds the **density field**, in the units
/// ADR-0020 fixes: one full-weight kernel reads `1.0` at its own centre
/// ([`polis_layout::quartic`]), and the outermost band is `0.55`. So the quantity
/// that has to clear the fringe is [`Territory::field_peak`]. For most of this
/// constant's life `rest` normalised the **mass** instead — `Σw`, a different
/// number and always the larger one — and this doc argued for the value in that
/// unit, which is a comparison of a sum of weights against a field value.
///
/// On a tight territory the two coincide: one kernel's peak *is* its weight. On
/// a spread one they do not, and PRD §6.4's multi-lobed territory is spread by
/// construction. Measured during the investigation behind this change, over 472
/// clouds that [`select_clouds`] actually selected: **33 of them — 7.0 % — were
/// computed, ranked, handed to the rasteriser and never painted**, at a mean mass
/// of 1.136 (comfortably over this floor) and a mean peak of 0.429 (under the
/// fringe). That harness is not in the tree and the numbers were not re-taken
/// here.
///
/// **0.9 as a peak is 1.64× the 0.55 fringe, and that headroom is the reason the
/// floor works rather than an accident.** Three things shrink the peak between
/// here and the pixel, and all three are on the window's side of the house:
/// `polis_app::clouds` floors each drawn radius at `.max(2.0)` where
/// `polis_render::frame` does not, it adds a chain of low-weight bridge kernels
/// between lobes, and `polis_render::live::CloudField::sample` reads a lattice at
/// cell centres and therefore under-reads a sharp peak by the sub-cell offset.
/// Set this at 0.55 exactly and any one of those losses puts the cloud back under
/// the fringe. Set it far above and a territory nobody has touched for seven
/// minutes is drawn as brightly as the thread working right now, which inverts
/// PRD §10.3's *"spend the rest on layers 4–5"*: a resting territory should be the
/// faintest cloud on the map, because "this is where that agent was working" is
/// worth less than "this is where one is working now".
pub const RESTING_WEIGHT: f32 = 0.9;

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

/// The share of a thread's weight a cluster must carry to count as a lobe
/// (PRD §6.4).
///
/// Low, deliberately. A lobe is not a claim: it says "this thread is also
/// working here", and the operator's own words for what that must look like
/// were *"even if the line gets thinner in the middle, it should be clear that
/// this is one entity working on both of these things"*. Set high enough and an
/// orchestrator's smaller lobes vanish and it reads as two unrelated agents;
/// set at zero and a single stray read raises a lobe, which is the failure
/// §6.2's trim exists to prevent. 8% is below the smallest real lobe measured
/// on this repository (`polis-events`, 62 of 3 386 calls ≈ 1.8% — deliberately
/// excluded) and above a stray touch.
pub const MIN_LOBE_MASS: f32 = 0.08;

/// How many lobes one territory may draw.
///
/// PRD §10.4 caps clouds because "forty threads means forty systems and the map
/// vanishes under haze"; the same argument applies within one thread. Past a
/// handful of lobes the thread is working everywhere, and the honest rendering
/// of "everywhere" is the heaviest few plus the connecting band, not a wash
/// over the whole city.
pub const MAX_LOBES: usize = 6;

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

/// How much better a rival kernel must be before the **drawn anchor** moves to
/// it (PRD §6.3).
///
/// > **Move only on sustained evidence**: the territory's centre of mass may
/// > only shift districts after `N=8` consecutive weighted observations fall
/// > outside the current claim. One read elsewhere moves nothing.
///
/// [`DRIFT_CONFIRMATIONS`] implements that sentence for the *claim*. Nothing
/// implemented it for the point the thread's ring is drawn at, and until now
/// nothing had to: that point was [`Territory::centre_of_mass`], an arithmetic
/// mean, and a mean has no ties to break — it slides and never jumps. A mode
/// does. Two lobes of nearly equal density swap the argmax on one observation
/// and the ring teleports across the city and back. Measured over the sessions
/// this change was investigated on, the mode's median frame-to-frame jump is
/// 0.26 bandwidths against the mean's 1.28 — much calmer — but its **p90 is
/// 14.68**, and that p90 is the near-tie teleport and nothing else.
///
/// So the held anchor keeps the ring until a different kernel's density beats
/// the held point's *current* density by this factor. At 1.0 the margin does
/// nothing and the p90 comes back. Set far above it and the ring sticks where
/// the thread started while the field has plainly moved on, which is PRD §6's
/// opening failure in its other form — a mark in the one place nothing is
/// happening.
///
/// The margin also buys a bound worth stating on its own, because it is what
/// stops hysteresis parking the ring somewhere cold: the drawn anchor's density
/// is never below `1 / ANCHOR_MARGIN` of [`Territory::field_peak`] — 80 % of the
/// peak at this value — and `the_anchor_holds_through_a_near_tie` asserts it.
///
/// **Asserted, not swept.** 1.25 is "clearly more than a tie, clearly less than
/// a move" read off the jump distribution above; no sweep of *this constant*
/// against a corpus was run, and the harness that took those numbers is not in
/// the tree, so they cannot currently be re-taken. [`CLOUD_CAP`]'s caveat
/// applies here with more force, because that one at least had a fleet behind
/// it.
pub const ANCHOR_MARGIN: f32 = 1.25;

/// Relaxation time constant for the point the map draws the ring at.
///
/// # Why a glide and not more hysteresis
///
/// [`ANCHOR_MARGIN`] stops the *target* thrashing between two lobes of nearly
/// equal density. It cannot stop the target moving when the field genuinely
/// changes, and when it does move it moves in one frame — the ring teleports.
/// Measured by replaying sessions `4bbcee1c` and `4bed007c` through this world
/// and sampling once per second of session time, the anchor was **stationary on
/// 134 of 148 and on 351 of 373 samples** and then stepped the whole way at
/// once. Both halves of that are wrong: a map that is frozen 90 % of the time
/// and teleports the rest is not showing a thread moving, it is showing a thread
/// blinking between two places.
///
/// The answer is not to damp the argmax harder — that is the same state machine
/// with a longer fuse. It is to stop drawing the argmax. The mode stays the
/// *target*, with all of [`ANCHOR_MARGIN`]'s reasoning intact, and the drawn
/// point relaxes toward it. A near-tie flip then costs a slow glide rather than
/// a jump, and a real relocation reads as the thread travelling.
///
/// # Eight seconds, because a session is not eight seconds
///
/// A session runs ten minutes and up, so the ring can afford to take the better
/// part of a minute to arrive: 63 % of the way at one time constant, 95 % at
/// three. On any one glance that is a few seconds of drift, which is the point.
///
/// # This is exponential on purpose, and that is a determinism property
///
/// `refresh_anchor`'s docs reject a wall-clock throttle because a state machine
/// over a *sequence of comparisons* settles differently under the window's 2 Hz
/// tick and the headless renderer's one-tick-per-schedule (PRD §7.4, ADR-0029).
/// An exponential relaxation has no such hazard: `exp(-a/τ)·exp(-b/τ)` is
/// exactly `exp(-(a+b)/τ)`, so relaxing once over `dt` and relaxing twice over
/// `dt/2` reach the same point to float precision. Composing under subdivision
/// is the whole reason this is a decay and not a linear step, and
/// `the_drawn_anchor_glides_the_same_way_however_finely_it_is_ticked` asserts
/// it.
pub const ANCHOR_GLIDE: Duration = Duration::from_secs(8);

/// Weight below which a kernel stops contributing and is dropped.
const MIN_KERNEL_WEIGHT: f32 = 0.01;

/// The inferred scope of a main agent (PRD §3, §6).
#[derive(Debug, Clone, Default)]
pub struct Territory {
    /// Live kernels. Empty until convergence.
    pub kernels: Vec<Kernel>,
    /// The claimed ancestor directory, once [`Territory::has_converged`].
    pub claim: Option<LogicalPath>,
    /// PRD §6.4's lobes, heaviest first — where this thread is working when a
    /// single ancestor cannot say it.
    ///
    /// A focused thread has one lobe and a [`Territory::claim`]; an
    /// orchestrator spread across the tree has several lobes and no claim,
    /// because their common ancestor is the root. Before this existed such a
    /// thread drew nothing at all, which is the one case §6.4 was written for.
    pub lobes: Vec<Lobe>,
    /// Weighted arithmetic mean of the live field — literally `Σ(c·w)/Σw`.
    ///
    /// **Not where this thread's mark is drawn.** That is [`Territory::anchor`],
    /// and the difference is the paragraph this module opens with: an
    /// orchestrator with kernels in `src/auth` and in `tests/` has its mean in
    /// the empty gap between them, *"which is the one place nothing is
    /// happening"*. Every drawn mark read this field until the anchor existed.
    ///
    /// Two consumers genuinely want a mean and keep it. `contention`'s
    /// `CloudSummary::centre` feeds a 4σ bounding rejection, which is a
    /// statement about the field's *spread* and therefore needs its first
    /// moment. And [`crate::place::thread_position`] keeps it as the last rung,
    /// because a territory assembled by hand — kernels never pushed through
    /// [`Territory::observe`] — has nothing else to answer with.
    ///
    /// The drift vector is **not** measured against this, despite what this doc
    /// used to say: [`Territory::drift_state`] and the drift trace both call the
    /// private `weighted_centre` over time-windowed *subsets* of the kernels and
    /// never read this field at all.
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
    /// How many times the anchor scan has run.
    ///
    /// Public for the same reason [`Territory::drift_samples`] is: the cost of
    /// this territory is O(k²) per scan and the only property worth asserting is
    /// *what triggers a scan*. A uniform rescale of every kernel — PRD §6.3's
    /// decay, and `Territory::rest`'s lift — cannot move an argmax, so a tick
    /// must not bump this.
    /// `the_anchor_scan_is_bounded_by_the_kernel_list_and_not_by_ticks` asserts
    /// exactly that, and it is the whole reason there is no clock in
    /// `Territory::refresh_anchor`.
    pub anchor_recomputes: u64,
    /// When [`Territory::decay`] last ran.
    last_decay: Option<Instant>,
    /// When [`Territory::observe`] last accepted an observation — **including**
    /// one whose claim path is the repository root, which contributes no
    /// evidence at all.
    ///
    /// The clock [`Territory::quiet_for`] reads, and it is deliberately not the
    /// evidence's own newest timestamp. The two answer different questions: the
    /// evidence says *where* this thread is working, and PRD §10.4's dormancy
    /// gate asks *whether* it still is. An agent that spends ten minutes on
    /// `cargo build && cargo test` from the checkout root produces observations
    /// that are pure liveness and pure noise about location — 57 % of them on
    /// the measured corpus — and keyed off the evidence such a thread reads
    /// dormant after [`DORMANT_AFTER`] while it is demonstrably working. Its
    /// cloud then vanishes mid-build, which is the operator's *"i dont see any
    /// clouds"*.
    ///
    /// `None` for a territory assembled by hand, which is why
    /// [`Territory::quiet_for`] still falls back to the evidence.
    last_observation: Option<Instant>,
    /// The memoised anchor: the kernel centre the field is highest at, with
    /// [`ANCHOR_MARGIN`]'s hysteresis already applied.
    anchor: Option<Point>,
    /// The field's value at the best kernel centre, memoised alongside
    /// [`Territory::anchor`] because one scan produces both and
    /// `Territory::rest` needs the peak on every tick.
    anchor_peak: f32,
    /// `kernels.len()` as of the last scan.
    ///
    /// The memo's freshness key, and it exists for a specific failure: every
    /// hand-built fixture in this workspace pushes straight into the public
    /// [`Territory::kernels`] and never calls [`Territory::observe`], so a memo
    /// maintained only from `observe` would be `None` for all of them and
    /// [`Territory::anchor`] would silently answer with the mean. A length
    /// mismatch means "somebody changed the kernels behind our back"; the
    /// accessors then scan on demand rather than lie. It does not catch a
    /// mutation that leaves the length alone, and nothing outside this module
    /// does one.
    anchor_kernels: usize,
    /// Where the map actually draws the ring, gliding toward
    /// [`Territory::anchor`] rather than snapping to it.
    ///
    /// See [`ANCHOR_GLIDE`]. `None` until the first [`Territory::decay`] after
    /// the territory has an anchor at all, at which point it *snaps* — a thread
    /// appearing for the first time should not sail in from wherever the last
    /// one was.
    drawn_anchor: Option<Point>,
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
        // Stamped **above** the root drop below, and that placement is the whole
        // of the dormancy fix. Everything that reaches this line is an
        // observation `crate::World::observe` also stepped the trail for and
        // stamped `Thread::last_activity` with; a territory that answered
        // "quiet for nine minutes" while those were arriving would be
        // contradicting the rail in the same frame.
        self.last_observation = Some(obs.at);

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
        // still steps the trail, still counts in `observations`, and — since
        // this change — has already refreshed `last_observation` above, so it
        // counts as *liveness* for PRD §10.4's dormancy even though it counts
        // for nothing in the ancestor. `crate::World::observe` also counts it in
        // `crate::Health::root_scoped_observations`, which read 11 605 of 20 246
        // observations (57.3 %) on the measured corpus and is the single number
        // that would have made the missing-clouds defect self-diagnosing.
        //
        // So "it just says nothing about *where*" is finally the whole of what
        // is taken away, rather than an aspiration: before this change the drop
        // also silently told the dormancy gate the thread had stopped.
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
        if at.is_some() {
            // The kernel list changed — pushed, and possibly evicted at the
            // window — so the argmax may have. An observation with no geometry
            // is evidence about scope and touches no kernel, so it cannot move a
            // point in city space and does not pay for a scan.
            self.refresh_anchor();
        }
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
        // A uniform rescale moves the peak's value and not its place, so the
        // memo is carried through it arithmetically rather than rescanned. This
        // is the whole reason `refresh_anchor` needs no clock.
        self.anchor_peak *= factor;
        // Before the retain, not after: at 21 minutes idle every kernel is under
        // MIN_KERNEL_WEIGHT, so a lift applied afterwards would have an empty
        // list to lift.
        self.rest();
        let kernels_before = self.kernels.len();
        self.kernels.retain(|k| k.weight >= MIN_KERNEL_WEIGHT);
        if self.kernels.len() != kernels_before {
            // Dropping kernels is the one thing decay does that is *not*
            // uniform, so it is the one thing here that can move the anchor.
            self.refresh_anchor();
        }
        for e in &mut self.evidence {
            e.weight *= factor;
        }
        self.evidence.retain(|e| e.weight >= MIN_KERNEL_WEIGHT);
        // Two conditions, and the second one is new.
        //
        // Evidence empty is not on its own proof that a thread has finished. A
        // weight-1 entry survives about 6.6 half-lives before it falls under
        // `MIN_KERNEL_WEIGHT`, so this list empties after roughly ten minutes of
        // *nothing that carries usable scope* — and a stretch of `cargo build`,
        // `cargo test`, `git status` from the checkout root is exactly that: the
        // observations arrive, `observe` drops every one of them at the root
        // check, and the non-shell evidence decays out from under a claim
        // nothing is refreshing. The thread is working the whole time. Before
        // this change its claim, its lobes and its cloud were deleted anyway.
        //
        // So the collapse also waits for PRD §10.4's own test for a thread that
        // has actually stopped — [`Territory::quiet_for`] past [`DORMANT_AFTER`]
        // — which is the same gate `select_clouds` and `Territory::rest`
        // apply, and which now reads `last_observation` and therefore sees the
        // shell calls. Until then the claim stands and `rest` holds the field up
        // underneath it. This cannot invent scope: a territory that never
        // converged has no claim and no lobes to keep, `rest` returns early for
        // it, and it still fades to nothing.
        if self.evidence.is_empty() && self.quiet_for().is_some_and(|q| q > DORMANT_AFTER) {
            // Nothing is left to claim, and nothing is coming. The thread
            // returns to an unplaced marker rather than keeping a stale district
            // forever.
            //
            // The lobes go with it. They are refreshed only in `refresh_claim`,
            // which runs on an *observation* — so a thread that stops working
            // never refreshes them again, and a lobe left standing here would
            // make [`Territory::placement`] answer "somewhere" for ever, on
            // evidence that has entirely decayed. `claim` has always been
            // cleared here for exactly that reason; before lobes existed it was
            // the whole of the answer.
            self.claim = None;
            self.lobes.clear();
            self.centre_of_mass = None;
            // The anchor goes with the mean, and for the same reason: this is
            // the branch that returns the thread to an unplaced marker, and a
            // ring drawn over a territory `select_clouds` has already dropped
            // would be the rail and the map disagreeing again. `anchor_peak` is
            // *not* cleared: kernels can outlive the evidence — `rest` holds
            // them up while the evidence keeps decaying — and the peak of the
            // kernels that are still here is still a true statement about them.
            self.anchor = None;
            // Pin the memo shut alongside it. `anchor()` recomputes whenever
            // `anchor_kernels` disagrees with the kernel list, so clearing the
            // point without clearing the count would make this branch answer
            // `None` or `Some` according to whether the `retain` above happened
            // to drop anything — which is to say, according to which tick it
            // ran on. That is exactly the cadence dependence `refresh_anchor`
            // was written to keep out (ADR-0029).
            self.anchor_kernels = self.kernels.len();
            self.drift_trace.clear();
            self.outside_streak = 0;
        } else {
            self.refresh_centre_of_mass(now);
        }
        // Last, so it eases toward whatever the rest of this tick settled on —
        // including `None`, above, which takes the ring off the map rather than
        // gliding it to nowhere.
        self.glide_anchor(dt);
    }

    /// Eases [`Territory::drawn_anchor`] toward [`Territory::anchor`] over
    /// [`ANCHOR_GLIDE`].
    ///
    /// Snaps on the first frame the territory has an anchor at all, and drops
    /// the drawn point the moment the target does: a ring that outlived its
    /// territory would be the rail and the map disagreeing, which is the failure
    /// the collapse branch above exists to avoid.
    fn glide_anchor(&mut self, dt: Duration) {
        let Some(target) = self.anchor() else {
            self.drawn_anchor = None;
            return;
        };
        let Some(drawn) = self.drawn_anchor else {
            self.drawn_anchor = Some(target);
            return;
        };
        // `1 - exp(-dt/τ)`, which composes exactly under subdivision — see
        // [`ANCHOR_GLIDE`] for why that is the load-bearing property and not an
        // implementation detail.
        let t = 1.0 - (-dt.as_secs_f32() / ANCHOR_GLIDE.as_secs_f32()).exp();
        self.drawn_anchor = Some(Point::new(
            (target.x - drawn.x).mul_add(t, drawn.x),
            (target.y - drawn.y).mul_add(t, drawn.y),
        ));
    }

    /// **Where the map draws this thread's ring** — [`Territory::anchor`],
    /// gliding.
    ///
    /// Every *decision* still reads [`Territory::anchor`]: which district a
    /// thread claims, whether a cloud is selected, how PRD §6.3's drift is
    /// measured. This is only the drawn point, and it exists so that the one
    /// moment the mode legitimately moves is a thread travelling rather than a
    /// thread blinking. See [`ANCHOR_GLIDE`].
    ///
    /// `None` before the first [`Territory::decay`], and for a territory built
    /// by hand that has never been decayed — both of which fall back to
    /// [`Territory::anchor`] at the one call site that matters,
    /// `crate::place::thread_position`.
    #[must_use]
    pub fn drawn_anchor(&self) -> Option<Point> {
        self.drawn_anchor
    }

    /// **Where this thread is working, if anywhere** — the one question every
    /// surface has to ask, and the one place it may be answered.
    ///
    /// PRD §6.2 emits a single converged ancestor; PRD §6.4 says a thread that
    /// delegates has *"two lobes and a thin connecting band"* instead. Both are
    /// placed. Reaching for [`Territory::claim`] directly asks only the first
    /// question and silently answers "nowhere" to the second, which is how a
    /// thread with 17 workers and 648 real tool calls came to read **unplaced**
    /// in the status rail while its cloud was on the map — the rail and the map
    /// disagreeing about the same thread, in the same frame.
    ///
    /// So the field is never read outside this module. Everything that means
    /// *"does this thread have somewhere"* goes through here, and the enum makes
    /// the lobed case impossible to forget: there is no `bool` to get backwards.
    pub fn placement(&self) -> Placement<'_> {
        match (&self.claim, self.lobes.as_slice()) {
            (Some(claim), _) => Placement::Claim(claim),
            (None, []) => Placement::Nowhere,
            (None, lobes) => Placement::Lobes(lobes),
        }
    }

    /// The density field's value at `p`, in the units
    /// `polis_render::live::CLOUD_ISO` is thresholded against — one full-weight
    /// kernel reads 1.0 at its own centre (ADR-0020).
    ///
    /// The same curve the rasteriser splats, from the same function
    /// ([`polis_layout::quartic`]), because the whole point of
    /// [`Territory::anchor`] is that it is the argmax of *the field that is
    /// actually drawn* rather than of a lookalike. A kernel with a
    /// non-positive radius is skipped: it has no support, and dividing by it
    /// would put a `NaN` into an ordering that decides where a mark goes.
    ///
    /// Summed in `f64` and returned as `f32`. The extra width is not for
    /// precision, it is so the world and the rasteriser call one function; the
    /// field is `f32` everywhere it is read.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)] // f64 only to share one kernel with the rasteriser
    pub fn density_at(&self, p: Point) -> f32 {
        let mut sum = 0.0f64;
        for k in &self.kernels {
            if k.radius <= 0.0 {
                continue;
            }
            let dx = f64::from(k.centre.x - p.x);
            let dy = f64::from(k.centre.y - p.y);
            let r = f64::from(k.radius);
            sum += f64::from(k.weight) * polis_layout::quartic(dx.mul_add(dx, dy * dy) / (r * r));
        }
        sum as f32
    }

    /// The highest value the field reaches, evaluated at kernel centres.
    ///
    /// A **lower bound** on the true continuous peak, and deliberately so. The
    /// maximum of a sum of overlapping compact kernels need not sit on any
    /// kernel centre — two kernels a bandwidth apart peak between them — so the
    /// exact answer needs a search over the plane, and this is the cheap
    /// estimator that is honest about being one. It is also, exactly, the
    /// quantity [`Territory::anchor`] is chosen by: the anchor has to be a real
    /// observed position, so the search space *is* the kernel centres.
    ///
    /// `polis_render::live::CloudField::peak` is a different lower bound — the
    /// maximum over a rasterisation lattice, which under-reads a sharp peak by
    /// the sub-cell offset — so neither bounds the other and the two are not
    /// interchangeable.
    ///
    /// Memoised: this is read on every tick and computing it costs a full O(k²)
    /// scan.
    #[must_use]
    pub fn field_peak(&self) -> f32 {
        if self.anchor_kernels == self.kernels.len() {
            return self.anchor_peak;
        }
        self.scan_peak().map_or(0.0, |(_, d)| d)
    }

    /// **Where this thread's mark is drawn**: the kernel centre at which the
    /// density field is highest.
    ///
    /// > A main agent has no meaningful point location — it delegates rather
    /// > than edits. Computing a centroid of its workers is actively wrong: an
    /// > orchestrator with workers in `src/auth` and `tests/` gets a centroid in
    /// > the empty gap between them, which is the one place nothing is
    /// > happening. (PRD §6)
    ///
    /// Always a position something was actually observed at, never an average of
    /// two of them. That is the difference from [`Territory::centre_of_mass`],
    /// and it is the difference between a ring on the work and a ring in the
    /// gap.
    ///
    /// [`ANCHOR_MARGIN`]'s hysteresis means this is not strictly the argmax at
    /// every instant — it is the held point until a rival clears it by the
    /// margin — which bounds the drawn density at `1 / ANCHOR_MARGIN` of
    /// [`Territory::field_peak`] rather than pinning it to the peak. That is the
    /// trade PRD §6.3 asks for: *"one read elsewhere moves nothing"*.
    ///
    /// `None` when there are no kernels — a thread working entirely outside the
    /// checkout, or one that has not been placed yet — and `None` again once
    /// [`Territory::decay`] has taken the last of the evidence, which is the
    /// branch that returns the thread to an unplaced marker. Kernels can
    /// outlive the evidence that made them (`rest` holds them up), so the
    /// second case is not the first: a territory with kernels and no claim
    /// answers `None` here on purpose, because a ring drawn over a territory
    /// [`select_clouds`] has already dropped is the rail and the map
    /// disagreeing.
    #[must_use]
    pub fn anchor(&self) -> Option<Point> {
        if self.anchor_kernels == self.kernels.len() {
            return self.anchor;
        }
        self.scan_peak().map(|(c, _)| c)
    }

    /// The kernel centre where the field is highest, and the value there.
    ///
    /// O(k²), bounded by [`OBSERVATION_WINDOW`]² = 16 384 polynomial
    /// evaluations, and run only when the kernel list actually changes — see
    /// [`Territory::refresh_anchor`].
    ///
    /// Ties are broken on `(density, x, y)` with [`f32::total_cmp`] and never
    /// with `partial_cmp`: two observations of the same building carry equal
    /// density by construction, so the tie is the common case rather than the
    /// corner one, and ADR-0029 makes a `HashMap`-flavoured answer to *"which of
    /// these equal things"* a defect and not a nuisance.
    fn scan_peak(&self) -> Option<(Point, f32)> {
        let mut best: Option<(Point, f32)> = None;
        for k in &self.kernels {
            let density = self.density_at(k.centre);
            let take = match best {
                None => true,
                Some((at, seen)) => {
                    density
                        .total_cmp(&seen)
                        .then_with(|| at.x.total_cmp(&k.centre.x))
                        .then_with(|| at.y.total_cmp(&k.centre.y))
                        == std::cmp::Ordering::Greater
                }
            };
            if take {
                best = Some((k.centre, density));
            }
        }
        best
    }

    /// Recomputes [`Territory::anchor`] and [`Territory::field_peak`] in one
    /// scan, applying [`ANCHOR_MARGIN`].
    ///
    /// # There is no clock here, and that is the point
    ///
    /// The obvious shape for something this expensive is a wall-clock throttle —
    /// recompute at most once per [`DRIFT_SAMPLE_INTERVAL`], the way the drift
    /// trace is subsampled. It would be a determinism bug (PRD §7.4, ADR-0029).
    /// `decay` runs from `World::tick`, and the two renderers tick differently:
    /// the window's `ReplayDriver::advance` ticks once per advance, the headless
    /// `run_to_end` applies a whole schedule and ticks once. A throttle plus
    /// hysteresis is a state machine over the *sequence of comparisons*, so the
    /// same recording would settle on different anchors in the window and in a
    /// recorded GIF.
    ///
    /// It is also unnecessary, because **decay cannot move the anchor**. Both
    /// things `decay` does to the weights are uniform — one `factor` across
    /// every kernel, and `Territory::rest`'s single `lift` — so every density
    /// scales by the same number and both the argmax and the margin's ratio test
    /// are invariant. Only a change to the kernel *list* can move it: a push in
    /// [`Territory::observe`], the window eviction beside it, and the `retain`
    /// in `decay` on the ticks it actually drops something. Those are the three
    /// call sites, and the anchor is a pure function of the current kernel list
    /// at each of them.
    ///
    /// The cost that leaves is one O(k²) scan per observation that lands on
    /// city ground. A full scan at [`OBSERVATION_WINDOW`] = 128 kernels is
    /// 16 384 quartic evaluations and measures **20.0 µs** in `--release` on the
    /// operator's machine (2 000 scans of a saturated territory, 40.1 ms), so
    /// PRD §13.1's 500 events/sec sustained budget costs 10 ms of each second —
    /// 1 % of one core, across the whole fleet, because that budget is the
    /// fleet's and not each thread's. If it ever stops being affordable the
    /// bound to add is a count of observations, never a clock.
    fn refresh_anchor(&mut self) {
        self.anchor_kernels = self.kernels.len();
        self.anchor_recomputes = self.anchor_recomputes.saturating_add(1);
        let Some((candidate, peak)) = self.scan_peak() else {
            self.anchor = None;
            self.anchor_peak = 0.0;
            return;
        };
        self.anchor_peak = peak;
        if let Some(held) = self.anchor {
            let held_density = self.density_at(held);
            if held_density > 0.0 && peak <= held_density * ANCHOR_MARGIN {
                // PRD §6.3: one read elsewhere moves nothing. The held point may
                // no longer be a live kernel centre — its kernel can have
                // decayed out from under it — and that is still the right answer
                // while its neighbours keep the field high there. Once they do
                // not, this test fails on its own and the ring moves.
                return;
            }
        }
        self.anchor = Some(candidate);
    }

    /// Whether the observations **agree** yet (PRD §6.2).
    ///
    /// > Do not emit a territory after a fixed number of reads. Emit when the
    /// > observations agree […] Sometimes that is two observations, sometimes
    /// > twelve. Until then the thread renders with no cloud — an unplaced
    /// > marker in the status rail.
    ///
    /// Strictly §6.2's *single ancestor* test, and therefore **not** the
    /// question "is this thread placed": an orchestrator has lobes and no
    /// converged ancestor. Use [`Territory::placement`] for that.
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
                lobes: Vec::new(),
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
                lobes: Vec::new(),
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
        let lobes = Self::lobes_of(retained, &self.evidence, total);
        Convergence {
            claim,
            observations: n,
            depth,
            mass_ratio,
            trimmed: trim,
            lobes,
        }
    }

    /// PRD §6.4's lobes: the places this thread is actually working, when there
    /// is more than one.
    ///
    /// §6.2 takes a single ancestor over every retained observation, which is
    /// right for a focused thread and wrong for the case §6.4 exists to
    /// describe: *"An agent working in `auth` with one worker in `tests` gets
    /// two lobes and a thin connecting band."* The moment a thread has two
    /// well-separated lobes their common ancestor collapses toward the root, so
    /// the depth gate rejects it and the thread gets **no cloud at all** — the
    /// two sections contradict each other.
    ///
    /// Measured on this repository: one orchestrator with 101 workers made
    /// 3 386 path-bearing calls across 18 top-level directories. Its trimmed
    /// ancestor is the repo root, depth 0, so it drew nothing — while every one
    /// of its workers was in a nameable place.
    ///
    /// So the depth-and-mass test is applied **per cluster** instead. Evidence
    /// is grouped by its own [`MIN_CLAIM_DEPTH`]-deep prefix, and a group
    /// becomes a lobe when it carries at least [`MIN_LOBE_MASS`] of the whole.
    /// A focused thread yields exactly one lobe and behaves as before; an
    /// orchestrator yields several and can finally be drawn.
    fn lobes_of(retained: &[usize], evidence: &VecDeque<Evidence>, total: f32) -> Vec<Lobe> {
        if total <= 0.0 {
            return Vec::new();
        }
        // BTreeMap, not HashMap: this feeds what gets drawn, and PRD §7.4 does
        // not allow iteration order to reach the picture.
        let mut by_prefix: BTreeMap<LogicalPath, f32> = BTreeMap::new();
        for idx in retained {
            let e = &evidence[*idx];
            let Some(prefix) = e.path.ancestor_at(MIN_CLAIM_DEPTH) else {
                // Shallower than the gate allows on its own — a top-level file
                // cannot name a lobe, exactly as it cannot name a claim.
                continue;
            };
            *by_prefix.entry(prefix).or_insert(0.0) += e.weight;
        }
        let mut lobes: Vec<Lobe> = by_prefix
            .into_iter()
            .filter_map(|(path, weight)| {
                let mass = weight / total;
                (mass >= MIN_LOBE_MASS).then_some(Lobe { path, mass })
            })
            .collect();
        // Heaviest first, ties by path so the order is stable across machines.
        lobes.sort_by(|a, b| {
            b.mass
                .partial_cmp(&a.mass)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.path.cmp(&b.path))
        });
        lobes.truncate(MAX_LOBES);
        lobes
    }

    /// Holds a converged territory at a visible resting level once its work
    /// stops, instead of letting it fade to nothing.
    ///
    /// PRD §6.3's decay is right while a fleet is running: *"Contract slowly.
    /// Kernel weights decay with a half-life of 90 s."* But it is applied from
    /// each observation's own timestamp, so it also governs what an operator
    /// sees the moment they **open** the map — and there, it deletes the answer.
    ///
    /// Measured on the operator's own machine: three real sessions, idle for 3,
    /// 11 and 21 minutes, retain 25 %, 0.62 % and 0.0061 % of their weight. The
    /// second and third fall under [`MIN_KERNEL_WEIGHT`] and are dropped
    /// outright. So `polis watch` opened on a repository whose agents had just
    /// been working showed an empty map, and the operator's report was exactly
    /// that: *"clouds should be there immediately"*, three times.
    ///
    /// A territory that has converged has *earned* a shape, and that shape is
    /// still the truth about where the thread was working. So decay stops at
    /// [`RESTING_WEIGHT`]: the field keeps its form, rescaled — never
    /// redistributed, so a lobe cannot grow relative to another while nothing is
    /// happening — and stays faint but drawable.
    ///
    /// # What is normalised, and why it is not the mass
    ///
    /// The lift is chosen so that [`Territory::field_peak`] lands on
    /// [`RESTING_WEIGHT`]. It used to be chosen so that `Σw` did, and those are
    /// only the same number for a territory whose kernels sit on one point.
    /// `polis_render::live::CLOUD_ISO` is thresholded against the field, so a
    /// spread territory normalised by mass rests with a peak well under the
    /// fringe and is drawn as nothing at all: 7.0 % of selected clouds, at a mean
    /// peak of 0.429. [`RESTING_WEIGHT`]'s doc carries the measurement.
    ///
    /// # Why this is affordable at 2 Hz
    ///
    /// This runs from [`Territory::decay`], which runs for every thread on every
    /// `crate::World::tick` — 2 Hz at `crate::LIVE_TICK`, times PRD §16's
    /// hundred threads. A peak is an O(k²) scan at `k <= OBSERVATION_WINDOW`, and
    /// paying that here unconditionally would be 16 384 polynomial evaluations
    /// per thread per tick. It is not paid: [`Territory::field_peak`] is memoised
    /// beside [`Territory::anchor`], and both of the rescales in play — decay's
    /// `factor` and this function's own `lift` — are *uniform*, so the memo is
    /// carried through them by one multiply. The scan happens only when the
    /// kernel **list** changes. A territory assembled by hand — kernels pushed
    /// straight into the public [`Territory::kernels`] and
    /// [`Territory::observe`] never called — has a stale memo by construction
    /// and does pay a scan on every tick it is decayed; every such territory in
    /// this workspace is a fixture with a handful of kernels, and nothing in the
    /// live path builds one.
    ///
    /// # Dormancy still wins
    ///
    /// Nothing here keeps a dead thread alive. PRD §10.4's cap still drops a
    /// territory once [`Territory::quiet_for`] passes its dormancy window, which
    /// is the mechanism §10.4 actually names for letting *"dormant territories
    /// dissipate entirely"*, and the early return below applies the same gate so
    /// that the field is genuinely gone by the time the cap stops asking. This
    /// only stops the field vanishing in the minutes before that decision is due.
    fn rest(&mut self) {
        // Dormancy still wins. PRD §10.4: "let dormant territories dissipate
        // entirely" — resting holds a field through the pause between two
        // bursts of work, not forever. Past DORMANT_AFTER the thread is no
        // longer drawn at all, so holding its field up would only cost memory
        // and lie to `quiet_for`.
        if self.quiet_for().is_some_and(|q| q > DORMANT_AFTER) {
            return;
        }
        if self.claim.is_none() && self.lobes.is_empty() {
            // Never converged: an unfinished scatter still fades away, which is
            // what stops a stray read leaving a permanent smudge on the map.
            return;
        }
        let peak = self.field_peak();
        if peak <= 0.0 || peak >= RESTING_WEIGHT {
            return;
        }
        let lift = RESTING_WEIGHT / peak;
        for k in &mut self.kernels {
            k.weight *= lift;
        }
        // Uniform, like the decay factor above it: every density scales by
        // exactly `lift`, so the peak lands on RESTING_WEIGHT by construction
        // and the mode does not move at all. Updating the memo rather than
        // invalidating it is what keeps the next tick's `field_peak` free.
        self.anchor_peak *= lift;
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
    /// So the vector runs from the weighted centre of the field's *established*
    /// kernels to the centre of the **last [`DRIFT_WINDOW`] of work** — not from
    /// [`Territory::centre_of_mass`], see the next paragraph. That fires on the
    /// first minute of
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
    /// The mark runs from the weighted centre of the kernels **older** than
    /// [`DRIFT_WINDOW`] to the weighted centre of the ones inside it, and it is
    /// drawn at the **far** end. Neither end is [`Territory::centre_of_mass`] —
    /// that is a mean over the *whole* field, and including the recent kernels
    /// in their own baseline is exactly the dilution `drift_state` explains it
    /// avoids. Nor is either end [`Territory::anchor`]: this signal is about a
    /// centre moving, so it wants first moments at both ends, and putting a mode
    /// at the tail would make the mark jump a district at a time. The tail is
    /// where the thread has been; the tip is where it is going. That is why no
    /// extrapolation is needed to place the mark — the leading edge is a
    /// measured position, not a projected one.
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

    /// How long since this thread last did **anything** (PRD §10.4's dormancy).
    ///
    /// No `now` argument on purpose. [`Territory::decay`] runs from
    /// `World::tick` every tick, so `last_decay` **is** now, and a policy that
    /// took its own clock could disagree with the field it is judging. `None`
    /// before the first observation.
    ///
    /// # Anything, not any *evidence*
    ///
    /// This used to measure from the newest entry in [`Territory::evidence`],
    /// and that made three separate consumers — `Territory::rest`,
    /// [`Territory::decay`]'s collapse and [`select_clouds`]'s dormancy gate —
    /// answer *"has this thread stopped?"* with a fact about *where it was
    /// working*. A shell call scoped to the repository root leaves no evidence
    /// (see [`Territory::observe`]) and 57 % of real observations are exactly
    /// that, so an agent in the middle of a build-and-test stretch went quiet on
    /// this clock while it was making a call a second, lost its cloud at
    /// [`DORMANT_AFTER`], and got it back only when it next touched a file.
    ///
    /// So it reads `last_observation`, which every accepted observation stamps
    /// before the root drop. The consequence to state plainly: a thread that has
    /// been running `cargo test` from the root for twenty minutes and nothing
    /// else is **not** dormant here, and its cloud stays over whatever it was
    /// last working on. That is the honest reading — the field is stale about
    /// *where*, and PRD §6.3's decay is the thing that already says so by
    /// letting it fade — where "dormant" would have been a false claim that the
    /// agent had stopped.
    ///
    /// The evidence maximum survives as a fallback for a [`Territory`] whose
    /// kernels and evidence were assembled by hand, which is every cloud fixture
    /// in this workspace and none of the live path.
    pub fn quiet_for(&self) -> Option<Duration> {
        let now = self.last_decay?;
        let newest = self
            .last_observation
            .or_else(|| self.evidence.iter().map(|e| e.at).max())?;
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
        let convergence = self.convergence();
        // Lobes track the evidence directly. They need no hysteresis of their
        // own: PRD §6.3's smoothing already lives on the kernel weights the
        // field is drawn from, and a lobe that stops being worked simply loses
        // mass and drops below MIN_LOBE_MASS.
        self.lobes = convergence.lobes;
        let Some(candidate) = convergence.claim else {
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
    /// PRD §6.4's lobes — the separate places this thread is working, heaviest
    /// first. Empty for a thread with nothing converged anywhere; one entry for
    /// a focused thread; several for an orchestrator whose workers are spread
    /// across the tree, which is the case a single ancestor cannot express.
    pub lobes: Vec<Lobe>,
    /// How many observations are live.
    pub observations: usize,
    /// `depth(A)`. Must reach [`MIN_CLAIM_DEPTH`].
    pub depth: usize,
    /// Mass inside `A` over total. Must exceed [`CONVERGENCE_MASS`].
    pub mass_ratio: f32,
    /// How many observations the trim dropped.
    pub trimmed: usize,
}

/// One place a thread is working (PRD §6.4).
///
/// A territory is a density field, not a boundary, and a thread that delegates
/// has more than one centre. A lobe names one of them so the field can be drawn
/// as *"two lobes and a thin connecting band"* rather than collapsing to a
/// single ancestor that is either too broad to mean anything or missing
/// entirely.
#[derive(Debug, Clone, PartialEq)]
pub struct Lobe {
    /// The directory this lobe covers, exactly [`MIN_CLAIM_DEPTH`] deep.
    pub path: LogicalPath,
    /// Share of the thread's whole weight that sits inside it, in `[0, 1]`.
    ///
    /// Carried so the renderer can make a minor lobe read as minor: the band
    /// between a heavy lobe and a light one should be thin, and the light lobe
    /// smaller, because that is what is true.
    pub mass: f32,
}

/// Where a thread is working — [`Territory::placement`]'s answer.
///
/// Three states, not two, and the third is the one that keeps being lost. PRD
/// §6.2 and §6.4 describe *different* placed shapes, and a caller that tests
/// `claim.is_some()` has quietly decided that §6.4's shape is unplaced.
///
/// [`Placement::Nowhere`] is a real and useful state, not a failure: §6.2 says
/// an unconverged thread *"renders with no cloud — an unplaced marker in the
/// status rail"*, and [`Territory::convergence`] carries the honest reason.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Placement<'a> {
    /// PRD §6.2's converged ancestor: one district, and the evidence agrees.
    Claim(&'a LogicalPath),
    /// PRD §6.4's lobes, heaviest first — several places, no single ancestor
    /// above the root that could name them. Never empty.
    Lobes(&'a [Lobe]),
    /// No territory yet. The rail says so, and says why.
    Nowhere,
}

impl<'a> Placement<'a> {
    /// Whether this thread has anywhere at all — a cloud, a district name, a
    /// row in the rail that can say *where* rather than *unplaced*.
    ///
    /// The predicate lives on the enum rather than on [`Territory`] so that
    /// asking it forces a caller to have gone through
    /// [`Territory::placement`] first, and so `Lobes` cannot be dropped on the
    /// floor by a `claim.is_some()` written from memory.
    #[must_use]
    pub fn is_somewhere(self) -> bool {
        !matches!(self, Self::Nowhere)
    }

    /// Every place this thread is working, heaviest first. Empty for
    /// [`Placement::Nowhere`].
    ///
    /// One name for a focused thread, up to [`MAX_LOBES`] for an orchestrator.
    /// Borrows rather than allocating: this runs once per thread per frame.
    pub fn paths(self) -> impl Iterator<Item = &'a LogicalPath> {
        let (claim, lobes) = match self {
            Self::Claim(c) => (Some(c), [].as_slice()),
            Self::Lobes(l) => (None, l),
            Self::Nowhere => (None, [].as_slice()),
        };
        claim.into_iter().chain(lobes.iter().map(|l| &l.path))
    }

    /// The single district to name this thread by: its claim, else its heaviest
    /// lobe.
    ///
    /// For the surfaces that genuinely need *one* path — PRD §11.3's overlap
    /// pair, which asks whether two orchestrators are in the same district. The
    /// heaviest lobe is the honest answer there: it is where most of the
    /// thread's weight is, and it is the lobe a reader's eye lands on.
    #[must_use]
    pub fn district(self) -> Option<&'a LogicalPath> {
        self.paths().next()
    }
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
///
/// # Re-run, and the cliff is not there any more
///
/// `Territory::rest`'s move from normalising the field's mass to normalising
/// its peak was expected to brighten and widen every resting territory, so the
/// harness above was re-run on both sides of that change — same forty sessions,
/// same alignment, same instant, `cargo test -p polis-render --release --test
/// cloud_cap_policy -- --ignored --nocapture`:
///
/// | cap | clouds | cloud ink | contested ink | mean crowd |
/// |---|---|---|---|---|
/// | 2 | 2 | 1.31 % | 29.9 % | 1.27 |
/// | 4 | 4 | 2.14 % | 34.8 % | 1.41 |
/// | **5** | 5 | 2.99 % | **27.0 %** | **1.39** |
/// | 6 | 6 | 3.21 % | 27.9 % | **1.54** |
/// | 10 | 10 | 3.44 % | 41.5 % | 1.99 |
/// | 24 | 24 | 4.66 % | 44.6 % | 3.10 |
///
/// **Two findings, and only the second is about this cap.**
///
/// The first: every number in that column is *identical* before and after the
/// `rest` change, to the digit. At this harness's measurement instant — each
/// session aligned on its own busiest minute — nothing is resting, so `rest`
/// never fires and cannot move the ink. What did move is the dormancy sweep, and
/// only there; see [`DORMANT_AFTER`]. So **this cap is not re-baselined by that
/// change**, and 5 stands on the argument above.
///
/// The second is that the argument above no longer describes the fleet. There is
/// no 37.7 % → 58.3 % cliff between five and six; contested ink is 27.0 % → 27.9
/// %, and the mean-crowd 1.5 crossing has moved from five to **six**. The fleet
/// itself changed underneath the table — PRD §6.4's lobes (`fb414aa`) mean 24 of
/// these threads are placed where 11 were, so the same forty sessions now put
/// more, smaller territories on the map. Read strictly, this sweep would support
/// a cap of six. That is a decision about what the operator sees and not a
/// consequence of the clouds fix, so the cap is held at five and the
/// disagreement is written down rather than acted on.
pub const CLOUD_CAP: usize = 5;

/// How long a converged territory may sit untouched before it dissipates
/// (PRD §10.4).
///
/// > let dormant territories dissipate entirely
///
/// Deliberately equal to `polis_ingest::live::LIVE_WINDOW`, which is also eight
/// minutes: `ab844b1` moved this constant to match it *"so a thread on the map
/// has a cloud and one too old to have a cloud is not listed"*. A territory that
/// dissipated earlier would take the cloud off a session the ingest side is
/// still tailing.
///
/// # This is now the claim's lifetime, not just the cloud's
///
/// It used to gate one thing — whether [`select_clouds`] drew a territory this
/// frame — while [`Territory::claim`] and [`Territory::lobes`] were deleted on a
/// separate schedule, whenever the evidence list happened to empty. That
/// schedule was wrong (see [`Territory::decay`]), so the collapse now waits for
/// this window too. Three consumers key on it: `select_clouds`,
/// `Territory::rest`, and the collapse. Lengthening it keeps a stale district
/// name in the rail for longer; shortening it takes the map away from an agent
/// that paused to think.
///
/// # The sweep below is stale, and is kept because it is still the only one
///
/// It was taken at **180 s**, on the same forty-session fleet as [`CLOUD_CAP`],
/// with the cap lifted so the window was the only thing acting:
///
/// | window | clouds shown | dissipated | cloud ink |
/// |---|---|---|---|
/// | 30 s | 8 | 3 | 3.14 % |
/// | 90 s | 8 | 3 | 3.14 % |
/// | **180 s** | 10 | 1 | 3.19 % |
/// | 600 s | 11 | 0 | 3.25 % |
/// | none | 11 | 0 | 3.25 % |
///
/// `ab844b1` moved the constant from 180 s to eight minutes and left this table
/// describing the value it had replaced. Two of its readings survive the move
/// and one does not:
///
/// * The window is a **small** effect — one to three territories of eleven, and
///   about 3 % of the cloud ink. That still holds, and 480 s sits between the
///   180 s and 600 s rows where the effect is smallest of all.
/// * *"30 s buys nothing over 180 s but would strip the cloud off any thread
///   that pauses half a minute to think"* still holds, and is the argument
///   against every shorter value.
/// * *"at 600 s the gate has nothing left to catch, since a kernel's weight is
///   under the drop floor after about ten minutes and the territory has
///   dissipated on its own"* is **no longer true**, and that is the sentence
///   this change invalidates. `Territory::rest` pins the field's peak at
///   [`RESTING_WEIGHT`] for as long as the thread is not dormant, so nothing
///   dissipates on its own any more and this window is the only thing that ends
///   a territory.
///
/// # Re-run, on both sides of the `last_observation` change
///
/// Same harness, same forty-session fleet, cap lifted so the window is the only
/// thing acting. The rows the harness sweeps are 30/90/180/600 s; 480 s is not
/// one of them and sits inside the flat top:
///
/// | window | shown before | shown after | dissipated before | dissipated after |
/// |---|---|---|---|---|
/// | 30 s | 13 | **15** | 11 | 9 |
/// | 90 s | 16 | **19** | 8 | 5 |
/// | 180 s | 22 | **24** | 2 | 0 |
/// | 600 s | 24 | 24 | 0 | 0 |
/// | none | 24 | 24 | 0 | 0 |
///
/// This is the whole of what reading `last_observation` does to a real fleet,
/// and it is the right shape: two to three more territories survive each short
/// window, and none survives that would not have survived at 600 s. At the value
/// actually shipped the sweep is already flat — **nothing dissipates at 480 s on
/// this fleet, before or after** — which is the same "small effect" the earlier
/// table found, taken at the measurement instant this harness is honest about
/// preferring (see its header). A fleet sampled on a wall clock rather than at
/// its peak would have more dormant territories and more for this window to
/// catch; that harness is not written.
///
/// The same conclusion in one line: the effect is real, it is small, and it is
/// entirely in the short windows this constant is not set to.
///
/// PRD §17's open question — *dissipate entirely or leave a faint residue?* — is
/// answered **dissipate**, for the reason in [`CloudSelection::dormant`]. Note
/// that the answer now costs something: with `rest` holding the field up, a
/// residue is what you get by *lengthening* this window rather than by adding a
/// floor, so the two are one knob.
pub const DORMANT_AFTER: Duration = Duration::from_mins(8);

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
        // A thread with lobes but no claim is the orchestrator case (PRD §6.4):
        // its workers are each somewhere nameable, but their common ancestor is
        // the root, so §6.2's single-ancestor gate rejects it. Drawing nothing
        // there is what made a 101-worker session invisible on its own map, so
        // the test is [`Territory::placement`] and not the claim field.
        //
        // Kernels are a separate question and stay separate: placement is what
        // the *evidence* says, kernels are whether any of it landed on ground
        // this city has geometry for. A thread working entirely outside the
        // checkout is placed and has nothing to splat.
        if !territory.placement().is_somewhere() || territory.kernels.is_empty() {
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
/// **Nothing that draws calls this any more, deliberately.** It was written for
/// callers with neither an attention list nor an opinion about dormancy, on the
/// argument that the policy still applies so the two paths cannot disagree.
/// Both halves of that turned out to be wrong in the same place: it returns only
/// [`CloudSelection::visible`] and drops `unplaced`, `dormant` and `capped` on
/// the floor, so `polis_app::clouds` — its last real caller — could put an empty
/// sky on the map and had nothing to say about why; and passing no attention
/// list means the rank in PRD §10.4's *"threads with an active attention state
/// **or** in the top N"* is missing, so the window and `polis_render::frame`
/// could choose a different five under the cap.
///
/// It survives as the convenience the cap's own tests are written against.
/// Anything that draws should call [`select_clouds`] and keep the
/// [`CloudSelection`].
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
        // The third number the rail's `unplaced` hover now shows beside the
        // other two, and the reason it has to: `Convergence::observations` is
        // `evidence.len()`, so a thread that has made forty-nine calls reads
        // "9 observations" here and would read "0" after ten minutes of the
        // same. `Territory::observations` is the one the operator recognises.
        assert_eq!(
            t.evidence.len(),
            9,
            "the live evidence window is the smaller of the two numbers"
        );
        assert_eq!(
            t.quiet_for(),
            Some(Duration::ZERO),
            "and forty root-scoped shell calls leave the thread *alive*, which \
             is the half of them that is not thrown away"
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

    // -----------------------------------------------------------------------
    // PRD §10.4's dormancy is a statement about the *thread*, not about its
    // evidence. The operator's report: *"i dont see any clouds"*, on a fleet
    // that was building and testing.
    // -----------------------------------------------------------------------

    /// A `Bash` observation scoped to the checkout root — `cargo test` from the
    /// repository top, which is what an agent actually spends its calls on.
    fn root_shell(at: Instant) -> Observation {
        Observation {
            thread: ThreadId::of_session(SessionId::new("s")),
            worker: None,
            path: LogicalPath::root(),
            scope: PathScope::Directory,
            tool: ToolKind::Bash,
            at,
            weight: None,
        }
    }

    /// The reported defect, end to end. A converged thread runs nothing but
    /// root-scoped shell calls for twenty minutes — three times the ten it takes
    /// its non-shell evidence to decay under [`MIN_KERNEL_WEIGHT`] — and keeps
    /// its district and its cloud the whole way, because it never stopped
    /// working.
    #[test]
    fn a_converged_territory_survives_a_shell_only_stretch() {
        let mut t = Territory::for_extent(1000.0);
        let t0 = Instant::now();
        for i in 0..9 {
            t.observe(
                &obs(&format!("src/auth/f{i}.rs"), ToolKind::Edit, t0),
                1.0,
                place(i),
            );
        }
        assert_eq!(t.claim.as_ref().map(LogicalPath::as_str), Some("src/auth"));

        // Twenty minutes of build-and-test, one call every fifteen seconds, with
        // a tick between each — which is what `World::tick` does at 2 Hz.
        for step in 1..=80u32 {
            let at = t0 + Duration::from_secs(u64::from(step) * 15);
            t.observe(&root_shell(at), 1.0, place(0));
            t.decay(at);
        }

        assert!(
            t.evidence.is_empty(),
            "the premise: a shell call at the root leaves no evidence, so the \
             non-shell evidence has decayed out from under the claim"
        );
        assert_eq!(
            t.claim.as_ref().map(LogicalPath::as_str),
            Some("src/auth"),
            "and the claim must survive it: the thread never stopped working"
        );
        assert!(
            t.placement().is_somewhere(),
            "so the rail says where instead of `unplaced`"
        );
        assert!(
            !t.kernels.is_empty(),
            "and `select_clouds` has something to splat"
        );
        assert!(
            t.quiet_for().is_some_and(|q| q < DORMANT_AFTER),
            "dormancy asks whether the thread stopped, and it did not: {:?}",
            t.quiet_for()
        );

        // And then it does stop. Nothing at all past the dormancy window, and it
        // goes exactly as it went before — PRD §10.4's *"let dormant territories
        // dissipate entirely"*.
        let last = t0 + Duration::from_mins(20);
        t.decay(last + DORMANT_AFTER + Duration::from_secs(1));
        t.decay(last + DECAY_HALF_LIFE * 12);
        assert!(t.claim.is_none(), "back to an unplaced marker");
        assert_eq!(t.placement(), Placement::Nowhere);
        assert!(t.kernels.is_empty(), "and the field is gone, not faint");
    }

    /// The half of the fix that does the work, isolated: a root-scoped shell
    /// call is worth nothing to the ancestor and everything to the clock.
    #[test]
    fn a_root_scoped_shell_call_refreshes_the_dormancy_clock_and_nothing_else() {
        let mut t = Territory::for_extent(1000.0);
        let t0 = Instant::now();
        t.observe(&obs("src/auth/a.rs", ToolKind::Edit, t0), 1.0, place(0));
        t.observe(&obs("src/auth/b.rs", ToolKind::Edit, t0), 1.0, place(1));
        let evidence_before = t.evidence.len();

        let later = t0 + DORMANT_AFTER + Duration::from_secs(60);
        t.observe(&root_shell(later), 1.0, place(0));

        assert_eq!(
            t.evidence.len(),
            evidence_before,
            "it says nothing about where, so it leaves no evidence"
        );
        assert_eq!(t.kernels.len(), 2, "nor a kernel in the empty middle");
        assert_eq!(t.observations, 3, "it is still counted: the work happened");
        assert_eq!(
            t.quiet_for(),
            Some(Duration::ZERO),
            "and it says everything about whether: the thread is alive now"
        );
    }

    /// The fallback that keeps every hand-built cloud fixture in this workspace
    /// working. None of them call [`Territory::observe`], so `last_observation`
    /// is `None` and the evidence's own newest stamp has to answer.
    #[test]
    fn a_hand_built_territory_still_answers_the_dormancy_question() {
        let t0 = Instant::now();
        let mut t = Territory::for_extent(1000.0);
        t.evidence.push_back(Evidence {
            path: lp("src/auth"),
            weight: 1.0,
            at: t0,
        });
        t.claim = Some(lp("src/auth"));
        t.kernels.push(Kernel {
            centre: Point::new(0.0, 0.0),
            weight: 1.0,
            radius: 10.0,
            at: t0,
        });
        assert_eq!(t.quiet_for(), None, "no clock has run yet");

        t.decay(t0);
        t.decay(t0 + DORMANT_AFTER.saturating_sub(Duration::from_secs(1)));
        assert!(
            t.quiet_for().is_some_and(|q| q < DORMANT_AFTER),
            "the evidence's own stamp answers"
        );
        assert!(t.claim.is_some(), "so the claim stands");
    }

    /// `Territory::rest` normalises the field's **peak**, because
    /// `polis_render::live::CLOUD_ISO[0] = 0.55` thresholds the field and not the
    /// mass. Expressed here without the render dependency: the number is the one
    /// ADR-0020 fixes, and a spread field is where the two diverge.
    #[test]
    fn a_rested_field_reaches_the_fringe() {
        /// `polis_render::live::CLOUD_ISO[0]`, in ADR-0020's units — one
        /// full-weight kernel reads 1.0 at its own centre.
        const FRINGE: f32 = 0.55;

        let t0 = Instant::now();
        let mut t = Territory::for_extent(1000.0);
        // Two well-separated lobes, six observations each: PRD §6.4's shape, and
        // the one whose mass is several times its peak.
        for i in 0..6u32 {
            t.observe(
                &obs(&format!("src/auth/f{i}.rs"), ToolKind::Edit, t0),
                1.0,
                Some(Point::new(0.0, 0.0)),
            );
            t.observe(
                &obs(&format!("tests/unit/f{i}.rs"), ToolKind::Edit, t0),
                1.0,
                Some(Point::new(900.0, 900.0)),
            );
        }
        assert!(t.placement().is_somewhere(), "it has lobes to hold");

        // Seven and a half minutes quiet. Inside [`DORMANT_AFTER`] — eight — so
        // `rest` still acts, and long enough that PRD §6.3's half-life has taken
        // the field under the floor, which is when `rest` has anything to do at
        // all. The window between those two is narrow for a heavily evidenced
        // territory, and normalising the peak rather than the mass is what
        // widens it: a peak crosses the floor sooner than the mass it is a
        // fraction of.
        t.decay(t0 + Duration::from_secs(450));
        let peak = t.field_peak();
        let mass = t.mass();
        assert!(
            (peak - RESTING_WEIGHT).abs() < 1e-3,
            "the peak is what is normalised: {peak}"
        );
        assert!(
            peak >= FRINGE,
            "or the cloud is computed, selected and never painted: {peak}"
        );
        assert!(
            mass > peak * 1.5,
            "and the mass is the number that used to be normalised — several \
             times the peak on any spread field, which is why it was the wrong \
             one: mass {mass}, peak {peak}"
        );
    }

    /// The other end of the trade. Normalising the peak brightens a resting
    /// field; it must not brighten it past a thread that is actually working, or
    /// PRD §10.3's *"spend the rest on layers 4–5"* is inverted.
    #[test]
    fn a_resting_field_is_fainter_than_a_working_one() {
        let t0 = Instant::now();
        let mut t = Territory::for_extent(1000.0);
        for i in 0..8 {
            t.observe(
                &obs(&format!("src/auth/f{i}.rs"), ToolKind::Edit, t0),
                1.0,
                place(i),
            );
        }
        let working = t.field_peak();
        t.decay(t0 + Duration::from_secs(450));
        let resting = t.field_peak();
        assert!(
            resting < working,
            "resting {resting} must be fainter than working {working}"
        );
        assert!(
            (resting - RESTING_WEIGHT).abs() < 1e-3,
            "and it must still clear the fringe: {resting}"
        );
    }

    // -----------------------------------------------------------------------
    // PRD §6.2 vs §6.4: "is this thread placed" is one question with two
    // answers, and `Territory::placement` is the only place it is asked.
    // -----------------------------------------------------------------------

    /// The shape of the thread the operator was staring at: 17 workers spread
    /// over three top-level directories, so §6.2's trimmed ancestor is the
    /// repository root and `claim` is empty — while §6.4's lobes name every
    /// place it is working and its cloud is on the map.
    fn spread_orchestrator() -> Territory {
        let mut t = Territory::for_extent(1000.0);
        let now = Instant::now();
        // Interleaved, because PRD §6.3's hysteresis is "expand readily,
        // contract slowly": a thread that works in one directory first and only
        // then fans out keeps that first claim. A real orchestrator's workers
        // report from everywhere at once, which is the case being fixtured.
        let places = [
            "src/components/WorkspaceRail.jsx",
            "src/hooks/useWindowCentering.js",
            "tests/hooks/useDragResizeEdgePan.test.jsx",
        ];
        for k in 0..24u32 {
            for (i, path) in places.iter().enumerate() {
                // The heaviest lobe first, so `district()` has a stable answer:
                // `components` reports on every pass, `hooks` on two in three,
                // `tests` on one in three.
                if k as usize % 3 < 3 - i {
                    let slot = k * 8 + u32::try_from(i).expect("three places");
                    t.observe(
                        &obs(
                            path,
                            ToolKind::Read,
                            now + Duration::from_millis(slot.into()),
                        ),
                        1.0,
                        place(slot),
                    );
                }
            }
        }
        t
    }

    #[test]
    fn a_lobed_thread_is_placed_even_though_it_has_no_claim() {
        let t = spread_orchestrator();
        assert!(
            t.claim.is_none(),
            "the fixture must be the case §6.2 cannot describe, got {:?}",
            t.claim
        );
        assert!(
            !t.lobes.is_empty(),
            "but §6.4 must be able to describe it: {:?}",
            t.lobes
        );
        assert!(
            t.placement().is_somewhere(),
            "a thread with lobes is placed; reading `claim` alone is what printed `unplaced` \
             on a thread with 648 tool calls"
        );
        let named: Vec<&str> = t.placement().paths().map(LogicalPath::as_str).collect();
        assert!(
            named.contains(&"src/components") && named.contains(&"src/hooks"),
            "the rail has to be able to say where: {named:?}"
        );
    }

    #[test]
    fn placement_names_the_claim_first_and_the_heaviest_lobe_otherwise() {
        let mut focused = Territory::for_extent(1000.0);
        let now = Instant::now();
        focused.observe(&obs("src/auth/a.rs", ToolKind::Edit, now), 1.0, place(0));
        focused.observe(&obs("src/auth/b.rs", ToolKind::Edit, now), 1.0, place(1));
        assert_eq!(
            focused.placement().district().map(LogicalPath::as_str),
            Some("src/auth"),
            "a converged thread is named by its claim"
        );
        assert_eq!(
            focused.placement().paths().count(),
            1,
            "and by exactly one place, however many lobes the evidence forms"
        );

        let spread = spread_orchestrator();
        assert_eq!(
            spread.placement().district().map(LogicalPath::as_str),
            Some("src/components"),
            "an orchestrator is named by its heaviest lobe — PRD §11.3's pair needs one path"
        );
    }

    #[test]
    fn an_unconverged_thread_is_nowhere_and_says_why() {
        let mut t = Territory::for_extent(1000.0);
        let now = Instant::now();
        t.observe(
            &obs("src/auth/token.rs", ToolKind::Read, now),
            1.0,
            place(0),
        );
        assert_eq!(t.placement(), Placement::Nowhere);
        assert!(!t.placement().is_somewhere());
        assert_eq!(t.placement().paths().count(), 0);
        assert!(
            t.convergence().observations > 0,
            "and §6.2's reason is still available for the rail's hover"
        );
    }

    /// Lobes are refreshed only by an observation, so a thread that stops
    /// working never refreshes them again. If `decay` did not clear them, a dead
    /// territory would answer "somewhere" for ever on evidence that has entirely
    /// gone — the rail would keep saying `in src/components` for a session that
    /// ended an hour ago.
    #[test]
    fn lobes_do_not_outlive_the_evidence_that_made_them() {
        let mut t = spread_orchestrator();
        assert!(t.placement().is_somewhere());
        let t0 = t.last_decay.expect("the fixture observed something");
        t.decay(t0 + DECAY_HALF_LIFE * 12);
        assert!(t.evidence.is_empty(), "the evidence has decayed away");
        assert!(
            t.lobes.is_empty(),
            "and the lobes went with it, exactly as the claim does: {:?}",
            t.lobes
        );
        assert_eq!(
            t.placement(),
            Placement::Nowhere,
            "back to an unplaced marker (PRD §6.2)"
        );
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

    // ---------------------------------------------------------------------
    // The drawn anchor (PRD §6)
    //
    // > Computing a centroid of its workers is actively wrong: an orchestrator
    // > with workers in `src/auth` and `tests/` gets a centroid in the empty gap
    // > between them, which is the one place nothing is happening.
    //
    // This module has opened on that sentence since it was written, and
    // `refresh_centre_of_mass` was a centroid the whole time. These are the
    // tests that would have caught it the day the doc was written.
    // ---------------------------------------------------------------------

    #[allow(clippy::unnecessary_wraps)] // every call site passes an `Option<Point>`
    fn spot(x: f32, y: f32) -> Option<Point> {
        Some(Point::new(x, y))
    }

    #[allow(clippy::cast_precision_loss)] // small test indices
    fn step(i: usize, gap: f32) -> f32 {
        i as f32 * gap
    }

    /// PRD §6's opening paragraph, as an assertion.
    #[test]
    fn the_anchor_is_never_in_the_gap_between_two_lobes() {
        let mut t = Territory::for_extent(1000.0);
        let now = Instant::now();
        // Six edits in `src/auth`, tightly clustered.
        for i in 0..6 {
            t.observe(
                &obs(&format!("src/auth/f{i}.rs"), ToolKind::Edit, now),
                1.0,
                spot(step(i, 4.0), 0.0),
            );
        }
        // Four in `tests/unit`, four hundred units away — well past any
        // bandwidth this territory can reach for.
        for i in 0..4 {
            t.observe(
                &obs(&format!("tests/unit/g{i}.rs"), ToolKind::Edit, now),
                1.0,
                spot(400.0 + step(i, 4.0), 0.0),
            );
        }

        let com = t.centre_of_mass.expect("a field");
        let anchor = t.anchor().expect("a field has an anchor");

        assert!(
            (100.0..300.0).contains(&com.x),
            "the mean is in the gap, which is the defect: {com:?}"
        );
        assert!(
            t.kernels.iter().any(|k| k.centre == anchor),
            "the anchor is always a place something was observed: {anchor:?}"
        );
        assert!(
            anchor.x < 100.0,
            "and it is the heavier lobe, not the lighter one: {anchor:?}"
        );

        let peak = t.field_peak();
        assert!(
            t.density_at(anchor) >= 0.5 * peak,
            "the ring sits where the field is, not merely on some kernel: {} of {peak}",
            t.density_at(anchor)
        );
        assert!(
            t.density_at(com) < 0.05 * peak,
            "and the point it used to sit at carries {} of a {peak} field",
            t.density_at(com)
        );
    }

    /// The operator's report, reduced to its arithmetic: two sessions working in
    /// entirely different places had their marks land on nearly the same point,
    /// because a mean throws away everything except the first moment and two
    /// different shapes can share one.
    #[test]
    fn two_threads_in_different_places_do_not_share_an_anchor() {
        let now = Instant::now();
        let build = |places: [(f32, f32); 2], dirs: [&str; 2]| {
            let mut t = Territory::for_extent(1000.0);
            for (n, (dir, at)) in dirs.iter().zip(places.iter()).enumerate() {
                for i in 0..5 {
                    t.observe(
                        &obs(&format!("{dir}/f{n}{i}.rs"), ToolKind::Edit, now),
                        1.0,
                        spot(at.0 + step(i, 3.0), at.1),
                    );
                }
            }
            t
        };
        // One thread works east–west across the city, the other north–south.
        // Nothing either touches is within 250 units of anything the other does.
        let a = build([(0.0, 0.0), (400.0, 0.0)], ["src/services", "src/hooks"]);
        let b = build(
            [(200.0, -200.0), (200.0, 200.0)],
            ["src/components", "src/pages"],
        );

        let means_apart = a
            .centre_of_mass
            .expect("a field")
            .distance(b.centre_of_mass.expect("a field"));
        let anchors_apart = a
            .anchor()
            .expect("a field")
            .distance(b.anchor().expect("a field"));

        assert!(
            means_apart < 10.0,
            "the two means land on top of each other — that is the report: {means_apart}"
        );
        assert!(
            anchors_apart > 20.0 * means_apart.max(1.0),
            "and the two anchors do not: {anchors_apart} against {means_apart}"
        );
    }

    /// ADR-0029. Equal densities are the *common* case here — several
    /// observations of the same building carry the same weight at the same
    /// place — so the tie break has to be a total order and not `partial_cmp`.
    #[test]
    fn the_anchor_is_deterministic_under_ties() {
        let now = Instant::now();
        let build = || {
            let mut t = Territory::for_extent(1000.0);
            for (i, at) in [(0.0, 0.0), (300.0, 0.0), (0.0, 300.0)].iter().enumerate() {
                t.observe(
                    &obs(&format!("src/tie/f{i}.rs"), ToolKind::Edit, now),
                    1.0,
                    spot(at.0, at.1),
                );
            }
            t
        };
        let first = build().anchor().expect("a field");
        for _ in 0..8 {
            assert_eq!(
                build().anchor().expect("a field"),
                first,
                "three kernels of exactly equal density must resolve the same way every run"
            );
        }
        // Lowest x, then lowest y — a rule, not an accident of iteration order.
        assert_eq!(first, Point::new(0.0, 0.0));
    }

    /// The trap a stored anchor sets: a memo maintained only from `observe` is
    /// `None` for every hand-built fixture in this workspace, because they all
    /// push straight into the public `kernels`. `anchor()` would then quietly
    /// fall back to the mean and every test written against it would pass while
    /// proving nothing.
    #[test]
    fn a_hand_built_territory_still_answers_with_its_mode() {
        let now = Instant::now();
        let mut t = Territory::for_extent(1000.0);
        for x in [0.0f32, 6.0, 12.0, 400.0] {
            t.kernels.push(Kernel {
                centre: Point::new(x, 0.0),
                weight: 1.0,
                radius: 40.0,
                at: now,
            });
        }
        t.centre_of_mass = Some(Point::new(104.5, 0.0));
        assert_eq!(t.anchor_recomputes, 0, "nothing observed, nothing scanned");
        let anchor = t
            .anchor()
            .expect("kernels pushed by hand still have a mode");
        assert_eq!(
            anchor,
            Point::new(6.0, 0.0),
            "the middle of the tight three, not the hand-set mean"
        );
        assert!(
            t.field_peak() > 2.0,
            "and the peak is computed on demand too: {}",
            t.field_peak()
        );
    }

    /// [`ANCHOR_GLIDE`]'s load-bearing property: the drawn ring reaches the
    /// same place however finely the world is ticked.
    ///
    /// This is what lets the position channel move continuously without
    /// re-introducing the cadence dependence `refresh_anchor` was written to
    /// keep out (PRD §7.4, ADR-0029). The window ticks at 2 Hz, the headless
    /// renderer once per schedule; an exponential relaxation composes exactly
    /// under subdivision, so both settle identically. A linear step would not.
    #[test]
    fn the_drawn_anchor_glides_the_same_way_however_finely_it_is_ticked() {
        // Two lobes far apart. The second decisively overtakes the first, so
        // the target moves once and the drawn point has somewhere to travel.
        let build = || {
            let now = Instant::now();
            let mut t = Territory::for_extent(1000.0);
            for i in 0..8 {
                t.observe(
                    &obs(&format!("src/auth/f{i}.rs"), ToolKind::Edit, now),
                    1.0,
                    spot(step(i, 2.0), 0.0),
                );
            }
            t.decay(now); // seeds `last_decay`, snaps the drawn anchor
            t.decay(now + Duration::from_millis(1));
            for i in 0..40 {
                t.observe(
                    &obs(&format!("tests/unit/g{i}.rs"), ToolKind::Edit, now),
                    1.0,
                    spot(500.0 + step(i, 2.0), 0.0),
                );
            }
            (t, now)
        };

        let (mut coarse, t0) = build();
        let start = coarse.drawn_anchor().expect("a drawn anchor");
        let target = coarse.anchor().expect("a target");
        assert!(
            (target.x - start.x).abs() > 100.0,
            "the target must actually have moved for this test to mean anything:              {start:?} -> {target:?}"
        );

        // One coarse tick of eight seconds...
        coarse.decay(t0 + Duration::from_secs(8));
        let after_coarse = coarse.drawn_anchor().expect("a drawn anchor");

        // ...against eighty fine ticks of a tenth of a second each.
        let (mut fine, _) = build();
        for i in 1..=80 {
            fine.decay(t0 + Duration::from_millis(100 * i));
        }
        let after_fine = fine.drawn_anchor().expect("a drawn anchor");

        assert!(
            (after_coarse.x - after_fine.x).abs() < 0.5
                && (after_coarse.y - after_fine.y).abs() < 0.5,
            "one 8 s tick and eighty 0.1 s ticks must agree: {after_coarse:?} vs {after_fine:?}"
        );

        // And it is a glide, not a snap: one time constant is about 63% of the
        // way, so the ring is still a long way from the target it is chasing.
        let travelled = (after_coarse.x - start.x).abs();
        let distance = (target.x - start.x).abs();
        assert!(
            travelled < distance * 0.8,
            "after one time constant the ring must still be short of the target:              travelled {travelled} of {distance}"
        );
        assert!(
            travelled > distance * 0.4,
            "and it must have moved: travelled {travelled} of {distance}"
        );
    }

    /// PRD §6.3, for the anchor. Two lobes whose densities cross by less than
    /// [`ANCHOR_MARGIN`] must not swap the ring; a decisive overtake must.
    #[test]
    fn the_anchor_holds_through_a_near_tie() {
        let now = Instant::now();
        let mut t = Territory::for_extent(1000.0);
        for i in 0..8 {
            t.observe(
                &obs(&format!("src/auth/f{i}.rs"), ToolKind::Edit, now),
                1.0,
                spot(step(i, 2.0), 0.0),
            );
        }
        let held = t.anchor().expect("a field");
        assert!(held.x < 20.0, "the first lobe has the ring: {held:?}");

        // Nine of them somewhere else: heavier than nothing, and still inside
        // the margin. The ring does not move.
        for i in 0..9 {
            t.observe(
                &obs(&format!("tests/unit/g{i}.rs"), ToolKind::Edit, now),
                1.0,
                spot(500.0 + step(i, 2.0), 0.0),
            );
        }
        assert_eq!(
            t.anchor().expect("a field"),
            held,
            "a near tie is not sustained evidence — one read elsewhere moves nothing"
        );
        assert!(
            t.density_at(held) >= t.field_peak() / ANCHOR_MARGIN,
            "and the bound the margin buys still holds: {} against {}",
            t.density_at(held),
            t.field_peak()
        );

        // Decisively heavier. It moves, and it moves all the way.
        for i in 9..40 {
            t.observe(
                &obs(&format!("tests/unit/g{i}.rs"), ToolKind::Edit, now),
                1.0,
                spot(500.0 + step(i, 2.0), 0.0),
            );
        }
        let moved = t.anchor().expect("a field");
        assert!(
            moved.x > 400.0,
            "a decisive overtake moves the ring: {moved:?}"
        );
    }

    /// What keeps the O(k²) honest against PRD §13.1, and the reason
    /// `refresh_anchor` takes no clock: a uniform rescale cannot move an argmax,
    /// so a tick must not pay for a scan.
    #[test]
    fn the_anchor_scan_is_bounded_by_the_kernel_list_and_not_by_ticks() {
        let mut t = Territory::for_extent(1000.0);
        let mut at = Instant::now();
        for i in 0..5_000u32 {
            t.observe(
                &obs(&format!("src/auth/f{i}.rs"), ToolKind::Edit, at),
                1.0,
                place(i % 32),
            );
            at += Duration::from_millis(2);
        }
        assert!(
            t.anchor_recomputes <= t.observations,
            "one scan per observation that lands on ground, and never more: {} for {}",
            t.anchor_recomputes,
            t.observations
        );

        // A thousand ticks that only decay. Nothing crosses MIN_KERNEL_WEIGHT
        // over one second, so no kernel leaves the list and no scan is due.
        let scans = t.anchor_recomputes;
        let held = t.anchor().expect("a field");
        let peak_before = t.field_peak();
        for _ in 0..1_000 {
            at += Duration::from_millis(1);
            t.decay(at);
        }
        assert_eq!(
            t.anchor_recomputes, scans,
            "a tick that only decays cannot move the anchor, so it must not scan for one"
        );
        assert_eq!(t.anchor().expect("a field"), held, "and it did not move");
        assert!(
            t.field_peak() < peak_before && t.field_peak() > peak_before * 0.9,
            "the peak still followed the decay down: {peak_before} -> {}",
            t.field_peak()
        );
    }

    /// `field_peak` is the peak of the *kernel field*, so it has to track the
    /// two uniform rescales `decay` applies — the half-life factor and
    /// `Territory::rest`'s lift — without a rescan, or `rest` would read a
    /// stale number on every tick.
    #[test]
    fn the_memoised_peak_agrees_with_a_fresh_scan_after_decay_and_rest() {
        let t0 = Instant::now();
        let mut t = Territory::for_extent(1000.0);
        for i in 0..8 {
            t.observe(
                &obs(&format!("src/auth/f{i}.rs"), ToolKind::Edit, t0),
                1.0,
                place(i),
            );
        }
        for n in 1..=12u32 {
            t.decay(t0 + DECAY_HALF_LIFE * n);
            let fresh = t
                .kernels
                .iter()
                .map(|k| t.density_at(k.centre))
                .fold(0.0f32, f32::max);
            let memo = t.field_peak();
            assert!(
                (memo - fresh).abs() <= fresh.abs().mul_add(1e-3, 1e-6),
                "half-life {n}: memo {memo}, fresh {fresh}"
            );
        }
    }
}
