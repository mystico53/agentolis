//! The headless frame renderer: a world snapshot and a time, in; an image, out.
//!
//! # What this is for
//!
//! PRD §15's M2 is "read one JSONL file offline and animate it over the city",
//! and it says why: *"This is the fastest possible loop for iterating on the
//! visual language — minutes per iteration, real data, no live infrastructure.
//! Spend real time here; most of the notation gets decided in this milestone."*
//!
//! A window cannot be that loop. Iterating on notation means rendering the same
//! moment of the same session under two rules and putting the pictures side by
//! side, and that needs a renderer with **no window, no adapter, no surface and
//! no clock of its own**. So:
//!
//! ```text
//! FrameRenderer::new(&city, opts)      // renders the base map once
//! renderer.render(&snapshot, frame_dt) // -> Canvas, per frame
//! canvas.write_png(path)               // or gif::encode(&frames, delay)
//! ```
//!
//! That is the whole API. It runs in a test, in CI, in a container, and it is
//! what records a session as a GIF for review.
//!
//! # Two clocks, and they are not the same clock
//!
//! [`FrameRenderer::render`] takes a **presentation** `dt` — how much time the
//! viewer experienced — while the snapshot carries the **world** time, which in
//! replay may be running at 8× or be jumping across a compressed idle gap.
//!
//! Ages, fades and decays are read off the world clock, because they are
//! statements about the session. Tweens run on the presentation clock, because
//! they are statements about the animation: an agent should take the same
//! two-fifths of a second to cross the map whether the replay is at 1× or 64×,
//! and a fade should not.
//!
//! Conflating them is the bug that makes a fast replay look like a slideshow of
//! teleporting dots, which is exactly the "steppy" failure PRD §13 names.
//!
//! # The base map is drawn once
//!
//! > **Base map cached to a texture.** The city changes on the order of
//! > seconds; agents move continuously. Redraw the base only on layout change.
//! > (PRD §13)
//!
//! Here the "texture" is a [`Canvas`] and the composite is a `memcpy`, but the
//! rule is the same one: [`FrameRenderer::new`] pays for the city once, and a
//! frame costs a buffer copy plus the live layer. On the agentolis repository
//! that is roughly 900 ms once against ~11 ms per frame.
//!
//! [`plan::MapFrame`] hands the in-map type back separately so it can be
//! re-stamped **over** the clouds every frame, which is PRD §10.3's layer order.
//!
//! # Height: what is tweened, and what is not
//!
//! PRD §13 asks for building heights to be interpolated. PRD §7.3 makes height
//! uncommitted diff lines, and PRD §10.3 puts the massing in layer 2 — under the
//! contrast ceiling. Those three cannot all be honoured by growing the building:
//! redrawing the massing per frame means redrawing the base per frame, and a
//! building can never be brighter than the agent standing on it anyway.
//!
//! So the *committed* height stays in the base map and the **delta being worked
//! on right now** is tweened in the agent band, as PRD §8's scaffolding
//! ([`live::Scaffold`]). The file's growth is visible while it happens, it reads
//! as impermanent because it is, and the base map is still drawn once.

// `float_cmp` fires on the tween assertions, and an approximate comparison
// there would be the bug: "the agent arrived" and "the tween did not start from
// nowhere" are exact statements about an exact interpolation. The `cast_*`
// family lands in pixel indices that are clamped on purpose, as in `plan`, and
// `a`, `b`, `w`, `h`, `r` are the names the geometry itself uses.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::float_cmp,
    clippy::many_single_char_names,
    clippy::too_many_lines
)]

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use polis_events::{LogicalPath, ThreadId, WorkerId};
use polis_layout::city::City;
use polis_layout::{CityLayout, Point};
use polis_world::attention::{Attention, AttentionKind};
use polis_world::place;
use polis_world::snapshot::WorldSnapshot;
use polis_world::{Thread, ThreadStatus};

use crate::live::{
    self, Agent, AttentionMark, Body, CloudKernel, LiveFrame, LiveTimings, Mark, MarkKind,
    Scaffold, Thrash, Trail, TrailStep, TrailStyle,
};
use crate::plan::{self, Focus, MapFrame, View};
use crate::raster::{Canvas, Px, Rgb};

/// How long an agent takes to cross from one building to the next, in
/// **presentation** seconds.
///
/// Short enough that the mark is on its target before the next operation lands
/// in ordinary traffic, long enough that the eye registers a direction. This is
/// the single number that decides whether the replay reads as alive or as a
/// slideshow.
pub const TRANSIT: f64 = 0.45;

/// How fast a scaffold's rise chases its target, in units of "fraction of the
/// remaining gap per second".
const RISE_RATE: f64 = 4.0;

/// How fast a cloud fades in or out, per presentation second.
const CLOUD_RATE: f64 = 2.5;

/// How long an operation mark survives, in **world** seconds.
///
/// A mark is a memory, not a state, so it has to expire or the map becomes a
/// palimpsest — but the expiry rule belongs to the **world**, not to the
/// renderer. `polis_world` already keeps `Thread::ops` for
/// [`polis_world::TRAIL_TTL`] and caps it at `OPS_CAP`, so a second, shorter
/// TTL here is a policy nobody declared and nobody can see.
///
/// It was 90 s, and on a real session that threw away five sixths of the
/// evidence: measured at the busiest moment of a 27-hour session, the world
/// held 127 placeable operations and the renderer drew **one**. The shape
/// channel was empty while the data was there. So the renderer now draws what
/// the world kept, fades it across the world's own window, and limits density
/// with a per-thread mark cap rather than with time.
pub const MARK_TTL: f64 = polis_world::TRAIL_TTL.as_secs() as f64;

/// The most marks drawn for one thread. `Thread::ops` is capped at 128 by the
/// world; drawing all of them at district zoom is confetti.
const MARKS_PER_THREAD: usize = 48;

/// The most tethers drawn for one thread, running workers first.
///
/// PRD §10.4 caps clouds — *"Forty threads means forty systems and the map
/// vanishes under haze"* — and the same argument applies with more force to
/// tethers, because a tether is a **line across the whole map** rather than a
/// blob in one place. Measured on a real session: 58 workers on one thread drew
/// 58 near-parallel lines converging on one point, which was the brightest
/// structure in the frame and said nothing except "this thread delegates".
///
/// Sixteen keeps "this is one unit with several hands" legible. The count is in
/// the caption for the rest.
const TETHERS_PER_THREAD: usize = 16;

/// The most scaffolds drawn, tallest first.
///
/// Same argument, plus a budget one: a session that has touched 130 files draws
/// 130 five-stroke frames, which on the measurement that matters — PRD §13.1's
/// 4 ms — was most of the layer.
const MAX_SCAFFOLDS: usize = 24;

/// How recently a file must have been touched to carry scaffolding, in world
/// seconds.
const SCAFFOLD_WINDOW: f64 = 180.0;

/// Diff lines that map to a full-height scaffold.
const SCAFFOLD_REFERENCE: f64 = 240.0;

/// The largest a coarse mark may be drawn, as a fraction of the map height.
///
/// [`live::glyph_radius`] tracks the *block* size, so at close zoom it reaches
/// its ceiling and every glyph is drawn the size of a building. That is right
/// for a mark that **is** at a building and wrong for one that is not: a rung-3
/// mark is a claim about an agent, not about a lot, and letting it grow to
/// building size turned the seat rosette into one blob. Measured on
/// `9cab97d7` at `extent 25`: five marks of 27 px radius on a ring of 58 px.
const COARSE_MARK_CAP: f64 = 0.011;

/// Six unit directions, 60° apart — one per PRD §10.1 shape.
///
/// A constant table for the same reason [`live::CIRCLE`] is one: no
/// transcendental reaches the image, so a frame is byte-reproducible.
const SEATS: [[f64; 2]; 6] = [
    [1.000, 0.000],
    [0.500, 0.866],
    [-0.500, 0.866],
    [-1.000, 0.000],
    [-0.500, -0.866],
    [0.500, -0.866],
];

/// Rounded position, glyph and outcome — what makes two operations one mark.
///
/// Outcome is in the key, which is the part that matters: a failure can never be
/// aggregated into a success, however many successes surround it.
type MarkKey = (i64, i64, u8, u8);

/// Operations that agreed on position, shape and outcome, on their way to one
/// [`Mark`].
#[derive(Debug, Clone, Copy)]
struct MarkAgg {
    at: Px,
    glyph: polis_events::Glyph,
    outcome: polis_events::Outcome,
    /// Age of the **freshest** member, in world seconds.
    age: f64,
    scale: f64,
    coarse: bool,
    count: u32,
}

/// Budget order: failures, then things still in flight, then successes.
///
/// This is the only place an outcome decides anything other than a colour, and
/// it decides *whether there is room*, not what the mark looks like. PRD §17:
/// the question a mark has to answer is "does it change a decision".
fn outcome_rank(outcome: polis_events::Outcome) -> u8 {
    match outcome {
        polis_events::Outcome::Failed => 0,
        polis_events::Outcome::Pending => 1,
        polis_events::Outcome::Done => 2,
    }
}

/// The seat a (shape, colour) pair always takes around a coarse position: a unit
/// direction and a reach **in output pixels**.
///
/// **Angle from the shape, radius from the outcome.** Failures ride the inner
/// ring — closest to the agent, least likely to be crowded or clipped — and the
/// successes are pushed outward, which is the de-emphasis PRD §17 argues for
/// without touching shape or colour.
///
/// Fixed rather than packed: a seat that moved as counts changed would make the
/// rosette twitch every frame, and PRD §11.4 spends motion onset on arrivals.
/// So the geometry is sized from `mark` — the largest a mark at this position
/// can be drawn — rather than the seats being crammed to fit: one ring holds
/// six shapes, so its circumference must be at least six mark diameters, and
/// consecutive rings must be a mark diameter apart.
fn seat_of(
    glyph: polis_events::Glyph,
    outcome: polis_events::Outcome,
    r: f64,
    mark: f64,
) -> ([f64; 2], f64) {
    // Clear the agent body and its waiting ring (`r * 1.45`) on the inside, and
    // give six seats room on the outside.
    let inner = 1.45f64.mul_add(r, mark).max(mark * 2.3);
    let step = mark * 2.6;
    (
        SEATS[usize::from(glyph as u8) % SEATS.len()],
        f64::from(outcome_rank(outcome)).mul_add(step, inner),
    )
}

/// What could not be placed, on its way to PRD §6.2's status rail.
#[derive(Debug, Default)]
struct RailBuilder {
    /// Per thread: label, operations with nowhere to go, of which failed.
    ops: BTreeMap<ThreadId, (String, u32, u32)>,
    /// Threads with no position at all — no converged territory, no placeable
    /// step. PRD §6.2's literal case.
    threads: Vec<String>,
}

impl RailBuilder {
    fn note_op(&mut self, thread: &Thread, op: &polis_world::Operation) {
        let entry = self
            .ops
            .entry(thread.id.clone())
            .or_insert_with(|| (thread_label(thread), 0, 0));
        entry.1 = entry.1.saturating_add(1);
        if op.outcome == polis_events::Outcome::Failed {
            entry.2 = entry.2.saturating_add(1);
        }
    }

    fn rows(self, unattributed: usize) -> Vec<live::RailRow> {
        let mut rows: Vec<live::RailRow> = Vec::new();
        for label in self.threads {
            rows.push(live::RailRow {
                kind: live::RailKind::UnplacedThread,
                label,
                count: 1,
                failed: 0,
            });
        }
        let mut ops: Vec<(String, u32, u32)> = self.ops.into_values().collect();
        // Most failures first: the rail is read top-down and the reason to look
        // at it is the red.
        ops.sort_by_key(|(label, count, failed)| {
            (
                std::cmp::Reverse(*failed),
                std::cmp::Reverse(*count),
                label.clone(),
            )
        });
        for (label, count, failed) in ops {
            rows.push(live::RailRow {
                kind: live::RailKind::UnplacedOps,
                label,
                count,
                failed,
            });
        }
        if unattributed > 0 {
            rows.push(live::RailRow {
                kind: live::RailKind::UnattributedWorkers,
                label: "no parent link".to_owned(),
                count: u32::try_from(unattributed).unwrap_or(u32::MAX),
                failed: 0,
            });
        }
        rows
    }
}

/// A short name for a thread, for the rail.
fn thread_label(thread: &Thread) -> String {
    thread.title.clone().unwrap_or_else(|| {
        let s = thread.session_id.as_str();
        s.get(..8).unwrap_or(s).to_owned()
    })
}

/// What the renderer draws, beyond the layers themselves.
#[derive(Debug, Clone, Copy)]
pub struct FrameOptions {
    /// Width and height of the **map** area in output pixels. The caption strip,
    /// if any, is added below it.
    pub pixels: usize,
    /// Supersample factor for the one-off base-map render. 2 is the useful
    /// setting; 1 is for tests.
    pub supersample: usize,
    /// PRD §9's import layer. Off by default at this zoom.
    pub streets: bool,
    /// Which trail notation to draw — PRD §17's open question 3.
    pub trail: TrailStyle,
    /// Draw the caption strip under the map.
    pub caption: bool,
    /// How many territories may show a cloud at once (PRD §10.4: *"Cap the
    /// number of visible clouds"*).
    pub cloud_cap: usize,
    /// Where to point the camera. `None` fits the whole city — PRD §12's city
    /// tier; `Some` is the district tier, and it is the one the notation has to
    /// be judged at.
    pub focus: Option<Focus>,
    /// Draw PRD §6.2's status rail as a column to the right of the map.
    ///
    /// **Added to** the canvas width rather than taken out of the map: the rail
    /// is where everything that has no place on the map goes, and paying for it
    /// in map area would be a strange trade for a strip of chrome.
    pub rail: bool,
}

impl Default for FrameOptions {
    fn default() -> Self {
        Self {
            pixels: 900,
            supersample: 2,
            streets: false,
            trail: TrailStyle::Timed,
            caption: true,
            cloud_cap: 6,
            focus: None,
            rail: true,
        }
    }
}

/// The status rail's width, as a fraction of the map's.
const RAIL_FRACTION: f64 = 0.22;

/// The narrowest a status rail is worth drawing, in output pixels.
const RAIL_MIN: usize = 130;

/// How many characters of the built-in 5x7 font fit across the rail.
///
/// `STATUS RAIL` and `EVERY OP PLACED` are the two longest fixed strings on it,
/// and the type is sized from this rather than from a fraction of the width so
/// neither can ever be clipped.
const RAIL_COLUMNS: f64 = 15.0;

/// An agent's identity across frames: a thread, and a worker inside it.
type AgentKey = (ThreadId, Option<WorkerId>);

/// One agent's interpolated position.
#[derive(Debug, Clone, Copy)]
struct Motion {
    from: Px,
    to: Px,
    /// Progress in `[0, 1]`, eased by [`live::smoothstep`].
    t: f64,
}

impl Motion {
    fn at(&self) -> Px {
        let e = live::smoothstep(self.t);
        [
            (self.to[0] - self.from[0]).mul_add(e, self.from[0]),
            (self.to[1] - self.from[1]).mul_add(e, self.from[1]),
        ]
    }

    fn heading(&self) -> [f64; 2] {
        let dx = self.to[0] - self.from[0];
        let dy = self.to[1] - self.from[1];
        let l = dx.mul_add(dx, dy * dy).sqrt();
        if l < 1e-6 {
            [0.0, 0.0]
        } else {
            [dx / l, dy / l]
        }
    }
}

/// Renders frames of a live world over a fixed city, with no window.
#[derive(Debug)]
pub struct FrameRenderer {
    opts: FrameOptions,
    base: MapFrame,
    /// The composited canvas, reused between frames so a frame does not
    /// allocate 5 MB.
    scratch: Canvas,
    caption_height: usize,
    rail_width: usize,
    motion: BTreeMap<AgentKey, Motion>,
    rise: BTreeMap<LogicalPath, f64>,
    cloud_presence: f64,
    label: String,
    timings: LiveTimings,
    frames: u64,
}

impl FrameRenderer {
    /// Renders the base map once and prepares the tween state.
    ///
    /// This is the expensive call — the whole city — and it is the only one.
    #[must_use]
    pub fn new(city: &City, opts: FrameOptions) -> Self {
        let base = plan::render_map_frame(
            city,
            opts.pixels,
            opts.supersample,
            opts.streets,
            opts.focus,
        );
        let caption_height = if opts.caption {
            (opts.pixels / 11).max(48)
        } else {
            0
        };
        let rail_width = if opts.rail {
            (((opts.pixels as f64) * RAIL_FRACTION) as usize).max(RAIL_MIN)
        } else {
            0
        };
        let scratch = Canvas::new(
            opts.pixels + rail_width,
            opts.pixels + caption_height,
            CAPTION_PLATE,
        );
        Self {
            opts,
            base,
            scratch,
            caption_height,
            rail_width,
            motion: BTreeMap::new(),
            rise: BTreeMap::new(),
            cloud_presence: 0.0,
            label: String::new(),
            timings: LiveTimings::default(),
            frames: 0,
        }
    }

    /// The transform the base map was drawn with. Anything placed on a frame by
    /// hand must use this one.
    #[must_use]
    pub fn view(&self) -> &View {
        &self.base.view
    }

    /// The map unit — the median block diameter in pixels.
    #[must_use]
    pub fn unit(&self) -> f64 {
        self.base.unit
    }

    /// Output size in pixels, caption strip and status rail included.
    #[must_use]
    pub fn size(&self) -> (usize, usize) {
        (
            self.opts.pixels + self.rail_width,
            self.opts.pixels + self.caption_height,
        )
    }

    /// Width of the map area alone — the square everything live is drawn in.
    #[must_use]
    pub fn map_pixels(&self) -> usize {
        self.opts.pixels
    }

    /// Sets the free-text label drawn at the left of the caption strip —
    /// typically the session clock the recorder is driving.
    pub fn set_label(&mut self, label: impl Into<String>) {
        self.label = label.into();
    }

    /// What the live layer cost on the last frame. PRD §13.1 budgets layers 4
    /// and 5 at under 4 ms; [`LiveTimings::budgeted`] is that number.
    #[must_use]
    pub fn timings(&self) -> LiveTimings {
        self.timings
    }

    /// Re-points the camera and re-renders the base map.
    ///
    /// > **Follow a thread**: binds the camera to a thread. **Cut, do not pan.**
    /// > (PRD §12)
    ///
    /// A cut, necessarily: the base map is a cached image, so there is no
    /// intermediate view to draw. That is the PRD's preference anyway.
    pub fn refocus(&mut self, city: &City, focus: Option<Focus>) {
        self.opts.focus = focus;
        self.invalidate_base(city);
    }

    /// Re-renders the cached base map. Only on layout change (PRD §13).
    pub fn invalidate_base(&mut self, city: &City) {
        self.base = plan::render_map_frame(
            city,
            self.opts.pixels,
            self.opts.supersample,
            self.opts.streets,
            self.opts.focus,
        );
    }

    /// Advances the tweens by `dt` of **presentation** time and draws a frame.
    ///
    /// `dt` is what the viewer experienced, not what the session did: see the
    /// module docs. Pass `Duration::ZERO` to redraw a frame without moving
    /// anything, which is how the same moment gets rendered under two notations.
    pub fn render(&mut self, snapshot: &WorldSnapshot, dt: Duration) -> &Canvas {
        let frame = self.build(snapshot, dt.as_secs_f64());
        self.compose(&frame, snapshot);
        self.frames += 1;
        &self.scratch
    }

    /// Renders and hands back an owned copy, for callers collecting a sequence.
    pub fn render_owned(&mut self, snapshot: &WorldSnapshot, dt: Duration) -> Canvas {
        self.render(snapshot, dt).clone()
    }

    /// The already-interpolated [`LiveFrame`] this snapshot produces, without
    /// drawing it.
    ///
    /// The census that keeps PRD §10.1's shape channel and §10.2's colour
    /// channel honest has to count *marks*, not only pixels — "the run glyph
    /// never appears" is a statement about this struct — so the frame the
    /// renderer builds is readable from outside the crate.
    pub fn build_frame(&mut self, snapshot: &WorldSnapshot, dt: Duration) -> LiveFrame {
        self.build(snapshot, dt.as_secs_f64())
    }

    // -----------------------------------------------------------------------
    // Building the frame
    // -----------------------------------------------------------------------

    /// Turns a snapshot into the already-interpolated [`LiveFrame`] the live
    /// layer draws.
    fn build(&mut self, snap: &WorldSnapshot, dt: f64) -> LiveFrame {
        let now = snap.at;
        let layout = snap.layout.as_ref();
        let view = self.base.view;
        let mut frame = LiveFrame {
            unit: self.base.unit,
            map_height: self.opts.pixels as f64,
            ..LiveFrame::default()
        };

        // Which threads get a cloud (PRD §10.4's cap): waiting first, then most
        // recently active, which is the same order the status rail uses.
        let mut ranked: Vec<&Thread> = snap.threads.iter().collect();
        ranked.sort_by(|a, b| {
            a.status
                .rail_rank()
                .cmp(&b.status.rail_rank())
                .then(b.last_activity.cmp(&a.last_activity))
                .then(a.id.cmp(&b.id))
        });

        // The cloud field is the sum over the visible territories: PRD §6.4's
        // "overlap is field addition", which is also the early-warning
        // contention signal, so the fields must not be kept apart.
        let mut any_cloud = false;
        for thread in ranked.iter().take(self.opts.cloud_cap) {
            if thread.territory.claim.is_none() {
                continue;
            }
            for k in &thread.territory.kernels {
                if k.weight <= 0.0 {
                    continue;
                }
                any_cloud = true;
                frame.clouds.push(CloudKernel {
                    at: view.at(k.centre),
                    radius: f64::from(k.radius) * view.scale(),
                    weight: f64::from(k.weight),
                });
            }
        }
        // Presence eases so a territory does not pop into existence when its
        // eighth observation crosses the convergence threshold.
        let target = if any_cloud { 1.0 } else { 0.0 };
        self.cloud_presence = chase(self.cloud_presence, target, dt, CLOUD_RATE);
        for k in &mut frame.clouds {
            k.weight *= self.cloud_presence;
        }

        let mut anchors: BTreeMap<ThreadId, Px> = BTreeMap::new();
        let mut rail = RailBuilder::default();
        for thread in &snap.threads {
            let anchor = thread_anchor(thread, layout, &view);
            if let Some(a) = anchor {
                anchors.insert(thread.id.clone(), a);
            } else {
                // > the thread renders with no cloud — an unplaced marker in
                // > the status rail (PRD §6.2)
                rail.threads.push(thread_label(thread));
            }
            let waiting = thread.status == ThreadStatus::Waiting;

            // --- trail (PRD §12) -------------------------------------------
            let mut steps: Vec<TrailStep> = Vec::with_capacity(thread.trail.len());
            for (path, at) in &thread.trail {
                let Some(p) = place(layout, path, &view) else {
                    continue;
                };
                steps.push(TrailStep {
                    at: p,
                    age: secs_since(now, *at),
                    visits: thread.revisits(path),
                });
            }
            if steps.len() >= 2 {
                frame.trails.push(Trail {
                    steps,
                    ttl: polis_world::TRAIL_TTL.as_secs_f64(),
                });
            }

            // --- thrashing --------------------------------------------------
            //
            // Only for paths still on the trail: a revisit count with no recent
            // touch is history, and history belongs in the drill-down layer.
            let mut seen: BTreeMap<&LogicalPath, (Px, u32, f64)> = BTreeMap::new();
            for (path, at) in &thread.trail {
                let visits = thread.revisits(path);
                if visits < 3 {
                    continue;
                }
                let Some(p) = place(layout, path, &view) else {
                    continue;
                };
                let age = secs_since(now, *at) / polis_world::TRAIL_TTL.as_secs_f64();
                let entry = seen.entry(path).or_insert((p, visits, age));
                entry.2 = entry.2.min(age);
            }
            for (at, visits, age) in seen.into_values() {
                frame.thrash.push(Thrash {
                    at,
                    visits,
                    age: age.clamp(0.0, 1.0),
                });
            }

            // --- operation marks (PRD §10.1, §10.2) -------------------------
            //
            // Every operation gets a place: `place::site_of` is the whole
            // fallback chain, and only its fourth rung — a thread with no
            // position at all — leaves the map, for the status rail below.
            let mut agg: BTreeMap<MarkKey, MarkAgg> = BTreeMap::new();
            for op in &thread.ops {
                let age = secs_since(now, op.at);
                if age > MARK_TTL {
                    continue;
                }
                let site = place::site_of(op, thread, layout);
                let Some(point) = site.point() else {
                    rail.note_op(thread, op);
                    continue;
                };
                let at = view.at(point);
                let key = (
                    at[0].round() as i64,
                    at[1].round() as i64,
                    op.glyph as u8,
                    op.outcome as u8,
                );
                let entry = agg.entry(key).or_insert(MarkAgg {
                    at,
                    glyph: op.glyph,
                    outcome: op.outcome,
                    age,
                    scale: site.scale(),
                    coarse: site.is_coarse(),
                    count: 0,
                });
                entry.count = entry.count.saturating_add(1);
                // The stack ages with its freshest member: a mark that is still
                // being added to has not gone cold.
                entry.age = entry.age.min(age);
            }
            // Failures first, then the freshest. The cap is a contrast budget,
            // and a failed shell command is the most decision-changing mark on
            // the map (PRD §17) — so what a full map drops is successes.
            let mut merged: Vec<MarkAgg> = agg.into_values().collect();
            merged.sort_by(|a, b| {
                outcome_rank(a.outcome)
                    .cmp(&outcome_rank(b.outcome))
                    .then(a.age.total_cmp(&b.age))
            });
            // The cap never truncates a failure. It cannot run away — the world
            // caps `Thread::ops` at `OPS_CAP` and a mark is at least one
            // operation — and a budget that silently drops the red is the
            // failure this whole layer was built to stop.
            let failures = merged
                .iter()
                .filter(|m| m.outcome == polis_events::Outcome::Failed)
                .count();
            merged.truncate(MARKS_PER_THREAD.max(failures));
            // Coarse sites stack by construction — every pathless call of one
            // agent lands on that agent — so each (shape, colour) pair gets a
            // fixed seat on a ring around the position. Fixed, not packed: a
            // seat that moved as counts changed would make the ring twitch
            // every frame.
            let r = live::glyph_radius(frame.unit, frame.map_height);
            let lim = frame.map_height;
            let cap = frame.map_height * COARSE_MARK_CAP;
            for m in &mut merged {
                if !m.coarse {
                    continue;
                }
                // The biggest a mark at this position can be drawn — freshest
                // age, largest stack — is what the seat geometry has to make
                // room for, and it is capped so a close zoom cannot inflate it.
                let widest = live::stack_scale(u32::MAX);
                m.scale = m.scale.min(cap / (r * widest));
                let inside = m.at[0] >= 0.0 && m.at[1] >= 0.0 && m.at[0] <= lim && m.at[1] <= lim;
                let (dir, reach) = seat_of(m.glyph, m.outcome, r, r * m.scale * widest);
                let mut at = [
                    dir[0].mul_add(reach, m.at[0]),
                    dir[1].mul_add(reach, m.at[1]),
                ];
                // A seat may not push a mark off the map that was on it. The
                // offset is a drawing device, so bounding it costs nothing; the
                // position it orbits is the claim, and that is untouched.
                if inside {
                    at = [at[0].clamp(r, lim - r), at[1].clamp(r, lim - r)];
                }
                m.at = at;
            }
            // Drawn in reverse of the budget order, so the failures that won the
            // budget are also the ones on top.
            for m in merged.into_iter().rev() {
                frame.marks.push(Mark {
                    at: m.at,
                    glyph: m.glyph,
                    outcome: m.outcome,
                    age: m.age / MARK_TTL,
                    pulse: pulse(m.age),
                    scale: m.scale,
                    count: m.count,
                });
            }

            // --- the agents themselves --------------------------------------
            let outcome = thread
                .ops
                .back()
                .map_or(polis_events::Outcome::Pending, |o| o.outcome);
            if let Some(a) = anchor {
                let m = self.step_motion((thread.id.clone(), None), a, dt);
                frame.agents.push(Agent {
                    at: m.at(),
                    body: Body::Main,
                    outcome,
                    travel: 1.0 - m.t,
                    heading: m.heading(),
                    waiting,
                });
            }
            // Running workers first, then the most recently active: a tether
            // is a claim about *now*, and a finished worker's is the first to
            // go when there is not room for all of them.
            let mut ranked: Vec<&polis_world::Worker> = thread.workers.iter().collect();
            ranked.sort_by(|a, b| {
                b.running
                    .cmp(&a.running)
                    .then(b.last_activity.cmp(&a.last_activity))
                    .then(a.id.cmp(&b.id))
            });
            for (rank, worker) in ranked.iter().enumerate() {
                let Some(focus) = worker.focus.as_ref() else {
                    continue;
                };
                let Some(p) = place(layout, focus, &view) else {
                    continue;
                };
                let key = (thread.id.clone(), Some(worker.id.clone()));
                let m = self.step_motion(key, p, dt);
                let at = m.at();
                if let Some(a) = anchor {
                    if rank < TETHERS_PER_THREAD {
                        // Fan the bundle: rank 0 bows one way, the last bows the
                        // other, and the count becomes readable.
                        let n = TETHERS_PER_THREAD.min(thread.workers.len()).max(2) - 1;
                        let spread = (rank as f64 / n as f64).mul_add(2.0, -1.0);
                        frame.tethers.push(live::Tether {
                            anchor: a,
                            worker: at,
                            running: worker.running,
                            spread,
                        });
                    }
                }
                let worker_outcome = thread
                    .ops
                    .iter()
                    .rev()
                    .find(|o| o.worker.as_ref() == Some(&worker.id))
                    .map_or(polis_events::Outcome::Pending, |o| o.outcome);
                if worker.running || rank < TETHERS_PER_THREAD {
                    frame.agents.push(Agent {
                        at,
                        body: Body::Worker,
                        outcome: worker_outcome,
                        travel: 1.0 - m.t,
                        heading: m.heading(),
                        waiting: waiting && worker.running,
                    });
                }
            }
        }

        // --- scaffolding: the tweened height delta (PRD §7.3, §8, §13) ------
        for (path, state) in snap.files.iter() {
            let age = state
                .last_touched
                .map_or(f64::INFINITY, |t| secs_since(now, t));
            let target = if state.deleted || age > SCAFFOLD_WINDOW {
                0.0
            } else {
                (f64::from(state.diff_lines) / SCAFFOLD_REFERENCE).clamp(0.0, 1.0)
            };
            let entry = self.rise.entry(path.clone()).or_insert(0.0);
            *entry = chase(*entry, target, dt, RISE_RATE);
            if *entry <= 0.004 {
                continue;
            }
            let Some(building) = layout.buildings.get(path) else {
                continue;
            };
            let at = view.at(building.footprint.centroid());
            let half = footprint_half_width(building, &view);
            frame.scaffolds.push(Scaffold {
                at,
                half_width: half,
                rise: *entry * self.base.unit * 0.62,
                age: (age / SCAFFOLD_WINDOW).clamp(0.0, 1.0),
            });
        }
        // Forget files that have settled, so a long session does not grow a map
        // entry per file it ever touched.
        self.rise.retain(|_, v| *v > 0.004);
        if frame.scaffolds.len() > MAX_SCAFFOLDS {
            frame
                .scaffolds
                .sort_by(|a, b| b.rise.total_cmp(&a.rise).then(a.age.total_cmp(&b.age)));
            frame.scaffolds.truncate(MAX_SCAFFOLDS);
        }

        // --- attention (PRD §11.2) ------------------------------------------
        for mark in &snap.attention {
            if let Some(m) = attention_mark(mark, &anchors, layout, &view, now) {
                frame.attention.push(m);
            }
        }

        // --- the status rail (PRD §6.2) -------------------------------------
        frame.rail = rail.rows(snap.unattributed.len());
        frame
    }

    /// Advances one agent's tween, retargeting it when its destination moved.
    fn step_motion(&mut self, key: AgentKey, target: Px, dt: f64) -> Motion {
        let entry = self.motion.entry(key).or_insert(Motion {
            from: target,
            to: target,
            t: 1.0,
        });
        if (entry.to[0] - target[0]).abs() > 0.5 || (entry.to[1] - target[1]).abs() > 0.5 {
            // Retarget from wherever the agent currently *is*, not from its old
            // origin: an agent redirected mid-flight must not jump backwards.
            entry.from = entry.at();
            entry.to = target;
            entry.t = 0.0;
        }
        if dt > 0.0 {
            entry.t = (entry.t + dt / TRANSIT).min(1.0);
        }
        *entry
    }

    // -----------------------------------------------------------------------
    // Compositing
    // -----------------------------------------------------------------------

    /// PRD §10.3's layer order, bottom to top, with the type between the clouds
    /// and the agents.
    fn compose(&mut self, frame: &LiveFrame, snap: &WorldSnapshot) {
        let w = self.opts.pixels;
        let stride = self.scratch.width;
        // Row by row, because the canvas is wider than the map when the status
        // rail is on. The rail and caption plates are painted first so nothing
        // from the previous frame survives outside the map square.
        for p in self.scratch.pixels.as_chunks_mut::<3>().0 {
            *p = CAPTION_PLATE;
        }
        for y in 0..w {
            let src = y * w * 3;
            let dst = y * stride * 3;
            self.scratch.pixels[dst..dst + w * 3]
                .copy_from_slice(&self.base.map.pixels[src..src + w * 3]);
        }

        let clouds = live::draw_clouds(&mut self.scratch, frame);
        // Layer 3t: the type goes back on top of the clouds (PRD §10.3). The
        // indices are into the *map*, so they are re-strided onto the canvas.
        for (i, colour) in &self.base.labels {
            let i = *i as usize;
            let o = ((i / w) * stride + (i % w)) * 3;
            if o + 3 <= self.scratch.pixels.len() {
                self.scratch.pixels[o..o + 3].copy_from_slice(colour);
            }
        }
        let agents = live::draw_agents(&mut self.scratch, frame, self.opts.trail);
        let attention = live::draw_attention(&mut self.scratch, frame);
        self.timings = LiveTimings {
            clouds,
            agents,
            attention,
        };

        if self.caption_height > 0 {
            self.draw_caption(snap);
        }
        if self.rail_width > 0 {
            self.draw_rail(&frame.rail, snap);
        }
    }

    /// The caption strip: chrome, outside the map frame.
    ///
    /// Held below [`plan::ATTENTION_BAND`] like [`plan`]'s footer, for the same
    /// reason: *"layer 5 owns the top of the range"* has to be true of the whole
    /// image or it is not true at all. So the one thing on this strip that must
    /// catch the eye — a thread waiting on a human — is marked with a **glyph**
    /// rather than with brightness.
    /// PRD §6.2's status rail: the column where everything that has no place on
    /// the map is listed instead.
    ///
    /// > Until then the thread renders with no cloud — an unplaced marker in the
    /// > status rail.
    ///
    /// Its job is to make "nothing was silently dropped" checkable by looking.
    /// The rail is never blank: with nothing unplaced it says so, which is a
    /// different statement from an empty panel and the operator has to be able
    /// to tell them apart.
    ///
    /// Chrome, so it is held below [`plan::ATTENTION_BAND`] like the caption —
    /// except for the failure count, which is drawn in [`live::AGENT_FAILED`]
    /// (top channel 168, the agent band's ceiling) and paired with the failed
    /// operation's own glyph, so it is never a colour-only signal (PRD §11.4).
    fn draw_rail(&mut self, rows: &[live::RailRow], snap: &WorldSnapshot) {
        let x0 = self.opts.pixels as f64;
        let w = self.rail_width as f64;
        let h = (self.opts.pixels + self.caption_height) as f64;
        let pad = w * 0.06;
        // Sized to fit RAIL_COLUMNS characters across, because a rail whose
        // header runs off its own edge is worse than no rail.
        let size = ((w - pad * 2.0) / (RAIL_COLUMNS * 6.0)).max(1.0);
        let line = size * 7.0 * 1.7;

        self.scratch.rect(x0, 0.0, x0 + w, h, CAPTION_PLATE, 1.0);
        self.scratch
            .rect(x0, 0.0, x0 + w * 0.018, h, CAPTION_RULE, 1.0);

        let ops: u32 = rows
            .iter()
            .filter(|r| r.kind == live::RailKind::UnplacedOps)
            .map(|r| r.count)
            .sum();
        let failed: u32 = rows.iter().map(|r| r.failed).sum();
        let mut y = pad;
        self.scratch
            .text(x0 + pad, y, "STATUS RAIL", size, CAPTION_HEAD);
        y += line;
        // The second line is the **live** state; the footer is the running total
        // since the session started. They are different numbers and the rail
        // says which is which, because "0 unplaced now" and "18 unplaced ever"
        // are both true and only one of them is actionable.
        let head = if rows.is_empty() {
            "NOW: ALL PLACED".to_owned()
        } else if failed > 0 {
            format!("NOW {ops}, {failed} FAIL")
        } else {
            format!("NOW {ops} UNPLACED")
        };
        self.scratch.text(
            x0 + pad,
            y,
            &head,
            size,
            if failed > 0 {
                live::AGENT_FAILED
            } else {
                CAPTION_TEXT
            },
        );
        y += line * 1.35;

        let r = (size * 5.0 * 0.42).max(2.0);
        for row in rows {
            if y + line * 2.0 > h - pad {
                break;
            }
            // The glyph is the row's own operation shape when there is one to
            // show, so a red count always arrives with a shape beside it.
            let ink = if row.failed > 0 {
                live::AGENT_FAILED
            } else {
                CAPTION_TEXT
            };
            live::draw_glyph(
                &mut self.scratch,
                [x0 + pad + r, y + size * 3.5],
                r,
                match row.kind {
                    live::RailKind::UnplacedThread => polis_events::Glyph::HollowCircle,
                    live::RailKind::UnplacedOps => polis_events::Glyph::FilledTriangle,
                    live::RailKind::UnattributedWorkers => polis_events::Glyph::Delegate,
                },
                ink,
            );
            let count = if row.failed > 0 {
                format!("{} ({} FAIL)", row.count, row.failed)
            } else {
                row.count.to_string()
            };
            self.scratch.text(x0 + pad + r * 2.5, y, &count, size, ink);
            y += line * 0.85;
            let width = ((w - pad * 2.0) / (6.0 * size * 0.85)) as usize;
            debug_assert!(width >= 4);
            let label: String = row
                .label
                .chars()
                .take(width.max(4))
                .collect::<String>()
                .to_uppercase();
            self.scratch
                .text(x0 + pad, y, &label, size * 0.85, CAPTION_DIM);
            y += line * 0.75;
            self.scratch
                .text(x0 + pad, y, row.kind.label(), size * 0.85, CAPTION_DIM);
            y += line * 1.1;
        }

        // The bottom of the rail carries the counters that say whether the map
        // above it is complete — the health numbers `place` maintains.
        let census = snap.health.ops;
        let foot = [
            "SINCE START".to_owned(),
            format!("PLACED {}", census.placed()),
            format!("RAILED {}", census.rail),
            format!("FAILED {}", snap.health.ops_failed.total()),
        ];
        let mut y = h - pad - line * foot.len() as f64;
        for text in foot {
            self.scratch
                .text(x0 + pad, y, &text, size * 0.85, CAPTION_DIM);
            y += line;
        }
    }

    fn draw_caption(&mut self, snap: &WorldSnapshot) {
        let w = self.opts.pixels as f64;
        let y0 = self.opts.pixels as f64;
        let h = self.caption_height as f64;
        let pad = w * 0.014;
        // `Canvas::text` takes pixels **per font pixel**, and the font is seven
        // rows tall — so a "size" of `h * 0.17` draws a glyph a whole caption
        // strip high. Ask for the cap height and divide by the font's.
        let size = (h * 0.16 / 7.0).max(1.0);
        let line_h = size * 7.0;

        let mut waiting = 0;
        let mut working = 0;
        for t in &snap.threads {
            match t.status {
                ThreadStatus::Waiting => waiting += 1,
                ThreadStatus::Working => working += 1,
                _ => {}
            }
        }
        let workers: usize = snap.threads.iter().map(|t| t.workers.len()).sum();
        let running: usize = snap
            .threads
            .iter()
            .map(|t| t.workers.iter().filter(|w| w.running).count())
            .sum();
        let touched = snap.files.values().filter(|f| f.diff_lines > 0).count();

        self.scratch.rect(0.0, y0, w, y0 + h, CAPTION_PLATE, 1.0);
        self.scratch
            .rect(0.0, y0, w, y0 + h * 0.04, CAPTION_RULE, 1.0);

        let label = self.label.clone();
        let stats = format!(
            "{} THREAD{} / {waiting} WAITING / {working} WORKING / {running} OF {workers} WORKERS RUNNING",
            snap.threads.len(),
            if snap.threads.len() == 1 { "" } else { "S" }
        );
        let files = format!(
            "{} FILES / {touched} UNDER DIFF / {} ATTENTION / GEN {}",
            snap.files.len(),
            snap.attention.len(),
            snap.generation
        );
        let rows = [
            (label.as_str(), size * 1.2, CAPTION_HEAD),
            (stats.as_str(), size, CAPTION_TEXT),
            (files.as_str(), size, CAPTION_DIM),
        ];
        let mut y = y0 + h * 0.10;
        let mut left_edge: f64 = 0.0;
        for (text, s, ink) in rows {
            self.scratch.text(pad, y, text, s, ink);
            left_edge = left_edge.max(pad + Canvas::text_width(text, s));
            y += line_h * 1.35;
        }

        // The legend, right of whatever the text took. Shape is the thing the
        // operator has to learn, so it is drawn rather than named — and it is
        // skipped entirely when the strip is too narrow to hold it, because a
        // legend colliding with the readout is worse than no legend.
        let legend_x = left_edge + pad * 2.0;
        let r = (line_h * 0.42).max(2.0);
        let mut x = legend_x;
        for (glyph, name) in [
            (polis_events::Glyph::HollowCircle, "READ"),
            (polis_events::Glyph::BarredCircle, "EDIT"),
            (polis_events::Glyph::FilledSquare, "WRITE"),
            (polis_events::Glyph::FilledTriangle, "RUN"),
            (polis_events::Glyph::ConcentricCircles, "TEST"),
            (polis_events::Glyph::Delegate, "TASK"),
        ] {
            let tw = Canvas::text_width(name, size * 0.85);
            if x + r * 2.0 + tw > w - pad {
                break;
            }
            let cy = y0 + h * 0.32;
            live::draw_glyph(
                &mut self.scratch,
                [x + r, cy],
                r,
                glyph,
                live::AGENT_PENDING,
            );
            self.scratch.text(
                x + r * 2.0 + pad * 0.3,
                cy - size * 0.85 * 3.5,
                name,
                size * 0.85,
                CAPTION_DIM,
            );
            x += r * 2.0 + tw + pad;
        }
        let mut x = legend_x;
        // Failed first. The legend is truncated from the right when the strip is
        // narrow, and the one colour the operator must be able to name is the
        // one that changes a decision (PRD §17).
        for (ink, name) in [
            (live::AGENT_FAILED, "FAILED"),
            (live::AGENT_DONE, "DONE"),
            (live::AGENT_PENDING, "PENDING"),
        ] {
            let tw = Canvas::text_width(name, size * 0.85);
            if x + r * 2.0 + tw > w - pad {
                break;
            }
            let cy = y0 + h * 0.68;
            self.scratch
                .rect(x, cy - r * 0.7, x + r * 1.4, cy + r * 0.7, ink, 1.0);
            self.scratch.text(
                x + r * 2.0 + pad * 0.3,
                cy - size * 0.85 * 3.5,
                name,
                size * 0.85,
                CAPTION_DIM,
            );
            x += r * 2.0 + tw + pad;
        }
    }
}

/// Caption plate. Near-black, and outside the map frame.
const CAPTION_PLATE: Rgb = [7, 8, 10];
/// The rule separating the map from its caption.
const CAPTION_RULE: Rgb = [26, 28, 32];
/// Caption heading — the brightest chrome, and still below
/// [`plan::ATTENTION_BAND`].
const CAPTION_HEAD: Rgb = [150, 156, 164];
/// Caption body.
const CAPTION_TEXT: Rgb = [118, 124, 136];
/// Caption secondary.
const CAPTION_DIM: Rgb = [86, 92, 102];

// ---------------------------------------------------------------------------
// Placement
// ---------------------------------------------------------------------------

/// A path's position on the map: the building, else the district, else nowhere.
///
/// The "else nowhere" is load-bearing. A tool call on a path outside the
/// repository — a scratchpad, `~/.claude`, an absolute Windows path — has no
/// geometry, and inventing one would put activity where nothing is happening,
/// which is the exact failure PRD §6 opens by rejecting. Measured on a real
/// 26-hour session, 736 of the paths touched are outside the repository.
#[must_use]
pub fn place(layout: &CityLayout, path: &LogicalPath, view: &View) -> Option<Px> {
    position_of(layout, path).map(|p| view.at(p))
}

/// The city-space position of a path, matching `polis_world::World::position_of`.
#[must_use]
pub fn position_of(layout: &CityLayout, path: &LogicalPath) -> Option<Point> {
    place::position_in(layout, path)
}

/// Where a thread's main agent is drawn.
///
/// > A main agent has no meaningful point location — it delegates rather than
/// > edits. Computing a centroid of its workers is actively wrong. (PRD §6)
///
/// So the territory's centre of mass comes first: it is a property of the
/// density field rather than a mean of positions. Only when there is no
/// territory yet does the thread fall back to its own most recent step, and a
/// thread with neither is **unplaced** — no cloud, no anchor, and a row in the
/// status rail, exactly as PRD §6.2 asks.
#[must_use]
pub fn thread_anchor(thread: &Thread, layout: &CityLayout, view: &View) -> Option<Px> {
    place::thread_position(thread, layout).map(|p| view.at(p))
}

fn attention_mark(
    mark: &Attention,
    anchors: &BTreeMap<ThreadId, Px>,
    layout: &CityLayout,
    view: &View,
    now: Instant,
) -> Option<AttentionMark> {
    let pulse = f64::from(mark.pulse(now));
    let weight = f64::from(mark.weight(now));
    match &mark.kind {
        AttentionKind::NeedsDecision { thread, at, .. } => {
            let p = at
                .as_ref()
                .and_then(|path| place(layout, path, view))
                .or_else(|| anchors.get(thread).copied())?;
            Some(AttentionMark {
                kind: MarkKind::NeedsDecision,
                at: p,
                other: None,
                severity: None,
                pulse,
                weight,
            })
        }
        AttentionKind::Done { thread, verified } => {
            let p = anchors.get(thread).copied()?;
            Some(AttentionMark {
                kind: if *verified {
                    MarkKind::DoneVerified
                } else {
                    MarkKind::DoneUnverified
                },
                at: p,
                other: None,
                severity: None,
                pulse,
                weight,
            })
        }
        AttentionKind::Contention(c) => {
            // A relation, so it needs two places. When a thread has no anchor
            // the contended file itself stands in — the operator still has to be
            // shown where the collision is.
            let path = place(layout, c.path(), view);
            let (a, b) = c.threads();
            let pa = anchors.get(a).copied().or(path)?;
            let pb = anchors.get(b).copied().or(path)?;
            Some(AttentionMark {
                kind: MarkKind::Contention,
                at: pa,
                other: Some(pb),
                severity: Some(c.severity),
                pulse,
                weight,
            })
        }
    }
}

/// Half the footprint's width in pixels, for a scaffold that matches its
/// building.
fn footprint_half_width(building: &polis_layout::Building, view: &View) -> f64 {
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for v in &building.footprint.vertices {
        lo = lo.min(f64::from(v.x));
        hi = hi.max(f64::from(v.x));
    }
    if !lo.is_finite() {
        return 2.0;
    }
    ((hi - lo) * view.scale() * 0.5).max(1.5)
}

/// Seconds between two instants, clamped at zero.
fn secs_since(now: Instant, then: Instant) -> f64 {
    now.saturating_duration_since(then).as_secs_f64()
}

/// PRD §11.4's arrival pulse: 1 at onset, 0 at 400 ms.
fn pulse(age_secs: f64) -> f64 {
    if age_secs >= live::PULSE_SECS {
        0.0
    } else {
        1.0 - age_secs / live::PULSE_SECS
    }
}

/// An exponential-shaped chase with no transcendental in it.
///
/// `dt * rate / (1 + dt * rate)` is the rational approximation of
/// `1 - exp(-rate * dt)`: monotone, framerate-stable enough for an animation,
/// bounded in `[0, 1)` for every non-negative `dt`, and — the reason it is used
/// — a polynomial, so a frame is byte-identical on every libm (PRD §7.4).
fn chase(current: f64, target: f64, dt: f64, rate: f64) -> f64 {
    if dt <= 0.0 {
        return current;
    }
    let k = dt * rate / dt.mul_add(rate, 1.0);
    (target - current).mul_add(k, current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use polis_events::{
        Channel, Event, EventMeta, Glyph, OtelEvent, Outcome, Payload, SessionId, ToolCall,
        ToolKind, ToolUseId, WorkerId, WorktreeId,
    };
    use polis_layout::city;
    use polis_repo::{synthetic, RepoTree};
    use polis_world::snapshot;
    use polis_world::World;

    use crate::gif;

    fn small_city() -> City {
        city::generate_city(&synthetic::repository(240, 0x51))
    }

    fn lp(s: &str) -> LogicalPath {
        LogicalPath::new(s).expect("path")
    }

    /// One OTel `tool_result`, which is the channel that carries the firehose.
    fn tool_result(
        session: &str,
        worker: Option<&str>,
        tool: ToolKind,
        path: &LogicalPath,
        outcome: Outcome,
        at: Instant,
    ) -> Event {
        let mut meta = EventMeta::now(Channel::Otel).with_session(SessionId::new(session));
        meta.observed = at;
        meta.worker = worker.map(WorkerId::new);
        let call = ToolCall {
            tool,
            tool_use_id: Some(ToolUseId::new("toolu_1")),
            paths: vec![(WorktreeId::PRIMARY, path.clone())],
            outcome,
            duration_ms: Some(4.0),
        };
        Event::new(
            meta,
            Payload::Otel(Box::new(OtelEvent::ToolResult(Box::new(call)))),
        )
    }

    /// A world whose one thread has walked nine real buildings, one of them six
    /// times, so there is a trail, a revisit count and a spread of outcomes.
    fn populated(city: &City) -> World {
        let mut world = World::new(RepoTree::default(), city.layout.clone());
        let t0 = Instant::now();
        let paths: Vec<LogicalPath> = city.layout.buildings.keys().take(9).cloned().collect();
        let mut at = t0;
        for (i, path) in paths.iter().enumerate() {
            let tool = match i % 4 {
                0 => ToolKind::Read,
                1 => ToolKind::Edit,
                2 => ToolKind::Write,
                _ => ToolKind::Bash,
            };
            let outcome = match i % 3 {
                0 => Outcome::Done,
                1 => Outcome::Pending,
                _ => Outcome::Failed,
            };
            let worker = if i % 3 == 0 { Some("agent-1") } else { None };
            world.apply(&tool_result("s", worker, tool, path, outcome, at));
            at += Duration::from_secs(2);
        }
        // Thrashing: one building hit six times (PRD §12's example, literally).
        for _ in 0..5 {
            world.apply(&tool_result(
                "s",
                None,
                ToolKind::Edit,
                &paths[0],
                Outcome::Done,
                at,
            ));
            at += Duration::from_secs(1);
        }
        world.tick(at);
        world
    }

    #[test]
    fn a_frame_is_the_base_map_plus_a_live_layer() {
        let c = small_city();
        let mut r = FrameRenderer::new(
            &c,
            FrameOptions {
                pixels: 420,
                supersample: 1,
                caption: false,
                rail: false,
                ..FrameOptions::default()
            },
        );
        assert_eq!(r.size(), (420, 420));
        let world = populated(&c);
        let (_, reader) = snapshot::from_world(&world);
        let snap = reader.load();
        let canvas = r.render(&snap, Duration::from_millis(33));
        assert_eq!(canvas.width, 420);
        assert_eq!(canvas.height, 420);
        let live_px = canvas
            .pixels
            .as_chunks::<3>()
            .0
            .iter()
            .filter(|p| p.iter().copied().max().unwrap_or(0) > plan::TYPE_BAND.1)
            .count();
        assert!(live_px > 0, "nothing live was drawn");
        let share = live_px as f64 / (420.0 * 420.0);
        assert!(
            share < 0.10,
            "the live layer inked {:.1}% of the map; the city is gone under it",
            share * 100.0
        );
    }

    /// The claim that makes the whole layering scheme worth having: the city is
    /// still there, and the live layer is provably on top of it.
    #[test]
    fn the_base_map_is_untouched_everywhere_the_live_layer_did_not_draw() {
        let c = small_city();
        let opts = FrameOptions {
            pixels: 420,
            supersample: 1,
            caption: false,
            rail: false,
            ..FrameOptions::default()
        };
        let mut r = FrameRenderer::new(&c, opts);
        // The reference is the base map **with its type stamped**, because the
        // label plates are layer-2 ink and darkening the ground is what a plate
        // is for. Comparing against the bare map would charge the live layer for
        // typography it did not draw.
        let mut base = r.base.map.clone();
        let label_at: std::collections::BTreeSet<u32> =
            r.base.labels.iter().map(|(i, _)| *i).collect();
        for (i, colour) in &r.base.labels {
            let o = *i as usize * 3;
            base.pixels[o..o + 3].copy_from_slice(colour);
        }
        let world = populated(&c);
        let (_, reader) = snapshot::from_world(&world);
        let snap = reader.load();
        let frame = r.render(&snap, Duration::from_millis(33)).clone();

        let luma = |p: &[u8; 3]| {
            0.2126f64.mul_add(
                f64::from(p[0]),
                0.7152f64.mul_add(f64::from(p[1]), 0.0722 * f64::from(p[2])),
            )
        };
        let mut changed = 0usize;
        let mut darkened = 0usize;
        let mut cleared_the_ceiling = 0usize;
        for (i, (b, f)) in base
            .pixels
            .as_chunks::<3>()
            .0
            .iter()
            .zip(frame.pixels.as_chunks::<3>().0.iter())
            .enumerate()
        {
            if b == f || label_at.contains(&(i as u32)) {
                continue;
            }
            changed += 1;
            if f.iter().copied().max().unwrap_or(0) > plan::BASE_MAP_CEILING {
                cleared_the_ceiling += 1;
            }
            if luma(f) < luma(b) - 1e-9 {
                darkened += 1;
            }
        }
        assert!(changed > 0);
        // **The live layer never subtracts.** Every ink it uses is brighter than
        // anything the base map is allowed to be, and every blend is that ink
        // *over* the map, so a live pixel can only be lighter than what it
        // covered. This is what makes "the storm gets the ink" true pixel by
        // pixel rather than on average — and it is a consequence of the ceiling,
        // not a thing that had to be arranged.
        assert_eq!(
            darkened, 0,
            "{darkened} of {changed} live pixels made the map darker"
        );
        // Roughly half the live layer's pixels are the *cores* of its marks and
        // land in their own band outright. The rest are the antialiased edges of
        // two-pixel strokes, and a coverage-antialiased edge is a fraction of a
        // mark by construction — it can be a soft silhouette or it can be
        // in-band, not both.
        let share = cleared_the_ceiling as f64 / changed as f64;
        assert!(
            share > 0.40,
            "only {:.1}% of live pixels cleared the base map's ceiling",
            share * 100.0
        );
    }

    /// PRD §13: *"tween agent positions … it is the entire difference between
    /// 'alive' and 'steppy'."* The assertion is the literal one: between the
    /// event that moved an agent and its arrival, the agent is somewhere in
    /// between.
    #[test]
    fn an_agent_travels_between_buildings_instead_of_teleporting() {
        let c = small_city();
        let mut r = FrameRenderer::new(
            &c,
            FrameOptions {
                pixels: 400,
                supersample: 1,
                caption: false,
                ..FrameOptions::default()
            },
        );
        let paths: Vec<LogicalPath> = c.layout.buildings.keys().take(2).cloned().collect();
        let view = *r.view();
        let a = place(&c.layout, &paths[0], &view).expect("a");
        let b = place(&c.layout, &paths[1], &view).expect("b");
        let key = (ThreadId::of_session(SessionId::new("s")), None);

        let m0 = r.step_motion(key.clone(), a, 0.0);
        assert_eq!(
            m0.at(),
            a,
            "the first sighting must not animate from nowhere"
        );
        let m1 = r.step_motion(key.clone(), b, TRANSIT / 3.0);
        let mid = m1.at();
        let far_from_a = (mid[0] - a[0]).abs() + (mid[1] - a[1]).abs();
        let far_from_b = (mid[0] - b[0]).abs() + (mid[1] - b[1]).abs();
        assert!(
            far_from_a > 0.5 && far_from_b > 0.5,
            "agent teleported: {mid:?} between {a:?} and {b:?}"
        );
        assert!(m1.t > 0.0 && m1.t < 1.0);
        let m2 = r.step_motion(key, b, TRANSIT);
        assert_eq!(m2.t, 1.0);
        assert!((m2.at()[0] - b[0]).abs() < 1e-9);
    }

    /// The two clocks. Tweens run on presentation time, so a replay at 64× must
    /// not make agents cross the map 64 times faster.
    #[test]
    fn the_tween_ignores_how_fast_the_session_is_replaying() {
        let c = small_city();
        let mut r = FrameRenderer::new(&c, FrameOptions::default());
        let key = (ThreadId::of_session(SessionId::new("s")), None);
        r.step_motion(key.clone(), [10.0, 10.0], 0.0);
        let m = r.step_motion(key, [200.0, 200.0], 0.016);
        assert!((m.t - 0.016 / TRANSIT).abs() < 1e-12);
    }

    /// The renderer is a pure function of (snapshot, dt sequence).
    #[test]
    fn the_same_sequence_renders_the_same_bytes() {
        let c = small_city();
        let world = populated(&c);
        let (_, reader) = snapshot::from_world(&world);
        let snap = reader.load();
        let run = || {
            let mut r = FrameRenderer::new(
                &c,
                FrameOptions {
                    pixels: 300,
                    supersample: 1,
                    ..FrameOptions::default()
                },
            );
            r.set_label("T+00:00:07");
            let mut out = Vec::new();
            for _ in 0..6 {
                out.push(r.render_owned(&snap, Duration::from_millis(40)).pixels);
            }
            out
        };
        assert_eq!(run(), run());
    }

    /// PRD §17 open question 3, set up so it can be answered with two images of
    /// the *same* moment rather than two moments.
    #[test]
    fn the_same_moment_renders_under_both_trail_notations() {
        let c = small_city();
        let world = populated(&c);
        let (_, reader) = snapshot::from_world(&world);
        let snap = reader.load();
        let shot = |style| {
            let mut r = FrameRenderer::new(
                &c,
                FrameOptions {
                    pixels: 320,
                    supersample: 1,
                    caption: false,
                    trail: style,
                    ..FrameOptions::default()
                },
            );
            r.render_owned(&snap, Duration::ZERO).pixels
        };
        assert_ne!(shot(TrailStyle::Fade), shot(TrailStyle::Timed));
    }

    /// A path with no geometry contributes nothing rather than landing at the
    /// origin. Measured on a real session, 736 touched paths are outside the
    /// repository, and every one of them at `[0, 0]` would be a bright pile in
    /// the corner of the map.
    #[test]
    fn a_path_outside_the_repository_is_not_placed() {
        let c = small_city();
        let view = View::fit([0.0, 0.0], [10.0, 10.0], 100, 100, 2.0);
        assert!(place(&c.layout, &lp("does/not/exist.rs"), &view).is_none());
    }

    /// Scaffolding rises smoothly rather than snapping, and comes back down.
    #[test]
    fn a_growing_file_raises_scaffolding_and_a_settled_one_drops_it() {
        let mut v = 0.0;
        for _ in 0..3 {
            v = chase(v, 1.0, 0.05, RISE_RATE);
        }
        assert!(v > 0.0 && v < 1.0, "rise snapped to {v}");
        let mut fall = 1.0;
        for _ in 0..200 {
            fall = chase(fall, 0.0, 0.05, RISE_RATE);
        }
        assert!(fall < 0.01);
        // The chase is monotone and never overshoots, whatever the step size —
        // a dropped frame must not make a scaffold jump past its target.
        assert!(chase(0.0, 1.0, 100.0, RISE_RATE) < 1.0);
        assert_eq!(chase(0.4, 0.9, 0.0, RISE_RATE), 0.4);
    }

    /// The frame budget, measured on the real composite rather than on the live
    /// layer alone: PRD §13.1 gives layers 4 and 5 under 4 ms.
    #[test]
    fn the_live_layer_holds_the_frame_budget_on_a_real_city() {
        let c = small_city();
        let mut r = FrameRenderer::new(&c, FrameOptions::default());
        let world = populated(&c);
        let (_, reader) = snapshot::from_world(&world);
        let snap = reader.load();
        r.render(&snap, Duration::from_millis(16));
        r.render(&snap, Duration::from_millis(16));
        let t = r.timings();
        println!(
            "frame: clouds {:?}, agents {:?}, attention {:?}, budgeted {:?}",
            t.clouds,
            t.agents,
            t.attention,
            t.budgeted()
        );
        let budget = if cfg!(debug_assertions) {
            Duration::from_millis(80)
        } else {
            Duration::from_millis(4)
        };
        assert!(t.budgeted() <= budget, "{:?} over {budget:?}", t.budgeted());
    }

    /// The caption is chrome and must stay out of layer 5's band, or "attention
    /// owns the top of the range" stops being true of the image.
    #[test]
    fn the_caption_strip_stays_below_the_attention_band() {
        let c = small_city();
        let mut r = FrameRenderer::new(
            &c,
            FrameOptions {
                pixels: 400,
                supersample: 1,
                ..FrameOptions::default()
            },
        );
        r.set_label("T+01:23:45");
        let world = populated(&c);
        let (_, reader) = snapshot::from_world(&world);
        let snap = reader.load();
        let frame = r.render(&snap, Duration::from_millis(16)).clone();
        // Everything outside the 400x400 map square: the caption strip below it
        // and the status rail to its right. Scanned by coordinate, because the
        // canvas is wider than the map once the rail is on.
        let (w, h) = (frame.width, frame.height);
        let mut inked = 0usize;
        let mut chrome = 0usize;
        for y in 0..h {
            for x in 0..w {
                if x < 400 && y < 400 {
                    continue;
                }
                chrome += 1;
                let o = (y * w + x) * 3;
                let p: [u8; 3] = [frame.pixels[o], frame.pixels[o + 1], frame.pixels[o + 2]];
                let head = p.iter().copied().max().unwrap_or(0);
                assert!(head < plan::ATTENTION_BAND.0, "chrome pixel at {head}");
                if p != CAPTION_PLATE {
                    inked += 1;
                }
            }
        }
        assert!(chrome > 400 * 48, "the chrome area came out at {chrome} px");
        assert!(inked > 200, "the chrome drew {inked} px");
    }

    /// A mark with an unresolvable position is dropped, not stacked at `[0, 0]`
    /// — including through the attention path, which has its own fallbacks.
    #[test]
    fn an_attention_mark_with_no_position_is_dropped() {
        let c = small_city();
        let view = View::fit([0.0, 0.0], [10.0, 10.0], 100, 100, 2.0);
        let mark = Attention::new(
            AttentionKind::NeedsDecision {
                thread: ThreadId::of_session(SessionId::new("ghost")),
                at: Some(lp("nowhere/at/all.rs")),
                source: polis_world::attention::DecisionSource::PermissionRequest,
            },
            Instant::now(),
        );
        assert!(
            attention_mark(&mark, &BTreeMap::new(), &c.layout, &view, Instant::now()).is_none()
        );
    }

    /// PRD §12's thrashing example, end to end: a building touched six times
    /// gets a mark that a building touched once does not.
    #[test]
    fn a_building_revisited_six_times_is_marked_as_thrashed() {
        let c = small_city();
        let mut r = FrameRenderer::new(
            &c,
            FrameOptions {
                pixels: 400,
                supersample: 1,
                caption: false,
                ..FrameOptions::default()
            },
        );
        let world = populated(&c);
        let (_, reader) = snapshot::from_world(&world);
        let snap = reader.load();
        let frame = r.build(&snap, 0.016);
        assert!(
            !frame.thrash.is_empty(),
            "no revisit was marked, though one building was touched six times"
        );
        let worst = frame
            .thrash
            .iter()
            .map(|t| t.visits)
            .max()
            .unwrap_or_default();
        assert!(worst >= 6, "the worst revisit count came out as {worst}");
    }

    /// Every mark, trail step and agent lands inside the map area. A live mark
    /// drawn over the caption strip would be a mark the operator reads as
    /// chrome.
    #[test]
    fn nothing_live_is_placed_outside_the_map_frame() {
        let c = small_city();
        let mut r = FrameRenderer::new(&c, FrameOptions::default());
        let world = populated(&c);
        let (_, reader) = snapshot::from_world(&world);
        let snap = reader.load();
        let frame = r.build(&snap, 0.016);
        let h = frame.map_height;
        let inside = |p: Px| p[0] >= -1.0 && p[1] >= -1.0 && p[0] <= h + 1.0 && p[1] <= h + 1.0;
        for m in &frame.marks {
            assert!(inside(m.at), "mark at {:?}", m.at);
        }
        for a in &frame.agents {
            assert!(inside(a.at), "agent at {:?}", a.at);
        }
        for t in &frame.trails {
            for s in &t.steps {
                assert!(inside(s.at), "trail step at {:?}", s.at);
            }
        }
    }

    /// A world whose thread runs shell commands and calls pathless tools, which
    /// is what a real session mostly does: 6 742 shell calls against 1 104
    /// edits in one recorded run.
    fn shelling(city: &City) -> World {
        let mut world = World::new(RepoTree::default(), city.layout.clone());
        let t0 = Instant::now();
        let path = city
            .layout
            .buildings
            .keys()
            .next()
            .expect("a city has buildings")
            .clone();
        let mut at = t0;
        // One placeable step, so the thread has a position at all.
        world.apply(&tool_result(
            "s",
            None,
            ToolKind::Edit,
            &path,
            Outcome::Done,
            at,
        ));
        at += Duration::from_secs(1);
        // Then twelve shell calls with no path of their own, four of them
        // failing. On the old rule not one of these reached the map.
        for i in 0..12 {
            let outcome = if i % 3 == 0 {
                Outcome::Failed
            } else {
                Outcome::Done
            };
            let mut meta = EventMeta::now(Channel::Otel).with_session(SessionId::new("s"));
            meta.observed = at;
            let call = ToolCall {
                tool: ToolKind::PowerShell,
                tool_use_id: Some(ToolUseId::new(format!("toolu_sh_{i}"))),
                paths: Vec::new(),
                outcome,
                duration_ms: Some(4.0),
            };
            world.apply(&Event::new(
                meta,
                Payload::Otel(Box::new(OtelEvent::ToolResult(Box::new(call)))),
            ));
            at += Duration::from_secs(1);
        }
        world.tick(at);
        world
    }

    /// The defect this whole layer exists to fix: a shell call carries no file
    /// path, and on the old rule that meant no mark — which hid 69 % of all real
    /// tool failures and the single most-used tool.
    #[test]
    fn a_shell_call_with_no_path_is_still_drawn_and_a_failed_one_is_red() {
        let c = small_city();
        let mut r = FrameRenderer::new(&c, FrameOptions::default());
        let world = shelling(&c);
        let (_, reader) = snapshot::from_world(&world);
        let snap = reader.load();
        let frame = r.build(&snap, 0.016);

        let runs: Vec<&Mark> = frame
            .marks
            .iter()
            .filter(|m| m.glyph == polis_events::Glyph::FilledTriangle)
            .collect();
        assert!(
            !runs.is_empty(),
            "the run glyph never appeared, though the thread ran twelve commands"
        );
        let failed: u32 = runs
            .iter()
            .filter(|m| m.outcome == Outcome::Failed)
            .map(|m| m.count)
            .sum();
        assert_eq!(failed, 4, "every failed run has to reach the map");
        let done: u32 = runs
            .iter()
            .filter(|m| m.outcome == Outcome::Done)
            .map(|m| m.count)
            .sum();
        assert_eq!(done, 8);
        // Aggregated, not repeated: twelve calls at one position become two
        // marks, because a failure may never be merged into a success.
        assert_eq!(runs.len(), 2, "shell volume was not aggregated: {runs:?}");
        // And their positions differ, because each (shape, colour) pair has its
        // own seat around the agent.
        assert_ne!(runs[0].at, runs[1].at);
        // Coarser position, smaller mark — never a different shape or colour.
        assert!(runs.iter().all(|m| m.scale < 1.0));
    }

    /// Nothing may be silently dropped. A thread with no position at all has no
    /// map to draw on, so its operations are counted in the status rail.
    #[test]
    fn an_operation_with_nowhere_to_go_lands_in_the_status_rail() {
        let c = small_city();
        let mut r = FrameRenderer::new(&c, FrameOptions::default());
        // A world over an **empty** city: nothing has geometry, so no rung of
        // the chain can resolve and every operation is rung 4.
        let mut world = World::new(RepoTree::default(), polis_layout::CityLayout::default());
        let at = Instant::now();
        let mut meta = EventMeta::now(Channel::Otel).with_session(SessionId::new("ghost"));
        meta.observed = at;
        world.apply(&Event::new(
            meta,
            Payload::Otel(Box::new(OtelEvent::ToolResult(Box::new(ToolCall {
                tool: ToolKind::Bash,
                tool_use_id: Some(ToolUseId::new("toolu_x")),
                paths: Vec::new(),
                outcome: Outcome::Failed,
                duration_ms: None,
            })))),
        ));
        world.tick(at);
        let (_, reader) = snapshot::from_world(&world);
        let snap = reader.load();
        let frame = r.build(&snap, 0.016);
        assert!(frame.marks.is_empty(), "nothing could be placed");
        let ops: u32 = frame
            .rail
            .iter()
            .filter(|row| row.kind == live::RailKind::UnplacedOps)
            .map(|row| row.count)
            .sum();
        assert_eq!(
            ops, 1,
            "the unplaced operation was dropped: {:?}",
            frame.rail
        );
        let failed: u32 = frame.rail.iter().map(|row| row.failed).sum();
        assert_eq!(failed, 1, "the rail lost the failure");
        assert!(
            frame
                .rail
                .iter()
                .any(|row| row.kind == live::RailKind::UnplacedThread),
            "a thread with no position is itself an unplaced marker (PRD §6.2)"
        );
        // And it reaches pixels: the rail is chrome, and chrome that draws
        // nothing is indistinguishable from a rail with nothing in it.
        let map = r.map_pixels();
        let canvas = r.render(&snap, Duration::from_millis(16));
        let (w, h) = (canvas.width, canvas.height);
        assert!(w > map, "the rail has no column to draw in");
        let inked = (0..h)
            .flat_map(|y| (map..w).map(move |x| (x, y)))
            .filter(|(x, y)| {
                let o = (y * w + x) * 3;
                [canvas.pixels[o], canvas.pixels[o + 1], canvas.pixels[o + 2]] != CAPTION_PLATE
            })
            .count();
        assert!(inked > 100, "the status rail drew {inked} px");
    }

    /// Every operation the world holds ends up somewhere: on the map, or in the
    /// rail. The conservation law this layer is for.
    #[test]
    fn no_operation_is_lost_between_the_world_and_the_frame() {
        let c = small_city();
        let mut r = FrameRenderer::new(&c, FrameOptions::default());
        let world = shelling(&c);
        let (_, reader) = snapshot::from_world(&world);
        let snap = reader.load();
        let frame = r.build(&snap, 0.016);
        let held: usize = snap.threads.iter().map(|t| t.ops.len()).sum();
        let drawn: u32 = frame.marks.iter().map(|m| m.count).sum();
        let railed: u32 = frame
            .rail
            .iter()
            .filter(|row| row.kind == live::RailKind::UnplacedOps)
            .map(|row| row.count)
            .sum();
        assert_eq!(
            drawn + railed,
            u32::try_from(held).unwrap_or(u32::MAX),
            "{held} operations became {drawn} drawn and {railed} railed"
        );
    }

    // -----------------------------------------------------------------------
    // The recorder
    // -----------------------------------------------------------------------

    /// Records a **real** session over its **real** city, as PNG frames and a
    /// GIF.
    ///
    /// This is the M2 iteration loop PRD §15 asks for — *"minutes per iteration,
    /// real data, no live infrastructure"* — and it is a test rather than a
    /// binary so it lives inside `polis-render/src` and needs no manifest
    /// change. `#[ignore]` because it reads the operator's `~/.claude/projects`
    /// and writes megabytes.
    ///
    /// ```text
    /// cargo test -p polis-render --release -- --ignored --nocapture record_a_real_session
    /// ```
    ///
    /// | Variable | Meaning |
    /// |---|---|
    /// | `POLIS_OUT` | output directory (required) |
    /// | `POLIS_SESSION` | substring of the session id, repo path or title to record |
    /// | `POLIS_LIST` | print the ten busiest replayable sessions and stop |
    /// | `POLIS_FRAMES` | frames to render; default 96 |
    /// | `POLIS_PIXELS` | map size in pixels; default 900 |
    /// | `POLIS_FROM` / `POLIS_TO` | fraction of the compressed timeline to record; default the whole of it |
    /// | `POLIS_TRAIL` | `fade` or `timed`; default renders **both** |
    ///
    /// # It seeks rather than plays
    ///
    /// [`polis_world::replay::ReplayClock`] clamps its speed at 64×
    /// ([`polis_world::replay::SPEED_RANGE`]), which is right for a transport a
    /// human is driving and wrong for a recorder: at 64× a 26-hour session needs
    /// 24 minutes of wall clock to record. The frames are therefore placed on
    /// the compressed timeline by hand and the driver is *seeked* to each one. A
    /// forward seek applies exactly the events in the span and ticks the world
    /// once at the end — the same thing `advance` does — so the world each frame
    /// sees is the world that moment had.
    #[test]
    #[ignore = "reads the operator's real sessions and writes images"]
    fn record_a_real_session() {
        use polis_events::PathMapper;
        use polis_world::replay::ReplayDriver;
        use polis_world::sessions::{IndexOptions, SessionIndex};
        use polis_world::DenylistUbiquity;

        let Some(projects) = polis_world::sessions::default_projects_dir().filter(|p| p.is_dir())
        else {
            eprintln!("skipped: no ~/.claude/projects on this machine");
            return;
        };
        let index = SessionIndex::scan_with(&projects, &IndexOptions::quick()).expect("scan");
        let want = std::env::var("POLIS_SESSION").ok();
        let mut candidates: Vec<_> = index
            .sessions
            .iter()
            .filter(|s| s.repo_exists && s.repo.is_some())
            .filter(|s| {
                want.as_ref().is_none_or(|w| {
                    s.session.as_str().contains(w)
                        || s.repo
                            .as_ref()
                            .is_some_and(|r| r.to_string_lossy().contains(w))
                })
            })
            .collect();
        candidates.sort_by_key(|s| std::cmp::Reverse(s.bytes));
        if std::env::var("POLIS_LIST").is_ok() {
            for s in candidates.iter().take(10) {
                eprintln!(
                    "{:>7} KiB  {}  {}",
                    s.bytes / 1024,
                    s.session,
                    s.repo
                        .as_ref()
                        .map_or_else(String::new, |r| r.display().to_string())
                );
            }
            return;
        }
        let Ok(out) = std::env::var("POLIS_OUT") else {
            eprintln!("skipped: set POLIS_OUT to a directory");
            return;
        };
        let out = std::path::PathBuf::from(out);
        std::fs::create_dir_all(&out).expect("output directory");
        let Some(sample) = candidates.first() else {
            eprintln!("skipped: no replayable session with a live repository");
            return;
        };
        let repo_path = sample.repo.clone().expect("a repo");
        eprintln!(
            "session {} ({} KiB) over {}",
            sample.session,
            sample.bytes / 1024,
            repo_path.display()
        );

        let built = Instant::now();
        let repo = polis_repo::tree::RepoIndex::open(&repo_path).expect("index the repository");
        let city = polis_layout::city::generate_city(repo.tree());
        eprintln!(
            "  city: {} buildings, {} districts in {:?}",
            city.layout.buildings.len(),
            city.layout.districts.len(),
            built.elapsed()
        );

        let mapper = PathMapper::new(&repo_path).expect("mapper");
        let read = Instant::now();
        let mut schedule = sample.schedule(&mapper).expect("offline full read");
        // Idle compression is what makes a day-long session watchable at all.
        schedule.compress_idle_gaps(Some(polis_world::replay::DEFAULT_IDLE_GAP_CAP));
        eprintln!(
            "  {} events from {} files in {:?}; {:.1} min after compression, from {:.1} h",
            schedule.len(),
            schedule.files.len(),
            read.elapsed(),
            schedule.duration_ms() as f64 / 60_000.0,
            schedule.session_duration_ms() as f64 / 3_600_000.0,
        );
        assert!(!schedule.is_empty(), "a real session produces events");

        let env_num = |k: &str, d: f64| -> f64 {
            std::env::var(k)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(d)
        };
        let frames = env_num("POLIS_FRAMES", 96.0) as usize;
        let pixels = env_num("POLIS_PIXELS", 900.0) as usize;
        let from = env_num("POLIS_FROM", 0.0).clamp(0.0, 1.0);
        let to = env_num("POLIS_TO", 1.0).clamp(from, 1.0);
        let styles: Vec<(&str, TrailStyle)> = match std::env::var("POLIS_TRAIL").as_deref() {
            Ok("fade") => vec![("fade", TrailStyle::Fade)],
            Ok("timed") => vec![("timed", TrailStyle::Timed)],
            _ => vec![("timed", TrailStyle::Timed), ("fade", TrailStyle::Fade)],
        };
        let total = schedule.duration_ms();
        // Where each frame lands on the compressed timeline.
        //
        // This used to be "the busiest 45 s window, cut into N equal slices",
        // and the constant that came out of it — world-seconds per frame — was
        // doing all the work with nothing checking it: one M2 recording ran at
        // ~0.9 world-s/frame and read as alive while another spent 43% of its
        // frames changing fewer than 200 of 1.32M pixels. `crate::pacing`
        // derives the step from the schedule's own event density instead, and
        // clamps it to the range the notation can actually draw.
        let pace = crate::pacing::plan(&schedule, frames);
        let placements: Vec<u64> =
            if std::env::var("POLIS_FROM").is_ok() || std::env::var("POLIS_TO").is_ok() {
                let (a, b) = ((total as f64 * from) as u64, (total as f64 * to) as u64);
                (0..frames)
                    .map(|i| a + (b - a) * i as u64 / (frames.max(2) as u64 - 1))
                    .collect()
            } else {
                pace.frames_ms.clone()
            };
        let frame_dt = Duration::from_millis(1000 / 24);
        eprintln!(
            "  {} frames at {pixels} px over {:.2}-{:.2} min of the compressed timeline",
            placements.len(),
            placements.first().copied().unwrap_or(0) as f64 / 60_000.0,
            placements.last().copied().unwrap_or(0) as f64 / 60_000.0
        );
        eprintln!(
            "  pacing: {:.3} world-s/frame mean (step {} ms from {:.1} live events/s), \
             {} live events + {} waiting-on-you moments in the window, \
             {} frames at the floor, {} at the ceiling — {}",
            pace.mean_step_secs(),
            pace.step_ms,
            pace.events_per_second,
            pace.live_events,
            pace.decisions,
            pace.floored,
            pace.ceilinged,
            pace.verdict(),
        );

        // Frame the work, not the repository. On a real checkout most of the
        // block graph is `target/` or `node_modules/` — PRD §8's industrial
        // zones, which are *supposed* to be dull — and fitting the whole city
        // leaves the source quarter a few dozen pixels across. The camera is
        // therefore pointed at the bounding box of the paths this session
        // actually touched, which is PRD §12's district tier.
        let focus = if std::env::var("POLIS_ZOOM").as_deref() == Ok("city") {
            None
        } else {
            // A pre-pass over the whole schedule, because
            // `replay::paths_in` reads the OTel channel and a transcript replay
            // carries none: the paths have to come from the world the events
            // build. It costs one run of the session, which is milliseconds.
            let mut scout = World::for_replay(city.layout.clone());
            ReplayDriver::with_origin(schedule.clone(), Instant::now()).run_to_end(&mut scout);
            let mut xs: Vec<f32> = Vec::new();
            let mut ys: Vec<f32> = Vec::new();
            for path in scout.files.keys() {
                if let Some(p) = position_of(&city.layout, path) {
                    xs.push(p.x);
                    ys.push(p.y);
                }
            }
            eprintln!(
                "  {} of {} touched paths land on the city",
                xs.len(),
                scout.files.len()
            );
            if xs.len() < 4 {
                None
            } else {
                // A **robust** frame, not a bounding box. One file read in
                // `tests/` at the far edge of the map would otherwise pull the
                // extent out by a factor of three and undo the zoom, which is
                // exactly what a bounding box did on the first attempt. The
                // camera goes on the median with the 70th-percentile radius.
                xs.sort_by(f32::total_cmp);
                ys.sort_by(f32::total_cmp);
                let centre = Point {
                    x: xs[xs.len() / 2],
                    y: ys[ys.len() / 2],
                };
                let mut radii: Vec<f32> = xs
                    .iter()
                    .zip(ys.iter())
                    .map(|(x, y)| (x - centre.x).abs().max((y - centre.y).abs()))
                    .collect();
                radii.sort_by(f32::total_cmp);
                let extent = env_num("POLIS_EXTENT", 0.0) as f32;
                let extent = if extent > 0.0 {
                    extent
                } else {
                    (radii[radii.len() * 85 / 100] * 1.30).max(city.layout.extent * 0.07)
                };
                let focus = Focus { centre, extent };
                eprintln!(
                    "  camera: centre ({:.1}, {:.1}), extent {extent:.1} of a {:.1} city",
                    focus.centre.x, focus.centre.y, city.layout.extent
                );
                Some(focus)
            }
        };

        for (name, style) in styles {
            let mut world = World::new(repo.tree().clone(), city.layout.clone());
            world.set_ubiquity(Box::new(DenylistUbiquity::default()));
            let origin = Instant::now();
            let mut driver = ReplayDriver::with_origin(schedule.clone(), origin);

            let mut renderer = FrameRenderer::new(
                &city,
                FrameOptions {
                    pixels,
                    supersample: 2,
                    trail: style,
                    focus,
                    ..FrameOptions::default()
                },
            );
            let (publisher, reader) = snapshot::from_world(&world);
            let mut shots: Vec<Canvas> = Vec::with_capacity(frames);
            let mut budgeted: Vec<Duration> = Vec::with_capacity(frames);
            let mut draw_total = Duration::ZERO;
            let started = Instant::now();
            for (i, target) in placements.iter().copied().enumerate() {
                driver.seek(target, &mut world);
                publisher.force(&world);
                let snap = reader.load();
                let p = driver.progress();
                let session_s = p.session_elapsed().as_secs();
                let waiting = snap
                    .threads
                    .iter()
                    .filter(|t| t.status == ThreadStatus::Waiting)
                    .count();
                renderer.set_label(format!(
                    "T+{:02}:{:02}:{:02}  {:>3.0}%  {} TRAIL  {}",
                    session_s / 3600,
                    (session_s / 60) % 60,
                    session_s % 60,
                    p.fraction() * 100.0,
                    name.to_uppercase(),
                    if waiting > 0 { "WAITING" } else { "" }
                ));
                if i * 2 == frames {
                    let f = renderer.build(&snap, 0.0);
                    eprintln!(
                        "  [{name}] mid frame: {} marks, {} trail steps, {} agents, {} tethers,                          {} thrash, {} scaffolds, {} cloud kernels; unit {:.1} px, glyph r {:.1} px",
                        f.marks.len(),
                        f.trails.iter().map(|t| t.steps.len()).sum::<usize>(),
                        f.agents.len(),
                        f.tethers.len(),
                        f.thrash.len(),
                        f.scaffolds.len(),
                        f.clouds.len(),
                        f.unit,
                        live::glyph_radius(f.unit, f.map_height),
                    );
                    let th = &snap.threads[0];
                    let with_path = th.ops.iter().filter(|o| o.path.is_some()).count();
                    let placed = th
                        .ops
                        .iter()
                        .filter_map(|o| o.path.as_ref())
                        .filter(|p| position_of(&city.layout, p).is_some())
                        .count();
                    let fresh = th
                        .ops
                        .iter()
                        .filter(|o| secs_since(snap.at, o.at) <= MARK_TTL)
                        .count();
                    eprintln!(
                        "  [{name}] thread: {} ops ({with_path} with a path, {placed} placeable,                          {fresh} within {MARK_TTL}s), {} trail, {} visits, status {:?}",
                        th.ops.len(),
                        th.trail.len(),
                        th.visits.len(),
                        th.status
                    );
                    let t = &th.territory;
                    eprintln!(
                        "  [{name}] territory: claim {:?}, {} kernels, {} observations",
                        t.claim.as_ref().map(polis_events::LogicalPath::as_str),
                        t.kernels.len(),
                        t.observations
                    );
                }
                let canvas = renderer.render_owned(&snap, frame_dt);
                let t = renderer.timings();
                budgeted.push(t.budgeted());
                draw_total += t.total();
                if i % 8 == 0 || i + 1 == frames {
                    canvas
                        .write_png(&out.join(format!("{name}-{i:03}.png")))
                        .expect("png");
                }
                shots.push(canvas);
            }
            let snap = reader.load();
            budgeted.sort_unstable();
            eprintln!(
                "  [{name}] {frames} frames in {:?}; agent+attention p50 {:?}, p95 {:?}, max {:?};                  whole live layer mean {:?}",
                started.elapsed(),
                budgeted[budgeted.len() / 2],
                budgeted[budgeted.len() * 95 / 100],
                budgeted.last().copied().unwrap_or_default(),
                draw_total / u32::try_from(frames).unwrap_or(1)
            );
            eprintln!(
                "  [{name}] end state: {} threads, {} workers, {} files, {} attention marks",
                snap.threads.len(),
                snap.threads.iter().map(|t| t.workers.len()).sum::<usize>(),
                snap.files.len(),
                snap.attention.len()
            );
            gif::write(&out.join(format!("replay-{name}.gif")), &shots, 4).expect("gif");
        }
        eprintln!("wrote {}", out.display());
    }
    /// Draws the whole visual language once, on a real city, at district zoom.
    ///
    /// The real-session recording is the honest test of the renderer and a poor
    /// test of the *notation*: a live session spends most of its time reading
    /// files, so eight frames in ten show hollow circles in teal and nothing
    /// else. This sheet puts every one of PRD §10.1's six shapes, §10.2's three
    /// colours, both trail notations, the revisit rosette, the scaffolding, the
    /// tether fan, an iso-contour cloud and all three §11.2 attention states on
    /// one map, at the size they are actually drawn, so the questions "can these
    /// six shapes be told apart" and "is the city still legible under them" have
    /// a picture to be answered from.
    ///
    /// Everything is placed on **real buildings** of a generated city, at the
    /// real glyph radius. Nothing is scaled up for the illustration.
    ///
    /// ```text
    /// POLIS_OUT=<dir> cargo test -p polis-render --release -- --ignored --nocapture notation_sheet
    /// ```
    #[test]
    #[ignore = "writes images"]
    fn notation_sheet() {
        let Ok(out) = std::env::var("POLIS_OUT") else {
            eprintln!("skipped: set POLIS_OUT to a directory");
            return;
        };
        let out = std::path::PathBuf::from(out);
        std::fs::create_dir_all(&out).expect("output directory");

        let city = city::generate_city(&synthetic::repository(900, 0x51));
        let pixels = 900;
        let renderer = FrameRenderer::new(
            &city,
            FrameOptions {
                pixels,
                supersample: 2,
                caption: false,
                ..FrameOptions::default()
            },
        );
        let view = *renderer.view();
        let unit = renderer.unit();
        let r = live::glyph_radius(unit, pixels as f64);
        eprintln!(
            "city: {} buildings, unit {unit:.1} px, glyph radius {r:.1} px",
            city.layout.buildings.len()
        );

        // A column of buildings to hang the sheet on, spread across the map.
        let mut spots: Vec<Px> = city
            .layout
            .buildings
            .values()
            .map(|b| view.at(b.footprint.centroid()))
            .collect();
        spots.sort_by(|a, b| a[1].total_cmp(&b[1]).then(a[0].total_cmp(&b[0])));
        let pick = |fx: f64, fy: f64| -> Px {
            // Nearest real building to a fractional position on the map.
            let target = [fx * pixels as f64, fy * pixels as f64];
            *spots
                .iter()
                .min_by(|a, b| {
                    let d = |p: &Px| (p[0] - target[0]).powi(2) + (p[1] - target[1]).powi(2);
                    d(a).total_cmp(&d(b))
                })
                .expect("a building")
        };

        let glyphs = [
            (Glyph::HollowCircle, "READ"),
            (Glyph::BarredCircle, "EDIT"),
            (Glyph::FilledSquare, "WRITE"),
            (Glyph::FilledTriangle, "RUN"),
            (Glyph::ConcentricCircles, "VERIFY"),
            (Glyph::Delegate, "DELEGATE"),
        ];
        let outcomes = [
            (Outcome::Pending, "PENDING"),
            (Outcome::Done, "DONE"),
            (Outcome::Failed, "FAILED"),
        ];

        let mut frame = LiveFrame {
            unit,
            map_height: pixels as f64,
            ..LiveFrame::default()
        };

        // 1. The shape × colour matrix, one mark per real building.
        for (gi, (glyph, _)) in glyphs.iter().enumerate() {
            for (oi, (outcome, _)) in outcomes.iter().enumerate() {
                frame.marks.push(Mark::single(
                    pick(0.10 + gi as f64 * 0.055, 0.12 + oi as f64 * 0.055),
                    *glyph,
                    *outcome,
                    0.0,
                    0.0,
                ));
            }
        }
        // 2. The same six shapes aged, to show the fade stays inside the band.
        for (gi, (glyph, _)) in glyphs.iter().enumerate() {
            for step in 0..4 {
                frame.marks.push(Mark::single(
                    pick(0.10 + gi as f64 * 0.055, 0.32 + f64::from(step) * 0.045),
                    *glyph,
                    Outcome::Done,
                    f64::from(step) / 3.0,
                    0.0,
                ));
            }
        }
        // 3. An arrival, mid-pulse.
        frame.marks.push(Mark::single(
            pick(0.50, 0.14),
            Glyph::FilledSquare,
            Outcome::Done,
            0.0,
            0.55,
        ));

        // 4. A trail that backtracks and thrashes (PRD §12).
        let walk: Vec<Px> = [
            (0.62, 0.30),
            (0.72, 0.36),
            (0.66, 0.44),
            (0.78, 0.42),
            (0.66, 0.44),
            (0.84, 0.52),
            (0.66, 0.44),
            (0.74, 0.58),
            (0.66, 0.44),
            (0.60, 0.55),
            (0.66, 0.44),
            (0.70, 0.66),
        ]
        .iter()
        .map(|(x, y)| pick(*x, *y))
        .collect();
        let n = walk.len();
        frame.trails.push(Trail {
            steps: walk
                .iter()
                .enumerate()
                .map(|(i, at)| TrailStep {
                    at: *at,
                    age: (n - 1 - i) as f64 * 24.0,
                    visits: if i % 2 == 0 { 6 } else { 1 },
                })
                .collect(),
            ttl: 300.0,
        });
        frame.thrash.push(Thrash {
            at: walk[2],
            visits: 6,
            age: 0.1,
        });

        // 5. A thread with eight workers: the tether fan.
        let anchor = pick(0.30, 0.72);
        for k in 0..8 {
            let worker = pick(0.14 + f64::from(k) * 0.035, 0.86);
            frame.tethers.push(live::Tether {
                anchor,
                worker,
                running: k % 4 != 3,
                spread: (f64::from(k) / 7.0).mul_add(2.0, -1.0),
            });
            frame.agents.push(Agent {
                at: worker,
                body: Body::Worker,
                outcome: match k % 3 {
                    0 => Outcome::Pending,
                    1 => Outcome::Done,
                    _ => Outcome::Failed,
                },
                travel: f64::from(k) / 8.0,
                heading: [0.6, -0.8],
                waiting: false,
            });
        }
        frame.agents.push(Agent {
            at: anchor,
            body: Body::Main,
            outcome: Outcome::Pending,
            travel: 0.0,
            heading: [0.0, 0.0],
            waiting: true,
        });

        // 6. Scaffolding on files under edit, at four heights.
        for k in 0..6 {
            let at = pick(0.50 + f64::from(k) * 0.04, 0.30);
            frame.scaffolds.push(Scaffold {
                at,
                half_width: r * 0.8,
                rise: unit * 0.12 * f64::from(k + 1),
                age: 0.1,
            });
        }

        // 7. A territory cloud (PRD §10.4).
        let core = pick(0.72, 0.80);
        for k in 0..26 {
            let a = f64::from(k) * 0.618;
            frame.clouds.push(live::CloudKernel {
                at: [
                    (a.fract() - 0.5).mul_add(unit * 5.0, core[0]),
                    ((a * 1.7).fract() - 0.5).mul_add(unit * 4.0, core[1]),
                ],
                radius: unit * 2.4,
                weight: 1.0,
            });
        }

        // 8. All three attention states, including a contention link.
        let a = pick(0.24, 0.50);
        let b = pick(0.86, 0.22);
        frame.attention.push(AttentionMark {
            kind: MarkKind::NeedsDecision,
            at: a,
            other: None,
            severity: None,
            pulse: 0.0,
            weight: 1.0,
        });
        frame.attention.push(AttentionMark {
            kind: MarkKind::DoneVerified,
            at: pick(0.44, 0.62),
            other: None,
            severity: None,
            pulse: 0.0,
            weight: 1.0,
        });
        frame.attention.push(AttentionMark {
            kind: MarkKind::DoneUnverified,
            at: pick(0.56, 0.72),
            other: None,
            severity: None,
            pulse: 0.0,
            weight: 1.0,
        });
        frame.attention.push(AttentionMark {
            kind: MarkKind::Contention,
            at: a,
            other: Some(b),
            severity: Some(polis_world::contention::Severity::Critical),
            pulse: 0.0,
            weight: 1.0,
        });

        for (name, style) in [("timed", TrailStyle::Timed), ("fade", TrailStyle::Fade)] {
            let mut canvas = renderer.base.map.clone();
            let t = live::draw_clouds(&mut canvas, &frame);
            for (i, colour) in &renderer.base.labels {
                let o = *i as usize * 3;
                canvas.pixels[o..o + 3].copy_from_slice(colour);
            }
            let ta = live::draw_agents(&mut canvas, &frame, style);
            let tb = live::draw_attention(&mut canvas, &frame);
            eprintln!("  [{name}] clouds {t:?}, agents {ta:?}, attention {tb:?}");
            canvas
                .write_png(&out.join(format!("notation-{name}.png")))
                .expect("png");

            // How much of the map did the live layer actually cover?
            let base = &renderer.base.map;
            let changed = base
                .pixels
                .as_chunks::<3>()
                .0
                .iter()
                .zip(canvas.pixels.as_chunks::<3>().0.iter())
                .filter(|(a, b)| a != b)
                .count();
            eprintln!(
                "  [{name}] live layer covers {:.1}% of the map",
                100.0 * changed as f64 / (pixels * pixels) as f64
            );
        }
        eprintln!("wrote {}", out.display());
    }
}
