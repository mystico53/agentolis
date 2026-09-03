//! Layers 3–5 — the live layer, drawn on the CPU (PRD §10, §11, §12, §13).
//!
//! This module owns the **notation**: every colour, every shape and every fade
//! rule for clouds, agents and attention marks. [`crate::plan`] owns layers 0–2
//! and the type sub-band; it imports its stand-in palette from here rather than
//! keeping a second copy, because a visual language with two definitions has
//! none.
//!
//! # The one rule PRD §10 opens with
//!
//! > **Shape encodes what, colour encodes how it went. Never conflate them.**
//!
//! So [`Mark`] carries a [`Glyph`] and an [`Outcome`] as two separate fields
//! that are read by two separate functions ([`draw_glyph`] and
//! [`outcome_ink`]), and neither one can see the other. A "failed edit" is the
//! barred circle in red; a "failed run" is the triangle in the *same* red. The
//! operator learns six shapes once and three colours once instead of eighteen
//! badges.
//!
//! # Fading happens *inside* the band, never toward the map
//!
//! This is the rule that makes PRD §10.3's budget survive contact with an
//! animation, and it is not obvious.
//!
//! The natural way to fade a mark is to drop its alpha. Over Polis's near-black
//! base map that is wrong: a trail at `α = 0.55` in `AGENT_TRAIL` composites to
//! channel 69, which is **in the cloud band**, and a two-minute-old trail step
//! ends up dimmer than a district label. The layering claim — live activity is
//! always brighter than the city under it — would then be true only for the
//! newest marks.
//!
//! So the live layer draws **opaque ink and fades by tone**: [`fade`] walks a
//! colour down toward its own band's floor and stops there. A mark that has
//! aged out completely is *removed*, not dimmed into the map. "Dynamic range,
//! not omission" (§10.3) applied inside a band rather than across the image:
//!
//! * agents fade `168 → 97` and then disappear;
//! * attention fades `255 → 169` and then disappears;
//! * cloud marks are opaque at their iso tone, as [`crate::plan`] settled.
//!
//! Antialiased *edges* still blend, and they may land anywhere below the mark.
//! That is coverage, not colour: it softens a silhouette and can never lift a
//! map pixel above its ceiling.
//!
//! # Peripheral perception (PRD §11.4)
//!
//! > Peripheral vision is poor at colour and good at motion onset.
//!
//! Every arrival — an operation mark and an attention mark alike — carries a
//! `pulse` in `[0, 1]` that the caller computes from a ≤400 ms window. It is
//! drawn as an **expanding ring**, so the thing that catches the eye is a
//! change in geometry rather than a change in hue. Steady state is shape and
//! position. No state anywhere in this module is signalled by colour alone:
//! needs-decision is a *pin*, done is a *ring*, contention is a *link*, and
//! severity is carried by stroke weight and dash before it is carried by red.
//!
//! # Nothing here reads a clock or a transcendental
//!
//! Ages and pulses arrive as numbers on [`LiveFrame`]; positions arrive already
//! interpolated. The drawing is a pure function of that struct, so a frame is
//! reproducible from a snapshot and a time, which is what makes
//! [`crate::frame`] able to record a session twice and get the same bytes. The
//! easing is polynomial ([`smoothstep`]) and the circle is a constant table, for
//! the same reason [`crate::plan`] avoids `sin` and `powf`.

// The same five lint families that fire on every line of `plan`'s geometry fire
// on every line of this one, and for the same reasons; see that module's note.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::float_cmp,
    clippy::many_single_char_names,
    clippy::similar_names,
    clippy::too_many_lines
)]

use std::time::{Duration, Instant};

use polis_events::{Glyph, Outcome};
use polis_world::contention::Severity;

use crate::raster::{Canvas, Px, Rgb};

// ---------------------------------------------------------------------------
// Layer 3 — clouds. The notation was settled by `plan::render_band_validation`
// on real pixels; the constants live here because the live path and the
// validation image must not be able to disagree about them.
// ---------------------------------------------------------------------------

/// Cloud iso-band tones (PRD §10.4, layer 3), fringe → body → core, inside
/// [`crate::plan::CLOUD_BAND`].
///
/// These are the tones of the **marks** — contour strokes and hatch strokes —
/// not of a wash. Each is opaque where it is drawn and absent everywhere else,
/// which is what lets a cloud own channels 49–84 without lifting the base map
/// underneath it by a single level.
pub const CLOUD_TONES: [Rgb; 3] = [[56, 64, 74], [64, 72, 80], [74, 80, 84]];

/// The axis the cloud hatch runs along: across the sun, and therefore across
/// [`crate::plan`]'s industrial hatch.
///
/// Two textures at the same angle are one texture; at right angles they are
/// two, and the map already needs to say "vendored tree" and "somebody's
/// territory" in the same square inch. Asserted equal to `plan`'s `RIDGE` in
/// that module's tests.
pub const CLOUD_HATCH: [f64; 2] = [-0.8, 0.6];

/// Hatch spacing per iso band, fringe → core, in **output** pixels.
///
/// Spacing is the encoding: PRD §10.4 wants the reader able to say "that file is
/// in the core of this thread's work" versus "it's at the fringe", and a
/// tightening hatch says it the way a contour map says altitude. One stroke in
/// 14, in 9, in 6 keeps the median of the base map underneath a cloud exactly
/// where it was.
pub const CLOUD_HATCH_SPACING: [f64; 3] = [14.0, 9.0, 6.0];

/// Hatch stroke width per iso band, in output pixels.
pub const CLOUD_HATCH_WIDTH: [f64; 3] = [1.0, 1.25, 1.5];

/// Contour stroke width per iso band, in output pixels, measured inward from
/// the boundary.
///
/// The **outer** contour is the boldest, which is the opposite of the hatch and
/// deliberate: the fringe boundary is the cloud's silhouette, and a silhouette
/// is the only part of any mark that survives being looked at from across the
/// room (PRD §1).
pub const CLOUD_CONTOUR_WIDTH: [f64; 3] = [3.0, 2.0, 2.0];

/// The iso thresholds, in **kernels overlapping here** (ADR-0020).
///
/// Absolute, never normalised against the observed field maximum: normalising
/// collapses every ordinary territory into a single fringe band, which is the
/// mush §10.4 forbids. The fringe threshold is above one kernel on purpose —
/// two overlapping kernels is the cheapest honest definition of "this is a
/// region, not a point".
pub const CLOUD_ISO: [f64; 3] = [1.3, 2.8, 5.4];

/// The band index meaning "outside the fringe" in a per-pixel band map.
pub const NO_BAND: u8 = u8::MAX;

// ---------------------------------------------------------------------------
// Layer 4 — agents. Every entry's largest channel is inside
// `plan::AGENT_BAND` (97–168), asserted in this module's tests.
// ---------------------------------------------------------------------------

/// An operation still in flight — PRD §10.2's neutral.
///
/// This is the *common* case, not an edge case: `toolUseResult` is missing on
/// 65 % of subagent tool results, so most marks are born neutral and a good
/// many die that way.
pub const AGENT_PENDING: Rgb = [132, 140, 156];
/// An operation that succeeded — PRD §10.2's teal.
pub const AGENT_DONE: Rgb = [96, 168, 150];
/// An operation that failed, was rejected, or was aborted — PRD §10.2's red.
pub const AGENT_FAILED: Rgb = [168, 98, 92];
/// The tether tying a worker to its thread (PRD §5: the thread is the unit the
/// operator thinks in).
pub const AGENT_TETHER: Rgb = [100, 110, 130];
/// A thread's trail (PRD §12), at its freshest.
pub const AGENT_TRAIL: Rgb = [110, 122, 144];
/// The agent itself — the moving body, not the mark it left. The brightest
/// neutral in the band, because "where is it *now*" outranks "where has it
/// been".
pub const AGENT_BODY: Rgb = [150, 158, 168];
/// A thread's anchor: the orchestrator's own position, where its tethers meet.
pub const AGENT_ANCHOR: Rgb = [138, 146, 162];
/// Scaffolding on a file under edit (PRD §8). Olive rather than neutral so it
/// reads as *material* rather than as another agent, and it is drawn as an open
/// frame so it reads as impermanent.
pub const AGENT_SCAFFOLD: Rgb = [140, 148, 112];

// ---------------------------------------------------------------------------
// Layer 5 — attention. Owns the top of the range; nothing else may enter it.
// ---------------------------------------------------------------------------

/// Needs-decision (PRD §11.2a) — amber, persistent, drawn as a standing pin.
/// The primary state: it is what the product is for.
pub const ATTN_DECISION: Rgb = [255, 188, 62];
/// Done (PRD §11.2b) — teal, decaying. Main agents only.
pub const ATTN_DONE: Rgb = [118, 236, 206];
/// Contention (PRD §11.2c) — red. A relation between two threads, drawn as a
/// link, never a badge on a dot.
pub const ATTN_CONTENTION: Rgb = [255, 92, 78];

/// The floor an agent-layer mark fades to. It never fades below its band.
pub const AGENT_FLOOR: u8 = crate::plan::AGENT_BAND.0;
/// The floor an attention mark fades to.
pub const ATTENTION_FLOOR: u8 = crate::plan::ATTENTION_BAND.0;

/// How long an arrival pulse lasts, in seconds (PRD §11.4: "≤400 ms").
pub const PULSE_SECS: f64 = 0.4;

// ---------------------------------------------------------------------------
// Geometry constants. No trigonometry reaches the image (PRD §7.4).
// ---------------------------------------------------------------------------

/// A 16-gon on the unit circle, from a constant table.
pub(crate) const CIRCLE: [[f64; 2]; 16] = [
    [1.000, 0.000],
    [0.924, 0.383],
    [0.707, 0.707],
    [0.383, 0.924],
    [0.000, 1.000],
    [-0.383, 0.924],
    [-0.707, 0.707],
    [-0.924, 0.383],
    [-1.000, 0.000],
    [-0.924, -0.383],
    [-0.707, -0.707],
    [-0.383, -0.924],
    [0.000, -1.000],
    [0.383, -0.924],
    [0.707, -0.707],
    [0.924, -0.383],
];

/// The narrowest a live stroke may be, in output pixels.
///
/// Two pixels, and the reason is the band claim rather than taste. The
/// rasteriser antialiases by **coverage**, so a stroke thinner than two pixels
/// can land with every one of its pixels partially covered — and a partially
/// covered agent mark composites *down*, out of the agent band and into the
/// map's. At two pixels a stroke always has at least one fully covered pixel
/// whatever its subpixel offset, so the mark always has ink where the band
/// scheme says it should. Measured: dropping this to 1.0 put 54 % of the live
/// layer's own pixels back inside the base map's band.
const MIN_STROKE: f64 = 2.0;

/// Three directions 120° apart: the satellites of PRD §10.1's delegate glyph.
const SATELLITES: [[f64; 2]; 3] = [[0.0, -1.0], [0.866, 0.5], [-0.866, 0.5]];

// ---------------------------------------------------------------------------
// Ink
// ---------------------------------------------------------------------------

/// PRD §10.2's colour channel, and the only function that reads an [`Outcome`].
///
/// Kept separate from [`draw_glyph`] so the two channels cannot be conflated by
/// accident — which is the failure §10 opens by forbidding.
#[must_use]
pub fn outcome_ink(outcome: Outcome) -> Rgb {
    match outcome {
        Outcome::Pending => AGENT_PENDING,
        Outcome::Done => AGENT_DONE,
        Outcome::Failed => AGENT_FAILED,
    }
}

/// Fade a live colour toward its band's floor, never past it.
///
/// `t = 1` is the colour as given; `t = 0` is the same hue with its largest
/// channel sitting exactly on `floor`. See the module docs for why this is a
/// tone ramp and not an alpha ramp.
#[must_use]
pub fn fade(colour: Rgb, floor: u8, t: f64) -> Rgb {
    let m = f64::from(colour.iter().copied().max().unwrap_or(0));
    if m <= 0.0 {
        return colour;
    }
    let floor = f64::from(floor).min(m);
    let target = (m - floor).mul_add(t.clamp(0.0, 1.0), floor);
    let k = target / m;
    [
        (f64::from(colour[0]) * k).round().clamp(0.0, 255.0) as u8,
        (f64::from(colour[1]) * k).round().clamp(0.0, 255.0) as u8,
        (f64::from(colour[2]) * k).round().clamp(0.0, 255.0) as u8,
    ]
}

/// The classic cubic ease. Polynomial, so the bytes are the same on every libm.
#[must_use]
pub fn smoothstep(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    t * t * 2.0f64.mul_add(-t, 3.0)
}

// ---------------------------------------------------------------------------
// The frame
// ---------------------------------------------------------------------------

/// Which trail notation to draw — PRD §17's open question 3, as a switch.
///
/// > Does the trail need an explicit time encoding (dash density, opacity
/// > ramp), or is fade sufficient?
///
/// Both are implemented so the question can be answered with two images of the
/// same frame rather than with an opinion. See [`draw_trail`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrailStyle {
    /// Fade alone: one continuous stroke, tone ramped by age, constant width.
    Fade,
    /// Explicit time encoding: the stroke breaks into dashes whose duty cycle
    /// falls with age, its width tapers, and every stop carries a bead.
    #[default]
    Timed,
}

/// One Gaussian-ish kernel of a territory, already projected to pixels.
#[derive(Debug, Clone, Copy)]
pub struct CloudKernel {
    /// Centre, in device pixels.
    pub at: Px,
    /// Reach, in device pixels.
    pub radius: f64,
    /// Weight after PRD §6.3's decay.
    pub weight: f64,
}

/// One stop on a thread's trail (PRD §12).
#[derive(Debug, Clone, Copy)]
pub struct TrailStep {
    /// Where.
    pub at: Px,
    /// Age in seconds. Drives the fade and, in [`TrailStyle::Timed`], the dash
    /// duty cycle.
    pub age: f64,
    /// How many times this thread has touched this path — PRD §12's thrashing
    /// signal, and **not** truncated by the trail cap.
    pub visits: u32,
}

/// A thread's trail: history without a timeline scrubber.
#[derive(Debug, Clone, Default)]
pub struct Trail {
    /// Stops, oldest first.
    pub steps: Vec<TrailStep>,
    /// How long a step survives, in seconds. Ages are divided by this.
    pub ttl: f64,
}

/// A worker tied back to its thread, so the thread reads as one unit.
#[derive(Debug, Clone, Copy)]
pub struct Tether {
    /// The thread's anchor.
    pub anchor: Px,
    /// The worker.
    pub worker: Px,
    /// Whether the worker is still running. A finished worker's tether is drawn
    /// dimmer and thinner rather than dropped, so a thread does not appear to
    /// shed limbs.
    pub running: bool,
    /// How far, and which way, this tether bows off the straight line, in
    /// `[-1, 1]`.
    ///
    /// Workers of one thread are usually working in **one place**, so their
    /// tethers share both endpoints and stack into a single opaque ribbon that
    /// is the loudest thing on the map and says only "this thread delegates".
    /// Fanning them apart turns that ribbon back into a countable number of
    /// hands, which is the thing worth knowing.
    pub spread: f64,
}

/// A building this thread keeps coming back to (PRD §12).
///
/// > you can see backtracking, thrashing (the same building revisited six
/// > times), and scope creep in it
///
/// A fading polyline cannot show this: six passes over one building draw six
/// coincident strokes and look exactly like one. So the count gets a mark of
/// its own — a rosette of radial ticks, one per visit — which is a *shape*
/// channel and therefore survives the box filter down to thumbnail size.
#[derive(Debug, Clone, Copy)]
pub struct Thrash {
    /// The building.
    pub at: Px,
    /// Total touches.
    pub visits: u32,
    /// Age of the most recent touch, normalised to `[0, 1]`.
    pub age: f64,
}

/// A file currently under edit (PRD §8's scaffolding).
///
/// # Why the live height delta is scaffolding and not a taller building
///
/// PRD §7.3 makes building height uncommitted diff lines, and PRD §13 asks for
/// height to be tweened between updates. Redrawing the massing every frame
/// would mean redrawing the base map every frame, which PRD §13 forbids in the
/// same breath ("redraw the base only on layout change") — and the massing is
/// layer 2, so a growing building could not be brighter than the agent working
/// on it anyway.
///
/// So the base map keeps the height the layout gave it, and the *delta* being
/// worked on right now is drawn as an open frame rising off the roof, in the
/// agent band, tweened. It reads as impermanent because it is: the scaffolding
/// comes down and the building keeps the height at the next layout step.
#[derive(Debug, Clone, Copy)]
pub struct Scaffold {
    /// The building.
    pub at: Px,
    /// Half-width of the frame, in pixels.
    pub half_width: f64,
    /// Rise, in pixels — the tweened diff size.
    pub rise: f64,
    /// Age of the last touch, normalised to `[0, 1]`.
    pub age: f64,
}

/// One operation mark: PRD §10.1's shape and §10.2's colour, side by side and
/// never conflated.
///
/// Two further fields, and neither is a third *state* channel — both are size,
/// which is what a proportional-symbol map has always used for "how much" and
/// "how sure":
///
/// * [`Mark::scale`] is how certain the **position** is. A mark at a building
///   is drawn full size; one placed at a district, or at the agent that ran it,
///   is drawn smaller because that is a weaker claim about where it happened.
/// * [`Mark::count`] is how many operations the mark stands for, after
///   identical ones at one position were aggregated.
#[derive(Debug, Clone, Copy)]
pub struct Mark {
    /// Where.
    pub at: Px,
    /// Shape — what the operation *was*.
    pub glyph: Glyph,
    /// Colour — how it *went*.
    pub outcome: Outcome,
    /// Age normalised to `[0, 1]`, where 1 is "about to be dropped".
    pub age: f64,
    /// Arrival pulse in `[0, 1]`, falling to zero over [`PULSE_SECS`].
    pub pulse: f64,
    /// Positional certainty as a factor on the glyph radius —
    /// `polis_world::OpSite::scale`. `1.0` is "this building".
    pub scale: f64,
    /// How many operations this mark stands for. `1` is a single call.
    ///
    /// Aggregation is how volume is kept off the map without losing anything:
    /// 6 742 shell calls in one session cannot each be a glyph, but "forty runs
    /// here, and they failed" is one legible mark. Only operations that agree on
    /// position, shape **and** outcome are ever merged, so a failure can never
    /// be absorbed into a success.
    pub count: u32,
}

impl Mark {
    /// A single, un-aggregated mark at a building — the plain case, and the one
    /// tests want to write.
    #[must_use]
    pub fn single(at: Px, glyph: Glyph, outcome: Outcome, age: f64, pulse: f64) -> Self {
        Self {
            at,
            glyph,
            outcome,
            age,
            pulse,
            scale: 1.0,
            count: 1,
        }
    }
}

/// How much bigger a mark gets for standing in for several operations.
///
/// Stepped rather than continuous, and no logarithm: `live` keeps
/// transcendentals out of the image so a frame is byte-reproducible (see the
/// module docs). Five steps is as much as the eye reads off a glyph anyway.
#[must_use]
pub fn stack_scale(count: u32) -> f64 {
    match count {
        0 | 1 => 1.0,
        2..=3 => 1.12,
        4..=7 => 1.24,
        8..=15 => 1.36,
        _ => 1.50,
    }
}

/// What an [`Agent`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Body {
    /// A main agent. It delegates rather than edits, so it is drawn at its
    /// territory's centre of mass and never at a file.
    Main,
    /// A worker.
    Worker,
}

/// A moving agent — the thing the operator is actually watching.
#[derive(Debug, Clone, Copy)]
pub struct Agent {
    /// Interpolated position this frame.
    pub at: Px,
    /// Main agent or worker.
    pub body: Body,
    /// How its last operation went. The agent's own colour, so a failing worker
    /// is red *while it moves*, not only where it stopped.
    pub outcome: Outcome,
    /// `1.0` at the start of a move, `0.0` on arrival. Drives the motion
    /// streak, which is the second half of PRD §11.4's motion-onset argument:
    /// travel has to be visible in the periphery too.
    pub travel: f64,
    /// Unit direction of travel, or `[0, 0]` when parked.
    pub heading: [f64; 2],
    /// Whether the thread this agent belongs to is waiting on a human.
    pub waiting: bool,
}

/// Which of PRD §11.2's three states a mark is.
///
/// Four variants for three states because *done, unverified* is really "needs
/// review" and persists — PRD §17's open question 2. Splitting it here costs
/// nothing and lets the renderer give it a different **shape**, which is what
/// stops the split from being a colour-only distinction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkKind {
    /// (a) Needs decision — a standing pin, amber, persistent.
    NeedsDecision,
    /// (b) Done, verified — a single ring, teal, decaying.
    DoneVerified,
    /// (b′) Done, unverified — a *double* ring. Persists.
    DoneUnverified,
    /// (c) Contention — a link joining two threads across the map.
    Contention,
}

/// One attention mark (PRD §11.2), owning the top of the contrast range.
#[derive(Debug, Clone, Copy)]
pub struct AttentionMark {
    /// Which state.
    pub kind: MarkKind,
    /// Where. For contention this is one of the two threads.
    pub at: Px,
    /// The other end of a contention link.
    pub other: Option<Px>,
    /// Severity, for contention only. Carried by stroke weight and dash, so it
    /// is not a colour-only distinction.
    pub severity: Option<Severity>,
    /// Arrival pulse in `[0, 1]`.
    pub pulse: f64,
    /// Steady-state prominence in `[0, 1]`.
    pub weight: f64,
}

/// Everything the live layer draws in one frame: already interpolated, already
/// projected into device pixels.
///
/// The renderer is a pure function of this struct. That is deliberate — it is
/// what lets [`crate::frame`] build a frame from a world snapshot and a time,
/// hand it here, and get the same bytes twice.
#[derive(Debug, Clone, Default)]
pub struct LiveFrame {
    /// The map unit — the median block's diameter in pixels. Every size in this
    /// module is a fraction of it, so the notation reads the same at 90 files
    /// and at 5 000.
    pub unit: f64,
    /// Height of the map area in pixels. Nothing is drawn below it.
    pub map_height: f64,
    /// Territory kernels, summed into one field: PRD §6.4's "overlap is field
    /// addition", which is also the early-warning contention signal.
    pub clouds: Vec<CloudKernel>,
    /// One trail per thread.
    pub trails: Vec<Trail>,
    /// Worker-to-thread tethers.
    pub tethers: Vec<Tether>,
    /// Revisit rosettes.
    pub thrash: Vec<Thrash>,
    /// Scaffolding on files under edit.
    pub scaffolds: Vec<Scaffold>,
    /// Operation marks.
    pub marks: Vec<Mark>,
    /// The agents themselves.
    pub agents: Vec<Agent>,
    /// Attention marks.
    pub attention: Vec<AttentionMark>,
    /// PRD §6.2's status rail: what has no place on the map. Chrome, drawn
    /// outside the map frame, and **never empty when something was dropped**.
    pub rail: Vec<RailRow>,
}

/// What a [`RailRow`] is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RailKind {
    /// A thread with no converged territory and no placeable step.
    ///
    /// > Until then the thread renders with no cloud — an unplaced marker in
    /// > the status rail. (PRD §6.2)
    UnplacedThread,
    /// Operations of a thread that has no position, so they have none either.
    /// The count is what stops them being silently dropped.
    UnplacedOps,
    /// Workers Polis cannot attach to any thread — uncertainty shown rather
    /// than guessed around.
    UnattributedWorkers,
}

impl RailKind {
    /// The word the rail prints for this kind.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::UnplacedThread => "NO TERRITORY",
            Self::UnplacedOps => "UNPLACED OPS",
            Self::UnattributedWorkers => "UNATTRIBUTED",
        }
    }
}

/// One row of the status rail.
#[derive(Debug, Clone)]
pub struct RailRow {
    /// Which kind of unplaced thing.
    pub kind: RailKind,
    /// A short name — a thread title, or a reason.
    pub label: String,
    /// How many things this row stands for.
    pub count: u32,
    /// How many of them failed. The reason the rail is worth a glance.
    pub failed: u32,
}

/// What each layer cost, measured. PRD §13.1 budgets layers 4 + 5 at under 4 ms.
#[derive(Debug, Clone, Copy, Default)]
pub struct LiveTimings {
    /// Layer 3.
    pub clouds: Duration,
    /// Layer 4 — agents, trails, tethers, scaffolding.
    pub agents: Duration,
    /// Layer 5 — attention.
    pub attention: Duration,
}

impl LiveTimings {
    /// The number PRD §13.1 budgets: the agent **and** attention layers.
    /// Clouds are excluded because §13.1 names those two.
    #[must_use]
    pub fn budgeted(&self) -> Duration {
        self.agents + self.attention
    }

    /// Everything the live layer cost.
    #[must_use]
    pub fn total(&self) -> Duration {
        self.clouds + self.agents + self.attention
    }
}

// ---------------------------------------------------------------------------
// The draw
// ---------------------------------------------------------------------------

/// Draws layers 3, 4 and 5 in order.
///
/// Callers that draw in-map labels (PRD §10.3 puts them **above** the clouds)
/// call [`draw_clouds`], then their labels, then [`draw_agents`] and
/// [`draw_attention`]. This is the convenience path for everyone else.
pub fn draw(canvas: &mut Canvas, frame: &LiveFrame, style: TrailStyle) -> LiveTimings {
    let clouds = draw_clouds(canvas, frame);
    let agents = draw_agents(canvas, frame, style);
    let attention = draw_attention(canvas, frame);
    LiveTimings {
        clouds,
        agents,
        attention,
    }
}

/// Layer 3 — territory clouds, as iso-contours and hatch (PRD §10.4).
///
/// The field is only evaluated inside the kernels' own bounds. That is the one
/// optimisation that matters here: a territory typically covers a fifth of the
/// map, and evaluating the other four fifths every frame to discover they are
/// empty is most of the cost of a naive implementation.
pub fn draw_clouds(canvas: &mut Canvas, frame: &LiveFrame) -> Duration {
    let start = Instant::now();
    if frame.clouds.is_empty() {
        return start.elapsed();
    }
    let rows = (frame.map_height as usize).min(canvas.height);
    if rows == 0 || canvas.width == 0 {
        return start.elapsed();
    }
    let Some(bands) = band_map(&frame.clouds, canvas.width, rows) else {
        return start.elapsed();
    };
    paint_cloud_bands(canvas, &bands);
    start.elapsed()
}

/// A per-pixel iso-band index over a rectangle of the canvas.
#[derive(Debug, Clone)]
pub struct BandMap {
    /// Left edge in canvas pixels.
    pub x0: usize,
    /// Top edge in canvas pixels.
    pub y0: usize,
    /// Width in pixels.
    pub width: usize,
    /// Height in pixels.
    pub height: usize,
    /// Band index per pixel, or [`NO_BAND`].
    pub cells: Vec<u8>,
}

impl BandMap {
    /// The band at a canvas pixel, or [`NO_BAND`] outside the rectangle.
    #[must_use]
    pub fn at(&self, x: usize, y: usize) -> u8 {
        if x < self.x0 || y < self.y0 {
            return NO_BAND;
        }
        let (dx, dy) = (x - self.x0, y - self.y0);
        if dx >= self.width || dy >= self.height {
            return NO_BAND;
        }
        self.cells[dy * self.width + dx]
    }
}

/// The density field of a set of kernels, thresholded into [`CLOUD_ISO`]'s
/// bands, over the kernels' bounding rectangle clipped to the canvas.
///
/// Returns `None` when the kernels fall entirely outside the canvas.
#[must_use]
pub fn band_map(kernels: &[CloudKernel], width: usize, rows: usize) -> Option<BandMap> {
    let mut lo = [f64::INFINITY; 2];
    let mut hi = [f64::NEG_INFINITY; 2];
    for k in kernels {
        if k.weight <= 0.0 || k.radius <= 0.0 {
            continue;
        }
        lo[0] = lo[0].min(k.at[0] - k.radius);
        lo[1] = lo[1].min(k.at[1] - k.radius);
        hi[0] = hi[0].max(k.at[0] + k.radius);
        hi[1] = hi[1].max(k.at[1] + k.radius);
    }
    if !lo[0].is_finite() {
        return None;
    }
    let x0 = lo[0].floor().max(0.0) as usize;
    let y0 = lo[1].floor().max(0.0) as usize;
    let x1 = (hi[0].ceil().max(0.0) as usize).min(width);
    let y1 = (hi[1].ceil().max(0.0) as usize).min(rows);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    let (w, h) = (x1 - x0, y1 - y0);

    // The field is evaluated on a coarse lattice and bilinearly resampled, which
    // is PRD §10.4's offscreen texture at a size that fits the region rather
    // than the screen. A contour then comes out as a smooth curve instead of a
    // staircase of lattice cells.
    let grid_x = (w / 3).clamp(2, 256);
    let grid_y = (h / 3).clamp(2, 256);
    let mut field = vec![0.0f64; grid_x * grid_y];
    let sx = w as f64 / grid_x as f64;
    let sy = h as f64 / grid_y as f64;
    for k in kernels {
        if k.weight <= 0.0 || k.radius <= 0.0 {
            continue;
        }
        let gx0 = (((k.at[0] - k.radius - x0 as f64) / sx).floor().max(0.0) as usize).min(grid_x);
        let gx1 =
            ((((k.at[0] + k.radius - x0 as f64) / sx).ceil().max(0.0) as usize) + 1).min(grid_x);
        let gy0 = (((k.at[1] - k.radius - y0 as f64) / sy).floor().max(0.0) as usize).min(grid_y);
        let gy1 =
            ((((k.at[1] + k.radius - y0 as f64) / sy).ceil().max(0.0) as usize) + 1).min(grid_y);
        for gy in gy0..gy1 {
            let py = (gy as f64 + 0.5).mul_add(sy, y0 as f64);
            let dy = (py - k.at[1]) / k.radius;
            for gx in gx0..gx1 {
                let px = (gx as f64 + 0.5).mul_add(sx, x0 as f64);
                let dx = (px - k.at[0]) / k.radius;
                field[gy * grid_x + gx] += k.weight * kernel(dx.mul_add(dx, dy * dy));
            }
        }
    }

    let mut cells = vec![NO_BAND; w * h];
    let lx = (grid_x - 1) as f64;
    let ly = (grid_y - 1) as f64;
    for y in 0..h {
        let fy = (((y as f64 + 0.5) / sy) - 0.5).clamp(0.0, ly);
        let r0 = fy.floor();
        let ty = fy - r0;
        let (r0, r1) = (r0 as usize, (r0 as usize + 1).min(grid_y - 1));
        for x in 0..w {
            let fx = (((x as f64 + 0.5) / sx) - 0.5).clamp(0.0, lx);
            let c0 = fx.floor();
            let tx = fx - c0;
            let (c0, c1) = (c0 as usize, (c0 as usize + 1).min(grid_x - 1));
            let a = field[r0 * grid_x + c0];
            let b = field[r0 * grid_x + c1];
            let c = field[r1 * grid_x + c0];
            let d = field[r1 * grid_x + c1];
            let top = (b - a).mul_add(tx, a);
            let bot = (d - c).mul_add(tx, c);
            if let Some(band) = iso_band((bot - top).mul_add(ty, top)) {
                cells[y * w + x] = band as u8;
            }
        }
    }
    Some(BandMap {
        x0,
        y0,
        width: w,
        height: h,
        cells,
    })
}

/// Paints a band map as nested contours plus a hatch that tightens toward the
/// core.
///
/// Nothing here is a fill, so the base map under a cloud is not lifted — it is
/// left alone and shows through between the strokes. The contour is found by
/// comparing a pixel's band with its neighbours a stroke-width away, which gives
/// a closed curve of the right thickness for free and cannot leak: a band
/// boundary is a boundary in the array.
pub fn paint_cloud_bands(canvas: &mut Canvas, bands: &BandMap) {
    let step: [isize; 3] = [
        CLOUD_CONTOUR_WIDTH[0].round().max(1.0) as isize,
        CLOUD_CONTOUR_WIDTH[1].round().max(1.0) as isize,
        CLOUD_CONTOUR_WIDTH[2].round().max(1.0) as isize,
    ];
    for y in 0..bands.height {
        for x in 0..bands.width {
            let band = bands.cells[y * bands.width + x];
            if band == NO_BAND {
                continue;
            }
            let neighbour = |dx: isize, dy: isize| -> u8 {
                let nx = x as isize + dx;
                let ny = y as isize + dy;
                if nx < 0 || ny < 0 || nx >= bands.width as isize || ny >= bands.height as isize {
                    return NO_BAND;
                }
                bands.cells[ny as usize * bands.width + nx as usize]
            };
            let s = step[band as usize];
            let contour = neighbour(s, 0) != band
                || neighbour(-s, 0) != band
                || neighbour(0, s) != band
                || neighbour(0, -s) != band;
            let cx = x + bands.x0;
            let cy = y + bands.y0;
            let hatched = !contour && {
                let spacing = CLOUD_HATCH_SPACING[band as usize];
                let a = CLOUD_HATCH[0].mul_add(cx as f64, CLOUD_HATCH[1] * cy as f64);
                a.rem_euclid(spacing) < CLOUD_HATCH_WIDTH[band as usize]
            };
            if contour || hatched {
                cloud_pixel(canvas, cx, cy, CLOUD_TONES[band as usize]);
            }
        }
    }
}

/// Layer 4 — trails, tethers, thrash rosettes, scaffolding, marks, agents.
///
/// Drawn in that order on purpose: history under structure under state under
/// the thing that is moving. The agent body is last because it is the only
/// element the operator tracks continuously, and a mark drawn over it would
/// read as a collision rather than as an overlap.
pub fn draw_agents(canvas: &mut Canvas, frame: &LiveFrame, style: TrailStyle) -> Duration {
    let start = Instant::now();
    let r = glyph_radius(frame.unit, frame.map_height);
    for trail in &frame.trails {
        draw_trail(canvas, trail, style, r);
    }
    for tether in &frame.tethers {
        draw_tether(canvas, *tether, r);
    }
    for t in &frame.thrash {
        draw_thrash(canvas, *t, r);
    }
    for s in &frame.scaffolds {
        draw_scaffold(canvas, *s, r);
    }
    for m in &frame.marks {
        draw_mark(canvas, *m, r);
    }
    for a in &frame.agents {
        draw_agent(canvas, *a, r);
    }
    start.elapsed()
}

/// Layer 5 — the three attention states (PRD §11.2).
pub fn draw_attention(canvas: &mut Canvas, frame: &LiveFrame) -> Duration {
    let start = Instant::now();
    let r = glyph_radius(frame.unit, frame.map_height);
    // Links first: contention is a relation, and a relation drawn over the two
    // things it relates hides them.
    for m in &frame.attention {
        if m.kind == MarkKind::Contention {
            draw_contention(canvas, *m, r);
        }
    }
    for m in &frame.attention {
        match m.kind {
            MarkKind::NeedsDecision => draw_pin(canvas, *m, r),
            MarkKind::DoneVerified | MarkKind::DoneUnverified => draw_done(canvas, *m, r),
            MarkKind::Contention => {}
        }
    }
    start.elapsed()
}

/// The radius every glyph is built from.
///
/// A fraction of the map unit, so the notation reads the same at 90 files and at
/// 5 000 — but with a floor and a ceiling taken from the **canvas**, not from
/// the city.
///
/// The floor is the one that had to be learned from a rendered frame. On a
/// repository whose block graph is dominated by an industrial mass, the median
/// block is a few pixels across and `unit * 0.26` comes out at two or three
/// pixels — at which size a filled triangle and a filled square are the same
/// four-pixel smudge, and PRD §10.1's whole shape channel is gone. Six pixels at
/// a 900-pixel map is about where a triangle stops being a square, so the floor
/// is stated as a fraction of the map and follows the output size.
#[must_use]
pub fn glyph_radius(unit: f64, map_height: f64) -> f64 {
    let floor = (map_height / 150.0).clamp(3.0, 9.0);
    let ceiling = (map_height / 34.0).max(floor + 1.0);
    (unit * 0.26).clamp(floor, ceiling)
}

// ---------------------------------------------------------------------------
// Trails — PRD §12, and PRD §17's open question 3
// ---------------------------------------------------------------------------

/// The dash period of [`TrailStyle::Timed`], as a multiple of the glyph radius.
const DASH_PERIOD: f64 = 1.35;

/// The most dashes one trail segment may be cut into.
const MAX_DASHES: f64 = 18.0;

/// How far the *n*-th traversal of one leg bows off the straight line, as a
/// multiple of the glyph radius.
///
/// This is the fix for the one thing PRD §12 asks the trail for and the trail
/// was not delivering. A thread that goes `auth.rs → tests.rs → auth.rs` pushes
/// three steps and draws two segments — and the two segments are **the same
/// two points in the opposite order**, so the return leg lands exactly on top
/// of the outbound one and six passes over a building look identical to one.
/// Watching the M2 recordings you saw marks accumulate and never saw an agent
/// come back.
///
/// So each repeat of a leg bows further off the line, alternating sides, and a
/// round trip draws as a **lens** rather than as an overstroke: the oscillation
/// is a shape, which is what survives being read from across the room (PRD §1)
/// and what the box filter down to thumbnail size keeps.
const LEG_BOW: f64 = 0.75;

/// The most times one leg's bow keeps growing.
///
/// Past six passes the lens is as wide as it can be without reading as
/// scribble, and "a lot" is the message — which the revisit rosette
/// ([`Thrash`]) is already carrying as a count.
const MAX_LEG_BOW: u32 = 6;

/// How many chords a bowed leg is drawn with.
const BOW_CHORDS: usize = 8;

/// Draws one trail in the requested notation.
///
/// # The two candidate notations
///
/// **[`TrailStyle::Fade`]** is the null hypothesis the PRD offers: one
/// continuous stroke, tone ramped from [`AGENT_TRAIL`] down to the band floor
/// with age, constant width.
///
/// **[`TrailStyle::Timed`]** adds the explicit encoding §17 asks about. Three
/// channels, all geometric:
///
/// * **dash duty cycle** — a fresh segment is solid, an old one is a row of
///   dots. Period is fixed so the *rhythm* changes, not the scale;
/// * **width taper** — the stroke narrows with age;
/// * **beads** — a disc at every stop, so the *stops* are countable. A
///   continuous line hides how many operations a run of it represents, and
///   "six operations in one building" versus "six buildings in a row" is
///   exactly the distinction §12 wants readable.
///
/// # Repeated legs bow apart, in both notations
///
/// Neither notation could show a **return**, because a return is the same two
/// points in the opposite order and both drew it on top of the outbound leg.
/// [`leg_repeats`] counts how many times each leg has been walked and
/// [`leg_path`] bows the *n*-th pass off the line, so a backtrack is motion
/// rather than a counter. See [`LEG_BOW`].
pub fn draw_trail(canvas: &mut Canvas, trail: &Trail, style: TrailStyle, r: f64) {
    if trail.steps.len() < 2 {
        return;
    }
    let ttl = trail.ttl.max(1e-6);
    let repeats = leg_repeats(&trail.steps);
    let mut path: Vec<Px> = Vec::with_capacity(BOW_CHORDS + 1);
    for (leg, pair) in trail.steps.windows(2).enumerate() {
        let (a, b) = (pair[0], pair[1]);
        // A segment is as old as its *newer* end: the eye reads a stroke as one
        // object, and grading it by its older end makes the whole trail look
        // staler than it is.
        let age = (b.age / ttl).clamp(0.0, 1.0);
        let fresh = 1.0 - age;
        let ink = fade(AGENT_TRAIL, AGENT_FLOOR, 0.75f64.mul_add(fresh, 0.25));
        leg_path(a.at, b.at, repeats[leg], r, &mut path);
        match style {
            TrailStyle::Fade => {
                canvas.polyline(&path, (r * 0.20).max(MIN_STROKE), ink, 1.0);
            }
            TrailStyle::Timed => {
                let width = (r * 0.26 * 0.55f64.mul_add(fresh, 0.45)).max(MIN_STROKE);
                // Duty cycle is the time encoding: solid when new, sparse when
                // old, and never zero — a trail that vanishes is a trail that
                // cannot be counted.
                let duty = 0.72f64.mul_add(smoothstep(fresh), 0.18);
                dashed_path(canvas, &path, r * DASH_PERIOD, duty, width, ink);
            }
        }
    }
    if style == TrailStyle::Timed {
        for step in &trail.steps {
            let fresh = 1.0 - (step.age / ttl).clamp(0.0, 1.0);
            let ink = fade(AGENT_TRAIL, AGENT_FLOOR, 0.75f64.mul_add(fresh, 0.25));
            canvas.disc(
                step.at,
                (r * 0.16 * 0.5f64.mul_add(fresh, 0.5)).max(MIN_STROKE * 0.7),
                ink,
                1.0,
            );
        }
    }
}

/// How many times each leg of a trail has already been walked.
///
/// `out[i]` is the number of **earlier** segments joining the same unordered
/// pair of stops as `steps[i] → steps[i+1]`. Direction is deliberately ignored:
/// `auth → tests` and `tests → auth` are the same road, and the whole point is
/// to keep the return leg off the outbound one.
///
/// Positions are quantised to whole pixels before keying, because two stops at
/// the same building come back from the projection at the same pixel and a
/// float key would call them different roads.
#[must_use]
pub fn leg_repeats(steps: &[TrailStep]) -> Vec<u32> {
    let n = steps.len().saturating_sub(1);
    let mut out = vec![0u32; n];
    if n < 2 {
        return out;
    }
    // Sort a key/index list and walk the runs, rather than keeping a map: this
    // runs on every trail of every frame inside PRD §13.1's 4 ms budget, and a
    // `BTreeMap` per trail was measured at a fifth of it on a forty-thread
    // frame. One allocation, one sort, one pass.
    let key = |p: Px| (p[0].round() as i64, p[1].round() as i64);
    let mut legs: Vec<([i64; 4], u32)> = Vec::with_capacity(n);
    for (i, pair) in steps.windows(2).enumerate() {
        let (a, b) = (key(pair[0].at), key(pair[1].at));
        // Unordered: the smaller endpoint first. `auth → tests` and
        // `tests → auth` are the same road.
        let k = if a <= b {
            [a.0, a.1, b.0, b.1]
        } else {
            [b.0, b.1, a.0, a.1]
        };
        legs.push((k, i as u32));
    }
    legs.sort_unstable();
    let mut run = 0u32;
    for i in 0..legs.len() {
        if i > 0 && legs[i].0 == legs[i - 1].0 {
            run += 1;
        } else {
            run = 0;
        }
        out[legs[i].1 as usize] = run;
    }
    // Within one key the sort is by leg index, so the counts are in walk order.
    out
}

/// Builds the polyline one leg is drawn along, bowing repeat passes apart.
///
/// The first pass is the straight line, so an ordinary trail is unchanged. Each
/// later pass swings to the other side and a little further out, up to
/// [`MAX_LEG_BOW`], which turns `A → B → A` into a lens and `A → B → A → B → A`
/// into a spindle. A degenerate leg — both stops on one pixel — is dropped: it
/// is the same building twice, which the bead and the rosette already say.
pub fn leg_path(a: Px, b: Px, repeat: u32, r: f64, out: &mut Vec<Px>) {
    out.clear();
    if repeat == 0 {
        out.push(a);
        out.push(b);
        return;
    }
    let dx = b[0] - a[0];
    let dy = b[1] - a[1];
    let len = dx.mul_add(dx, dy * dy).sqrt();
    if len < 1e-6 {
        out.push(a);
        out.push(b);
        return;
    }
    let n = repeat.min(MAX_LEG_BOW);
    // Passes alternate sides: 1 → +1, 2 → −1, 3 → +2, 4 → −2, …
    let rank = f64::from(n.div_ceil(2));
    let sign = if n % 2 == 1 { 1.0 } else { -1.0 };
    // Bounded by the leg's own length as well as by the radius, so a short
    // hop between two neighbouring buildings does not sprout a balloon.
    let bow = (rank * r * LEG_BOW).min(len * 0.45) * sign;
    let mid = [f64::midpoint(a[0], b[0]), f64::midpoint(a[1], b[1])];
    // The normal is taken from the leg's **canonical** direction — the same
    // smaller-endpoint-first rule [`leg_repeats`] keys on — and not from the
    // direction of travel. Otherwise the return pass flips both the sign and
    // the normal, the two cancel, and every pass bows to the same side: the
    // stack of arcs reads as one thick line again, which was the whole bug.
    let flip = if (a[0], a[1]) <= (b[0], b[1]) {
        1.0
    } else {
        -1.0
    };
    // Quadratic control point: the curve reaches half the control offset, so
    // the visible bow is `bow / 2`. Doubling it here keeps `LEG_BOW` readable
    // as "how far the stroke actually moves".
    let ctrl = [
        (-dy / len).mul_add(bow * 2.0 * flip, mid[0]),
        (dx / len).mul_add(bow * 2.0 * flip, mid[1]),
    ];
    for i in 0..=BOW_CHORDS {
        let t = i as f64 / BOW_CHORDS as f64;
        let u = 1.0 - t;
        out.push([
            (t * t).mul_add(b[0], (u * u).mul_add(a[0], 2.0 * u * t * ctrl[0])),
            (t * t).mul_add(b[1], (u * u).mul_add(a[1], 2.0 * u * t * ctrl[1])),
        ]);
    }
}

/// Strokes a dashed polyline with a fixed period and a variable duty cycle.
///
/// The dash phase runs along the **whole** path rather than restarting at every
/// chord, so a bowed leg keeps the same rhythm as a straight one — the duty
/// cycle is the time encoding and it must not change because a leg curved.
fn dashed_path(canvas: &mut Canvas, points: &[Px], period: f64, duty: f64, width: f64, ink: Rgb) {
    if points.len() < 2 {
        return;
    }
    let mut total = 0.0;
    for pair in points.windows(2) {
        let dx = pair[1][0] - pair[0][0];
        let dy = pair[1][1] - pair[0][1];
        total += dx.mul_add(dx, dy * dy).sqrt();
    }
    if total < 1e-9 {
        return;
    }
    // A dash period is a *rhythm*, and a rhythm needs a bounded number of
    // beats. A trail step that crosses the whole map at a fixed period draws
    // sixty dashes, which reads as a dotted line rather than as a rhythm and
    // costs sixty polygon fills — measured, that was most of a 6 ms frame. So
    // a long path stretches its period instead of subdividing further.
    let period = period.max(2.0).max(total / MAX_DASHES);
    if duty >= 0.995 {
        canvas.polyline(points, width, ink, 1.0);
        return;
    }
    let on = (period * duty.clamp(0.05, 1.0)).max(MIN_STROKE);
    let mut walked = 0.0;
    for pair in points.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let dx = b[0] - a[0];
        let dy = b[1] - a[1];
        let len = dx.mul_add(dx, dy * dy).sqrt();
        if len < 1e-9 {
            continue;
        }
        let at = |u: f64| [(dx / len).mul_add(u, a[0]), (dy / len).mul_add(u, a[1])];
        // The first dash of this chord starts wherever the running phase left
        // off on the previous one.
        let phase = walked % period;
        let mut t = if phase <= 0.0 { 0.0 } else { -phase };
        while t < len {
            let start = t.max(0.0);
            let stop = (t + on).min(len);
            if stop > start {
                canvas.segment(at(start), at(stop), width, ink, 1.0);
            }
            t += period;
        }
        walked += len;
    }
}

// ---------------------------------------------------------------------------
// Tethers, thrash, scaffolding
// ---------------------------------------------------------------------------

fn draw_tether(canvas: &mut Canvas, tether: Tether, r: f64) {
    // A tether is *structure*, not activity: it says which thread a worker
    // belongs to and nothing about what either is doing. So it is the dimmest
    // thing in the agent band — measured on a real frame, a tether at full tone
    // was the brightest structure on the map and the least informative.
    let (width, tone) = if tether.running {
        ((r * 0.09).max(MIN_STROKE * 0.75), 0.16)
    } else {
        ((r * 0.07).max(MIN_STROKE * 0.6), 0.0)
    };
    let ink = fade(AGENT_TETHER, AGENT_FLOOR, tone);
    // A tether bows away from the straight line so two workers on opposite
    // sides of an anchor do not draw one line through it.
    let mid = [
        f64::midpoint(tether.anchor[0], tether.worker[0]),
        f64::midpoint(tether.anchor[1], tether.worker[1]),
    ];
    let dx = tether.worker[0] - tether.anchor[0];
    let dy = tether.worker[1] - tether.anchor[1];
    let len = dx.mul_add(dx, dy * dy).sqrt().max(1e-6);
    let bow = (len * 0.075).min(r * 5.0) * tether.spread.clamp(-1.0, 1.0);
    let ctrl = [
        (-dy / len).mul_add(bow, mid[0]),
        (dx / len).mul_add(bow, mid[1]),
    ];
    let mut pts = Vec::with_capacity(9);
    for i in 0..=8 {
        let t = f64::from(i) / 8.0;
        let u = 1.0 - t;
        pts.push([
            (t * t).mul_add(
                tether.worker[0],
                (u * u).mul_add(tether.anchor[0], 2.0 * u * t * ctrl[0]),
            ),
            (t * t).mul_add(
                tether.worker[1],
                (u * u).mul_add(tether.anchor[1], 2.0 * u * t * ctrl[1]),
            ),
        ]);
    }
    canvas.polyline(&pts, width, ink, 1.0);
}

fn draw_thrash(canvas: &mut Canvas, thrash: Thrash, r: f64) {
    if thrash.visits < 3 {
        return;
    }
    let fresh = 1.0 - thrash.age.clamp(0.0, 1.0);
    let ink = fade(AGENT_TRAIL, AGENT_FLOOR, 0.6f64.mul_add(fresh, 0.4));
    // One tick per visit, up to a ring's worth. Past sixteen the count stops
    // being the point and "a lot" is the message.
    let n = (thrash.visits as usize).min(CIRCLE.len());
    let r0 = r * 1.25;
    let r1 = r * (1.55 + 0.35 * (n as f64 / CIRCLE.len() as f64));
    for dir in CIRCLE.iter().take(n) {
        canvas.segment(
            [
                dir[0].mul_add(r0, thrash.at[0]),
                dir[1].mul_add(r0, thrash.at[1]),
            ],
            [
                dir[0].mul_add(r1, thrash.at[0]),
                dir[1].mul_add(r1, thrash.at[1]),
            ],
            (r * 0.15).max(MIN_STROKE),
            ink,
            1.0,
        );
    }
}

fn draw_scaffold(canvas: &mut Canvas, s: Scaffold, r: f64) {
    if s.rise <= 0.5 {
        return;
    }
    let fresh = 1.0 - s.age.clamp(0.0, 1.0);
    // Scaffolding is a **state**, not an event: "this file has uncommitted work
    // on it". It is therefore the quietest thing in the agent band — on the
    // first rendered frame it was the loudest, forty bright frames that read as
    // the subject of the picture while the operations happening *now* hid
    // underneath them.
    let ink = fade(AGENT_SCAFFOLD, AGENT_FLOOR, 0.30f64.mul_add(fresh, 0.10));
    let w = s.half_width.max(r * 0.5);
    let top = s.at[1] - s.rise;
    let width = (r * 0.09).max(MIN_STROKE * 0.6);
    // An open frame, never a filled block: the whole point of PRD §8's
    // scaffolding is that it reads as temporary.
    canvas.segment([s.at[0] - w, s.at[1]], [s.at[0] - w, top], width, ink, 1.0);
    canvas.segment([s.at[0] + w, s.at[1]], [s.at[0] + w, top], width, ink, 1.0);
    canvas.segment([s.at[0] - w, top], [s.at[0] + w, top], width, ink, 1.0);
    // Two putlogs, so the frame has a scale and is not read as a bracket.
    for k in 1..=2 {
        let y = (s.rise * f64::from(k) / 3.0).mul_add(-1.0, s.at[1]);
        canvas.segment([s.at[0] - w, y], [s.at[0] + w, y], width * 0.8, ink, 1.0);
    }
}

// ---------------------------------------------------------------------------
// Operation marks and agents
// ---------------------------------------------------------------------------

fn draw_mark(canvas: &mut Canvas, mark: Mark, r: f64) {
    let fresh = 1.0 - mark.age.clamp(0.0, 1.0);
    // Size carries age as well as tone, so the newest operation is the biggest
    // thing in its neighbourhood even in a screenshot with no colour — then
    // positional certainty (`scale`) and multiplicity (`count`). None of the
    // three is an outcome: colour is the only thing that says how it went.
    let size = r
        * 0.35f64.mul_add(smoothstep(fresh), 0.65)
        * mark.scale.clamp(0.2, 2.0)
        * stack_scale(mark.count);
    let ink = fade(
        outcome_ink(mark.outcome),
        AGENT_FLOOR,
        0.7f64.mul_add(fresh, 0.3),
    );
    draw_glyph(canvas, mark.at, size, mark.glyph, ink);
    if mark.pulse > 0.0 {
        // PRD §11.4: the arrival is a brief expanding ring. Motion onset, not
        // hue, is what the periphery catches.
        let p = mark.pulse.clamp(0.0, 1.0);
        let radius = size * 2.6f64.mul_add(1.0 - p, 1.15);
        let ring = fade(outcome_ink(mark.outcome), AGENT_FLOOR, p);
        stroke_circle(
            canvas,
            mark.at,
            radius,
            (r * 0.16 * p).max(MIN_STROKE),
            ring,
        );
    }
}

fn draw_agent(canvas: &mut Canvas, agent: Agent, r: f64) {
    let ink = outcome_ink(agent.outcome);
    let body = if agent.body == Body::Main {
        AGENT_ANCHOR
    } else {
        AGENT_BODY
    };
    // The motion streak: a short tail behind a moving agent, which is the
    // cheapest possible way to make direction readable in the periphery.
    if agent.travel > 0.02 {
        let l = r * 3.2 * agent.travel.clamp(0.0, 1.0);
        let tail = [
            agent.heading[0].mul_add(-l, agent.at[0]),
            agent.heading[1].mul_add(-l, agent.at[1]),
        ];
        canvas.segment(
            tail,
            agent.at,
            (r * 0.22).max(MIN_STROKE),
            fade(body, AGENT_FLOOR, agent.travel.clamp(0.0, 1.0)),
            1.0,
        );
    }
    match agent.body {
        // A main agent has no meaningful point location (PRD §6) — it is drawn
        // as an open ring at its territory's centre of mass, which is a shape
        // that reads as "a region" rather than as "a thing at this pixel".
        Body::Main => {
            stroke_circle(canvas, agent.at, r * 1.05, (r * 0.24).max(MIN_STROKE), body);
            canvas.disc(agent.at, r * 0.30, ink, 1.0);
        }
        Body::Worker => {
            canvas.disc(agent.at, r * 0.62, body, 1.0);
            canvas.disc(agent.at, r * 0.34, ink, 1.0);
        }
    }
    // A waiting thread gets a gap in its own ring — a shape cue for the state
    // the attention layer will also mark, so the two never disagree.
    if agent.waiting {
        stroke_circle(canvas, agent.at, r * 1.45, (r * 0.14).max(MIN_STROKE), body);
    }
}

/// Draws one of PRD §10.1's six operation glyphs.
///
/// | Operation | Glyph |
/// |---|---|
/// | read / scan | hollow circle |
/// | edit | circle with a bar through |
/// | write | filled square |
/// | run (bash) | filled triangle |
/// | verify / test | concentric circles |
/// | delegate | centre dot with three satellites |
///
/// Shape only. The colour arrives as an argument and this function never looks
/// at an [`Outcome`].
pub fn draw_glyph(canvas: &mut Canvas, at: Px, r: f64, glyph: Glyph, colour: Rgb) {
    match glyph {
        Glyph::HollowCircle => stroke_circle(canvas, at, r, r * 0.42, colour),
        Glyph::BarredCircle => {
            stroke_circle(canvas, at, r, r * 0.42, colour);
            canvas.segment(
                [at[0] - r, at[1]],
                [at[0] + r, at[1]],
                r * 0.42,
                colour,
                1.0,
            );
        }
        Glyph::FilledSquare => canvas.rect(at[0] - r, at[1] - r, at[0] + r, at[1] + r, colour, 1.0),
        Glyph::FilledTriangle => canvas.fill_polygon(
            &[
                [at[0], at[1] - r],
                [at[0] + r, at[1] + r * 0.8],
                [at[0] - r, at[1] + r * 0.8],
            ],
            colour,
            1.0,
        ),
        Glyph::ConcentricCircles => {
            stroke_circle(canvas, at, r, r * 0.34, colour);
            stroke_circle(canvas, at, r * 0.5, r * 0.34, colour);
        }
        Glyph::Delegate => {
            canvas.disc(at, r * 0.34, colour, 1.0);
            for s in SATELLITES {
                canvas.disc(
                    [s[0].mul_add(r * 0.95, at[0]), s[1].mul_add(r * 0.95, at[1])],
                    r * 0.26,
                    colour,
                    1.0,
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Attention
// ---------------------------------------------------------------------------

fn draw_pin(canvas: &mut Canvas, mark: AttentionMark, r: f64) {
    let ink = fade(
        ATTN_DECISION,
        ATTENTION_FLOOR,
        0.35f64.mul_add(mark.weight, 0.65),
    );
    let h = r * 3.4;
    let head = [mark.at[0], mark.at[1] - h];
    canvas.segment(mark.at, head, (r * 0.22).max(MIN_STROKE), ink, 1.0);
    // A diamond head, which is the one silhouette in this module that no
    // operation glyph uses: a pin can never be mistaken for an edit.
    canvas.fill_polygon(
        &[
            [head[0], head[1] - r * 1.15],
            [head[0] + r * 0.80, head[1]],
            [head[0], head[1] + r * 1.15],
            [head[0] - r * 0.80, head[1]],
        ],
        ink,
        1.0,
    );
    if mark.pulse > 0.0 {
        let p = mark.pulse.clamp(0.0, 1.0);
        stroke_circle(
            canvas,
            head,
            r * 3.0f64.mul_add(1.0 - p, 1.3),
            (r * 0.2 * p).max(MIN_STROKE),
            ink,
        );
    }
}

fn draw_done(canvas: &mut Canvas, mark: AttentionMark, r: f64) {
    let ink = fade(ATTN_DONE, ATTENTION_FLOOR, 0.6f64.mul_add(mark.weight, 0.4));
    stroke_circle(canvas, mark.at, r * 1.5, (r * 0.28).max(MIN_STROKE), ink);
    if mark.kind == MarkKind::DoneUnverified {
        // The second ring is the whole distinction. "Done, unverified" is really
        // "needs review" and it persists (PRD §11.2b, §17 q2), so it must be
        // separable from "done, verified" without reading a colour.
        stroke_circle(canvas, mark.at, r * 2.35, (r * 0.20).max(MIN_STROKE), ink);
    }
    if mark.pulse > 0.0 {
        let p = mark.pulse.clamp(0.0, 1.0);
        stroke_circle(
            canvas,
            mark.at,
            r * 3.4f64.mul_add(1.0 - p, 1.6),
            (r * 0.2 * p).max(MIN_STROKE),
            ink,
        );
    }
}

fn draw_contention(canvas: &mut Canvas, mark: AttentionMark, r: f64) {
    let Some(other) = mark.other else {
        return;
    };
    let ink = fade(
        ATTN_CONTENTION,
        ATTENTION_FLOOR,
        0.4f64.mul_add(mark.weight, 0.6),
    );
    // Severity is carried by weight and rhythm first, colour second: PRD §11.4
    // forbids colour as the sole channel for any state, and the four tiers are
    // four *different links*, not four shades of red.
    let (width, duty) = match mark.severity.unwrap_or(Severity::High) {
        Severity::Critical => (r * 0.42, 1.0),
        Severity::High => (r * 0.32, 1.0),
        Severity::Medium => (r * 0.26, 0.55),
        Severity::Low => (r * 0.20, 0.25),
    };
    // The link bows, so it reads as joining two places rather than as a wall
    // across the ones between them.
    let mid = [
        f64::midpoint(mark.at[0], other[0]),
        f64::midpoint(mark.at[1], other[1]),
    ];
    let dx = other[0] - mark.at[0];
    let dy = other[1] - mark.at[1];
    let len = dx.mul_add(dx, dy * dy).sqrt().max(1e-6);
    let bow = (len * 0.18).max(r * 2.0);
    let ctrl = [
        (-dy / len).mul_add(bow, mid[0]),
        (dx / len).mul_add(bow, mid[1]),
    ];
    let mut pts = Vec::with_capacity(17);
    for i in 0..=16 {
        let t = f64::from(i) / 16.0;
        let u = 1.0 - t;
        pts.push([
            (t * t).mul_add(other[0], (u * u).mul_add(mark.at[0], 2.0 * u * t * ctrl[0])),
            (t * t).mul_add(other[1], (u * u).mul_add(mark.at[1], 2.0 * u * t * ctrl[1])),
        ]);
    }
    // One dash phase along the whole arc: dashing each chord separately
    // restarted the rhythm sixteen times and read as a bead string.
    dashed_path(canvas, &pts, r * 1.6, duty, width.max(MIN_STROKE), ink);
    for end in [mark.at, other] {
        canvas.disc(end, r * 0.9, ink, 1.0);
        if mark.pulse > 0.0 {
            let p = mark.pulse.clamp(0.0, 1.0);
            stroke_circle(
                canvas,
                end,
                r * 3.0f64.mul_add(1.0 - p, 1.1),
                (r * 0.2 * p).max(MIN_STROKE),
                ink,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Primitives shared with `plan`
// ---------------------------------------------------------------------------

/// Write one opaque cloud pixel.
///
/// # Why this is not a wash
///
/// The first version blended a translucent tone over every pixel inside an iso
/// band and then lifted the result to the band's floor. That is an area fill,
/// and measured on the shipped image it lifted **40.5 %** of the city by more
/// than six levels and moved the base median underneath it from `L 22` to
/// `L 45` — the map fogging into pale grey exactly where the activity was.
///
/// A cloud is instead a **set of marks the city shows through**. Marks are
/// opaque, so the layer provably owns its band with no floor-lifting trick, and
/// they are sparse, so the base map underneath is not merely recoverable, it is
/// **untouched**.
pub(crate) fn cloud_pixel(canvas: &mut Canvas, x: usize, y: usize, tone: Rgb) {
    if x >= canvas.width || y >= canvas.height {
        return;
    }
    let i = (y * canvas.width + x) * 3;
    canvas.pixels[i..i + 3].copy_from_slice(&tone);
}

/// Which iso band a field value falls in, or `None` outside the fringe.
pub(crate) fn iso_band(v: f64) -> Option<usize> {
    if v >= CLOUD_ISO[2] {
        Some(2)
    } else if v >= CLOUD_ISO[1] {
        Some(1)
    } else if v >= CLOUD_ISO[0] {
        Some(0)
    } else {
        None
    }
}

/// A quartic kernel with compact support: `(1 - r²)²` inside the radius.
///
/// PRD §10.4 splats Gaussians; a Gaussian needs `exp`, and nothing in the
/// rasteriser reaches for a transcendental (PRD §7.4). The quartic has the same
/// bell shape, has *finite* support — which makes the splat cheaper, not dearer
/// — and is a polynomial, so the same bytes come out on every libm.
pub(crate) fn kernel(r2: f64) -> f64 {
    if r2 >= 1.0 {
        return 0.0;
    }
    let k = 1.0 - r2;
    k * k
}

/// Stroke a circle of `radius` about `at`.
pub(crate) fn stroke_circle(canvas: &mut Canvas, at: Px, radius: f64, width: f64, colour: Rgb) {
    let pts: Vec<Px> = CIRCLE
        .iter()
        .map(|c| [c[0].mul_add(radius, at[0]), c[1].mul_add(radius, at[1])])
        .collect();
    canvas.stroke_polygon(&pts, width, colour, 1.0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{AGENT_BAND, ATTENTION_BAND, BASE_MAP_CEILING, CLOUD_BAND};

    fn head(c: Rgb) -> u8 {
        c.iter().copied().max().unwrap_or(0)
    }

    /// PRD §10.3's allocation, asserted on the palette rather than described in
    /// a comment. A colour belongs to the band its **largest** channel is in,
    /// which is the same rule `plan` measures rendered pixels with.
    #[test]
    fn every_live_colour_is_inside_the_band_its_layer_owns() {
        for tone in CLOUD_TONES {
            let h = head(tone);
            assert!(
                (CLOUD_BAND.0..=CLOUD_BAND.1).contains(&h),
                "cloud tone {tone:?} is at {h}, outside {CLOUD_BAND:?}"
            );
        }
        for (name, ink) in [
            ("pending", AGENT_PENDING),
            ("done", AGENT_DONE),
            ("failed", AGENT_FAILED),
            ("tether", AGENT_TETHER),
            ("trail", AGENT_TRAIL),
            ("body", AGENT_BODY),
            ("anchor", AGENT_ANCHOR),
            ("scaffold", AGENT_SCAFFOLD),
        ] {
            let h = head(ink);
            assert!(
                (AGENT_BAND.0..=AGENT_BAND.1).contains(&h),
                "agent ink {name} {ink:?} is at {h}, outside {AGENT_BAND:?}"
            );
        }
        for (name, ink) in [
            ("decision", ATTN_DECISION),
            ("done", ATTN_DONE),
            ("contention", ATTN_CONTENTION),
        ] {
            let h = head(ink);
            assert!(
                (ATTENTION_BAND.0..=ATTENTION_BAND.1).contains(&h),
                "attention ink {name} {ink:?} is at {h}, outside {ATTENTION_BAND:?}"
            );
        }
    }

    /// The rule the module docs argue for, as an assertion: a mark fades to its
    /// band's floor and stops. A fully faded agent mark is still brighter than
    /// every pixel the base map is allowed to draw.
    #[test]
    fn a_fully_faded_mark_never_falls_into_the_base_map() {
        for ink in [AGENT_PENDING, AGENT_DONE, AGENT_FAILED, AGENT_TRAIL] {
            let gone = fade(ink, AGENT_FLOOR, 0.0);
            assert_eq!(head(gone), AGENT_FLOOR, "{ink:?} faded to {gone:?}");
            assert!(head(gone) > BASE_MAP_CEILING);
        }
        for ink in [ATTN_DECISION, ATTN_DONE, ATTN_CONTENTION] {
            let gone = fade(ink, ATTENTION_FLOOR, 0.0);
            assert_eq!(head(gone), ATTENTION_FLOOR);
        }
        // …and it is a ramp, not a step.
        let mid = fade(AGENT_PENDING, AGENT_FLOOR, 0.5);
        assert!(head(mid) > AGENT_FLOOR && head(mid) < head(AGENT_PENDING));
        assert_eq!(fade(AGENT_PENDING, AGENT_FLOOR, 1.0), AGENT_PENDING);
    }

    /// Hue survives the fade: a failed operation stays red as it ages. If the
    /// ramp were per-channel-clamped rather than proportional, red would fade to
    /// grey and PRD §10.2's three-colour channel would collapse to two.
    #[test]
    fn fading_preserves_hue() {
        let faded = fade(AGENT_FAILED, AGENT_FLOOR, 0.2);
        assert!(faded[0] > faded[1] && faded[0] > faded[2], "{faded:?}");
        let teal = fade(AGENT_DONE, AGENT_FLOOR, 0.2);
        assert!(teal[1] > teal[0] && teal[1] >= teal[2], "{teal:?}");
    }

    /// PRD §10 opens with "never conflate them". The strongest available
    /// statement of that in code: every glyph draws identically whatever the
    /// outcome, and every outcome colours identically whatever the glyph.
    #[test]
    fn shape_and_colour_are_orthogonal() {
        let glyphs = [
            Glyph::HollowCircle,
            Glyph::BarredCircle,
            Glyph::FilledSquare,
            Glyph::FilledTriangle,
            Glyph::ConcentricCircles,
            Glyph::Delegate,
        ];
        // Same glyph, three outcomes: the *set of painted pixels* is identical.
        for glyph in glyphs {
            let mut shapes = Vec::new();
            for outcome in [Outcome::Pending, Outcome::Done, Outcome::Failed] {
                let mut c = Canvas::new(48, 48, [0, 0, 0]);
                draw_glyph(&mut c, [24.0, 24.0], 9.0, glyph, outcome_ink(outcome));
                shapes.push(
                    c.pixels
                        .as_chunks::<3>()
                        .0
                        .iter()
                        .map(|p| u8::from(p != &[0, 0, 0]))
                        .collect::<Vec<u8>>(),
                );
            }
            assert_eq!(shapes[0], shapes[1], "outcome changed the shape");
            assert_eq!(shapes[1], shapes[2], "outcome changed the shape");
        }
        // Six glyphs, six *different* shapes: the channel actually carries six
        // values and is not two shapes with decoration.
        let mut seen: Vec<Vec<u8>> = Vec::new();
        for glyph in glyphs {
            let mut c = Canvas::new(48, 48, [0, 0, 0]);
            draw_glyph(&mut c, [24.0, 24.0], 9.0, glyph, AGENT_PENDING);
            let mask: Vec<u8> = c
                .pixels
                .as_chunks::<3>()
                .0
                .iter()
                .map(|p| u8::from(p != &[0, 0, 0]))
                .collect();
            assert!(mask.contains(&1), "{glyph:?} drew nothing");
            assert!(
                !seen.contains(&mask),
                "{glyph:?} draws the same shape as another"
            );
            seen.push(mask);
        }
    }

    /// A glyph's core is **opaque**, which is what makes the band claim true of
    /// pixels and not only of constants.
    #[test]
    fn a_glyph_core_is_opaque_ink_and_not_a_blend() {
        let mut c = Canvas::new(40, 40, [20, 21, 24]);
        draw_glyph(&mut c, [20.0, 20.0], 8.0, Glyph::FilledSquare, AGENT_DONE);
        let i = (20 * 40 + 20) * 3;
        assert_eq!(&c.pixels[i..i + 3], &AGENT_DONE);
    }

    /// PRD §11.4: the arrival is a pulse, and the pulse is **motion**.
    ///
    /// Two halves, and the second is the one that matters. The mark has to reach
    /// visibly further from its centre while the pulse runs — a *change in
    /// extent* is what the periphery detects — and its steady-state core has to
    /// come out the same colour it went in, or the pulse has smuggled a second
    /// meaning into the colour channel.
    #[test]
    fn an_arrival_pulses_by_geometry_and_not_by_colour() {
        let render = |pulse: f64| {
            let mut c = Canvas::new(160, 160, [0, 0, 0]);
            draw_mark(
                &mut c,
                Mark::single([80.0, 80.0], Glyph::FilledSquare, Outcome::Done, 0.0, pulse),
                10.0,
            );
            c
        };
        let extent = |c: &Canvas| {
            let mut far = 0.0f64;
            for (i, p) in c.pixels.as_chunks::<3>().0.iter().enumerate() {
                if p == &[0, 0, 0] {
                    continue;
                }
                let dx = (i % 160) as f64 - 80.0;
                let dy = (i / 160) as f64 - 80.0;
                far = far.max(dx.mul_add(dx, dy * dy).sqrt());
            }
            far
        };
        let rest = render(0.0);
        let onset = render(0.5);
        assert!(
            extent(&onset) > extent(&rest) * 1.5,
            "the pulse reaches {:.1} px against {:.1} at rest — not a motion cue",
            extent(&onset),
            extent(&rest)
        );
        // The core is untouched: same pixel, same bytes.
        let i = (80 * 160 + 80) * 3;
        assert_eq!(rest.pixels[i..i + 3], onset.pixels[i..i + 3]);
    }

    /// PRD §10.4, as an assertion: a cloud is contour and hatch, never a fill.
    /// The measure is the one that matters to the operator — how much of the
    /// city the cloud covered.
    #[test]
    fn a_cloud_is_sparse_marks_and_the_city_shows_through() {
        let mut c = Canvas::new(300, 300, [20, 21, 24]);
        let kernels: Vec<CloudKernel> = (0..24)
            .map(|i| CloudKernel {
                at: [
                    f64::from(i % 6).mul_add(22.0, 90.0),
                    f64::from(i / 6).mul_add(22.0, 90.0),
                ],
                radius: 46.0,
                weight: 1.0,
            })
            .collect();
        let frame = LiveFrame {
            unit: 30.0,
            map_height: 300.0,
            clouds: kernels,
            ..LiveFrame::default()
        };
        draw_clouds(&mut c, &frame);
        let painted = c
            .pixels
            .as_chunks::<3>()
            .0
            .iter()
            .filter(|p| *p != &[20, 21, 24])
            .count();
        assert!(painted > 400, "the cloud drew almost nothing: {painted} px");
        let bands = band_map(&frame.clouds, 300, 300).expect("a field");
        let inside = bands.cells.iter().filter(|b| **b != NO_BAND).count();
        assert!(inside > 0);
        let coverage = painted as f64 / inside as f64;
        assert!(
            coverage < 0.5,
            "the cloud inked {:.0}% of its own footprint — that is a fill",
            coverage * 100.0
        );
        // Every painted pixel is exactly a cloud tone: opaque marks, no blend,
        // so the layer cannot lift the base map by a single level.
        for p in c.pixels.as_chunks::<3>().0 {
            if p != &[20, 21, 24] {
                assert!(CLOUD_TONES.contains(&[p[0], p[1], p[2]]), "{p:?}");
            }
        }
    }

    /// Three iso levels, and they **nest** — which is the distinction PRD §10.4
    /// exists to keep: *"that file is in the core of this thread's work"* versus
    /// *"it's at the fringe"*.
    ///
    /// Nesting rather than relative area, because area depends on the kernel
    /// arrangement and nesting does not: a contour map whose levels crossed
    /// would be meaningless whatever the areas came out at.
    #[test]
    fn the_field_resolves_a_core_from_a_fringe() {
        let kernels: Vec<CloudKernel> = (0..8)
            .map(|i| CloudKernel {
                at: [
                    150.0 + f64::from(i % 3) * 8.0,
                    150.0 + f64::from(i / 3) * 8.0,
                ],
                radius: 60.0,
                weight: 1.0,
            })
            .collect();
        let bands = band_map(&kernels, 300, 300).expect("a field");
        let mut counts = [0usize; 3];
        let mut bbox = [[usize::MAX, usize::MAX, 0usize, 0usize]; 3];
        for (i, b) in bands.cells.iter().enumerate() {
            if *b == NO_BAND {
                continue;
            }
            let k = *b as usize;
            counts[k] += 1;
            let (x, y) = (i % bands.width, i / bands.width);
            bbox[k][0] = bbox[k][0].min(x);
            bbox[k][1] = bbox[k][1].min(y);
            bbox[k][2] = bbox[k][2].max(x);
            bbox[k][3] = bbox[k][3].max(y);
        }
        assert!(
            counts[0] > 0 && counts[1] > 0 && counts[2] > 0,
            "{counts:?}"
        );
        for k in 0..2 {
            assert!(
                bbox[k][0] <= bbox[k + 1][0]
                    && bbox[k][1] <= bbox[k + 1][1]
                    && bbox[k][2] >= bbox[k + 1][2]
                    && bbox[k][3] >= bbox[k + 1][3],
                "band {} does not enclose band {}: {:?} vs {:?}",
                k,
                k + 1,
                bbox[k],
                bbox[k + 1]
            );
        }
    }

    /// The trail question (PRD §17 q3) in its testable half: the explicit
    /// encoding has to be *visibly* different from fade alone, or there is
    /// nothing to choose between.
    #[test]
    fn the_two_trail_notations_are_different_pictures() {
        let steps: Vec<TrailStep> = (0..10)
            .map(|i| TrailStep {
                at: [20.0 + f64::from(i) * 26.0, 150.0],
                age: f64::from(9 - i) * 30.0,
                visits: 1,
            })
            .collect();
        let trail = Trail { steps, ttl: 300.0 };
        let render = |style| {
            let mut c = Canvas::new(300, 300, [0, 0, 0]);
            draw_trail(&mut c, &trail, style, 9.0);
            c
        };
        let fade_c = render(TrailStyle::Fade);
        let timed_c = render(TrailStyle::Timed);
        assert_ne!(fade_c.pixels, timed_c.pixels);
        let ink = |c: &Canvas| {
            c.pixels
                .as_chunks::<3>()
                .0
                .iter()
                .filter(|p| *p != &[0, 0, 0])
                .count()
        };
        assert!(ink(&fade_c) > 0 && ink(&timed_c) > 0);
        // The old end of a timed trail is sparser than the new end. That is the
        // whole claim of the notation, so it is the thing to assert.
        let column_ink = |c: &Canvas, x0: usize, x1: usize| {
            let mut n = 0;
            for y in 0..300 {
                for x in x0..x1 {
                    let i = (y * 300 + x) * 3;
                    if c.pixels[i] != 0 {
                        n += 1;
                    }
                }
            }
            n
        };
        let old = column_ink(&timed_c, 30, 110);
        let new = column_ink(&timed_c, 180, 260);
        assert!(new > old, "timed trail: new {new} px, old {old} px");
    }

    /// Draws the thrashing notation at the size it is really drawn, for the eye
    /// rather than for an assertion: one, two, four and six passes over the same
    /// pair of buildings, plus the revisit rosette beside them.
    ///
    /// ```text
    /// POLIS_OUT=<dir> cargo test -p polis-render --release -- --ignored --nocapture thrashing_sheet
    /// ```
    #[test]
    #[ignore = "writes an image"]
    fn thrashing_sheet() {
        let Ok(out) = std::env::var("POLIS_OUT") else {
            eprintln!("skipped: set POLIS_OUT to a directory");
            return;
        };
        let r = 9.0;
        let mut c = Canvas::new(760, 260, [20, 21, 24]);
        for (row, passes) in [1usize, 2, 4, 6].into_iter().enumerate() {
            let y = 45.0 + row as f64 * 60.0;
            let (a, b) = (90.0, 330.0);
            let steps: Vec<TrailStep> = (0..=passes)
                .map(|i| TrailStep {
                    at: [if i % 2 == 0 { a } else { b }, y],
                    age: (passes - i) as f64 * 12.0,
                    visits: (passes / 2 + 1) as u32,
                })
                .collect();
            draw_trail(&mut c, &Trail { steps, ttl: 300.0 }, TrailStyle::Timed, r);
            draw_thrash(
                &mut c,
                Thrash {
                    at: [560.0, y],
                    visits: passes as u32 + 1,
                    age: 0.1,
                },
                r,
            );
            c.text(650.0, y - 6.0, &format!("{passes} PASS"), 1.4, AGENT_BODY);
        }
        let path = std::path::Path::new(&out).join("thrashing.png");
        c.write_png(&path).expect("png");
        eprintln!("wrote {}", path.display());
    }

    /// PRD §12's other claim, which the trail was silently not delivering:
    ///
    /// > you can see backtracking, thrashing (the same building revisited six
    /// > times), and scope creep in it
    ///
    /// `A → B → A` pushes three stops and draws two segments — and the two
    /// segments are the same two points in the opposite order, so the return
    /// leg landed exactly on the outbound one. Six passes drew as one line. The
    /// assertion is the fix in its measurable form: a round trip must cover
    /// **more** pixels than a one-way trip between the same two buildings.
    #[test]
    fn a_return_leg_is_visible_as_motion_and_not_as_an_overstroke() {
        let stop = |x: f64, age: f64, visits: u32| TrailStep {
            at: [x, 150.0],
            age,
            visits,
        };
        let render = |steps: Vec<TrailStep>, style| {
            let mut c = Canvas::new(300, 300, [0, 0, 0]);
            draw_trail(&mut c, &Trail { steps, ttl: 300.0 }, style, 9.0);
            c.pixels
                .as_chunks::<3>()
                .0
                .iter()
                .filter(|p| *p != &[0, 0, 0])
                .count()
        };
        for style in [TrailStyle::Fade, TrailStyle::Timed] {
            let one_way = render(vec![stop(60.0, 10.0, 1), stop(240.0, 0.0, 1)], style);
            let there_and_back = render(
                vec![
                    stop(60.0, 20.0, 2),
                    stop(240.0, 10.0, 1),
                    stop(60.0, 0.0, 2),
                ],
                style,
            );
            assert!(
                there_and_back > one_way + 40,
                "{style:?}: a return drew {there_and_back} px against {one_way} for one way — \
                 the two legs are still coincident"
            );
        }
    }

    /// Each further pass swings the other way and a little wider, so an
    /// oscillation reads as a spindle rather than as a thicker line.
    #[test]
    fn repeat_passes_alternate_sides_and_grow() {
        let a = [0.0, 0.0];
        let b = [100.0, 0.0];
        let mut path = Vec::new();
        leg_path(a, b, 0, 10.0, &mut path);
        assert_eq!(path, vec![a, b], "the first pass is the straight line");

        let apex = |repeat: u32| {
            let mut p = Vec::new();
            leg_path(a, b, repeat, 10.0, &mut p);
            p[p.len() / 2][1]
        };
        let (one, two, three) = (apex(1), apex(2), apex(3));
        assert!(one > 0.0 && two < 0.0, "passes alternate: {one}, {two}");
        assert!(three.abs() > one.abs(), "and widen: {three} against {one}");
        // Past the cap the lens stops growing; the revisit rosette carries the
        // rest of the count.
        assert_eq!(apex(MAX_LEG_BOW), apex(MAX_LEG_BOW + 40));

        // And the alternation must survive the *direction* flip, which is what
        // a return leg is. Pass 2 runs `b → a`; taking the normal from the
        // direction of travel makes its sign flip and its direction flip
        // cancel, and it lands back on top of pass 1 — which is the bug this
        // whole notation exists to fix, reintroduced one level down.
        let mut back = Vec::new();
        leg_path(b, a, 2, 10.0, &mut back);
        let returning = back[back.len() / 2][1];
        assert!(
            returning < 0.0 && (returning - two).abs() < 1e-9,
            "the return pass bows to the far side: {returning} against {two}"
        );
    }

    #[test]
    fn a_leg_is_counted_by_its_road_and_not_by_its_direction() {
        let s = |x: f64| TrailStep {
            at: [x, 0.0],
            age: 0.0,
            visits: 1,
        };
        // A → B → A → B → C: the first three legs are one road walked three
        // times, the fourth is a new one.
        let steps = vec![s(0.0), s(50.0), s(0.0), s(50.0), s(90.0)];
        assert_eq!(leg_repeats(&steps), vec![0, 1, 2, 0]);
    }

    #[test]
    fn a_bow_never_outgrows_its_own_leg() {
        // A short hop between two neighbouring buildings must not sprout a
        // balloon wider than the gap it spans.
        let mut path = Vec::new();
        leg_path([0.0, 0.0], [6.0, 0.0], 5, 12.0, &mut path);
        let apex = path[path.len() / 2][1].abs();
        assert!(apex <= 6.0 * 0.45 + 1e-9, "apex {apex} on a 6 px leg");
    }

    /// Severity is not a shade of red (PRD §11.4: colour is never the sole
    /// channel). Two severities have to differ in geometry with the colour held
    /// constant.
    #[test]
    fn contention_severity_is_carried_by_the_link_and_not_by_the_hue() {
        // The two end discs are the same in every tier, so they are excluded:
        // measuring them would dilute the very difference under test.
        let render = |severity| {
            let mut c = Canvas::new(300, 200, [0, 0, 0]);
            draw_contention(
                &mut c,
                AttentionMark {
                    kind: MarkKind::Contention,
                    at: [40.0, 100.0],
                    other: Some([260.0, 100.0]),
                    severity: Some(severity),
                    pulse: 0.0,
                    weight: 1.0,
                },
                8.0,
            );
            let mut n = 0;
            for (i, p) in c.pixels.as_chunks::<3>().0.iter().enumerate() {
                let x = i % 300;
                if (90..210).contains(&x) && p != &[0, 0, 0] {
                    n += 1;
                }
            }
            n
        };
        let critical = render(Severity::Critical);
        let high = render(Severity::High);
        let medium = render(Severity::Medium);
        let low = render(Severity::Low);
        assert!(
            critical > high && high > medium && medium > low,
            "the four tiers are not four different links: {critical} {high} {medium} {low}"
        );
        assert!(
            critical > low * 2,
            "critical {critical} px vs low {low} px — the tiers look alike"
        );
    }

    /// "Done, unverified" persists and is really "needs review", so it must be
    /// separable from "done, verified" with the colour covered up.
    #[test]
    fn the_two_done_states_are_different_shapes() {
        let render = |kind| {
            let mut c = Canvas::new(160, 160, [0, 0, 0]);
            draw_done(
                &mut c,
                AttentionMark {
                    kind,
                    at: [80.0, 80.0],
                    other: None,
                    severity: None,
                    pulse: 0.0,
                    weight: 1.0,
                },
                10.0,
            );
            c.pixels
                .as_chunks::<3>()
                .0
                .iter()
                .filter(|p| *p != &[0, 0, 0])
                .count()
        };
        assert!(render(MarkKind::DoneUnverified) > render(MarkKind::DoneVerified));
    }

    /// The layer is a pure function of its input.
    #[test]
    fn the_same_frame_draws_the_same_bytes() {
        let frame = LiveFrame {
            unit: 30.0,
            map_height: 300.0,
            clouds: vec![CloudKernel {
                at: [150.0, 150.0],
                radius: 70.0,
                weight: 3.0,
            }],
            trails: vec![Trail {
                steps: (0..6)
                    .map(|i| TrailStep {
                        at: [40.0 + f64::from(i) * 30.0, 120.0],
                        age: f64::from(i) * 10.0,
                        visits: u32::try_from(i).unwrap_or(0) + 1,
                    })
                    .collect(),
                ttl: 300.0,
            }],
            marks: vec![Mark::single(
                [150.0, 150.0],
                Glyph::Delegate,
                Outcome::Pending,
                0.2,
                0.5,
            )],
            agents: vec![Agent {
                at: [150.0, 150.0],
                body: Body::Main,
                outcome: Outcome::Done,
                travel: 0.4,
                heading: [1.0, 0.0],
                waiting: true,
            }],
            attention: vec![AttentionMark {
                kind: MarkKind::NeedsDecision,
                at: [200.0, 200.0],
                other: None,
                severity: None,
                pulse: 0.3,
                weight: 1.0,
            }],
            ..LiveFrame::default()
        };
        let once = {
            let mut c = Canvas::new(300, 300, [10, 11, 12]);
            draw(&mut c, &frame, TrailStyle::Timed);
            c.pixels
        };
        let twice = {
            let mut c = Canvas::new(300, 300, [10, 11, 12]);
            draw(&mut c, &frame, TrailStyle::Timed);
            c.pixels
        };
        assert_eq!(once, twice);
    }

    /// Layer 5 owns the top of the range and **nothing else may enter it**.
    /// Rendered as two frames that differ only in whether an attention mark is
    /// present, so the claim is measured rather than asserted about constants.
    #[test]
    fn only_the_attention_layer_enters_the_attention_band() {
        let mut frame = LiveFrame {
            unit: 30.0,
            map_height: 400.0,
            clouds: vec![CloudKernel {
                at: [200.0, 200.0],
                radius: 90.0,
                weight: 4.0,
            }],
            trails: vec![Trail {
                steps: (0..8)
                    .map(|i| TrailStep {
                        at: [60.0 + f64::from(i) * 36.0, 200.0],
                        age: f64::from(i) * 20.0,
                        visits: 4,
                    })
                    .collect(),
                ttl: 300.0,
            }],
            tethers: vec![Tether {
                anchor: [200.0, 200.0],
                worker: [300.0, 120.0],
                running: true,
                spread: 0.4,
            }],
            thrash: vec![Thrash {
                at: [140.0, 200.0],
                visits: 7,
                age: 0.1,
            }],
            scaffolds: vec![Scaffold {
                at: [260.0, 240.0],
                half_width: 9.0,
                rise: 30.0,
                age: 0.2,
            }],
            marks: (0..6)
                .map(|i| {
                    Mark::single(
                        [60.0 + f64::from(i) * 36.0, 200.0],
                        Glyph::BarredCircle,
                        Outcome::Failed,
                        f64::from(i) / 6.0,
                        if i == 5 { 1.0 } else { 0.0 },
                    )
                })
                .collect(),
            agents: vec![Agent {
                at: [240.0, 200.0],
                body: Body::Worker,
                outcome: Outcome::Pending,
                travel: 0.6,
                heading: [1.0, 0.0],
                waiting: false,
            }],
            ..LiveFrame::default()
        };
        let count_above = |c: &Canvas, floor: u8| {
            c.pixels
                .as_chunks::<3>()
                .0
                .iter()
                .filter(|p| p.iter().copied().max().unwrap_or(0) >= floor)
                .count()
        };

        let mut without = Canvas::new(400, 400, [20, 21, 24]);
        draw(&mut without, &frame, TrailStyle::Timed);
        assert_eq!(
            count_above(&without, ATTENTION_BAND.0),
            0,
            "layers 3-4 put ink in the attention band"
        );
        assert!(
            count_above(&without, AGENT_BAND.0) > 200,
            "the agent band is barely used, so the assertion above is vacuous"
        );

        frame.attention = vec![AttentionMark {
            kind: MarkKind::NeedsDecision,
            at: [200.0, 300.0],
            other: None,
            severity: None,
            pulse: 0.0,
            weight: 1.0,
        }];
        let mut with = Canvas::new(400, 400, [20, 21, 24]);
        draw(&mut with, &frame, TrailStyle::Timed);
        assert!(
            count_above(&with, ATTENTION_BAND.0) > 40,
            "the attention mark did not reach its own band"
        );
    }

    /// PRD §13.1 budgets the agent **and** attention layers at under 4 ms. The
    /// scene is deliberately busier than a real one: forty threads, each with a
    /// full trail and a full mark history.
    #[test]
    fn the_agent_and_attention_layers_draw_inside_the_frame_budget() {
        let mut frame = LiveFrame {
            unit: 26.0,
            map_height: 1400.0,
            ..LiveFrame::default()
        };
        for t in 0..40 {
            let ox = f64::from(t % 8) * 170.0 + 60.0;
            let oy = f64::from(t / 8) * 260.0 + 90.0;
            frame.trails.push(Trail {
                steps: (0..64)
                    .map(|i| TrailStep {
                        at: [ox + f64::from(i % 8) * 18.0, oy + f64::from(i / 8) * 18.0],
                        age: f64::from(i) * 4.0,
                        visits: 1 + (i as u32 % 7),
                    })
                    .collect(),
                ttl: 300.0,
            });
            for w in 0..4 {
                frame.tethers.push(Tether {
                    anchor: [ox, oy],
                    worker: [ox + f64::from(w) * 30.0, oy + 60.0],
                    running: w % 2 == 0,
                    spread: f64::from(w) / 4.0,
                });
            }
            for i in 0..16 {
                frame.marks.push(Mark::single(
                    [ox + f64::from(i % 8) * 18.0, oy + f64::from(i / 8) * 18.0],
                    Glyph::BarredCircle,
                    Outcome::Pending,
                    f64::from(i) / 16.0,
                    0.0,
                ));
            }
            frame.agents.push(Agent {
                at: [ox, oy],
                body: Body::Main,
                outcome: Outcome::Pending,
                travel: 0.5,
                heading: [0.7, 0.7],
                waiting: t % 5 == 0,
            });
            frame.attention.push(AttentionMark {
                kind: MarkKind::NeedsDecision,
                at: [ox, oy],
                other: None,
                severity: None,
                pulse: 0.2,
                weight: 1.0,
            });
        }
        let mut c = Canvas::new(1400, 1400, [20, 21, 24]);
        // One warm pass so the measurement is not the first-touch page faults on
        // a 5.9 MB buffer.
        draw_agents(&mut c, &frame, TrailStyle::Timed);
        let timings = draw(&mut c, &frame, TrailStyle::Timed);
        let budget = if cfg!(debug_assertions) {
            Duration::from_millis(80)
        } else {
            Duration::from_millis(4)
        };
        println!(
            "live layer: clouds {:?}, agents {:?}, attention {:?} ({} trails, {} marks)",
            timings.clouds,
            timings.agents,
            timings.attention,
            frame.trails.len(),
            frame.marks.len()
        );
        assert!(
            timings.budgeted() <= budget,
            "agent+attention took {:?}, budget {budget:?}",
            timings.budgeted()
        );
    }
}
