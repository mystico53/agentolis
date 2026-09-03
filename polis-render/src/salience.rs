//! The alarm: PRD §10.2's failure colour given **area** (PRD §11.4, §17).
//!
//! # The defect this module exists to fix
//!
//! By M4 the colour channel was correct. Every operation resolved to a place,
//! failures were never aggregated into successes, and the failed glyph was drawn
//! in [`live::AGENT_FAILED`] exactly where the failure happened. An independent
//! review then measured what that was worth from a metre away:
//!
//! > the colour channel now carries information and is correct at the mark; the
//! > map does not yet shout it. A failing session and a clean one are
//! > distinguishable in about a second by reading one number, and not
//! > distinguishable by peripheral vision, which is what PRD §11.4 actually asks
//! > for.
//!
//! The reddest frame in 1 440 held **142 red pixels out of 1.21 M** — two glyph
//! outlines. Reproduced here in `polis-render/tests/salience.rs` against a
//! 60 × 60 thumbnail: **2 hot pixels of 3 600**, against 0 for a clean session.
//! A difference of two pixels in three thousand is not a difference.
//!
//! **The problem is area, not hue.** PRD §11.4 says so in as many words —
//! *"peripheral vision is poor at colour and good at motion onset"*, and
//! *"colour alone is never the sole channel for any state"* — so making the red
//! redder was never going to work, and a brighter red would have broken §10.3's
//! band scheme for nothing.
//!
//! # What is drawn
//!
//! Failures within [`ALARM_LINK`] glyph-radii of each other are one **alarm**,
//! and an alarm is three things, in §11.4's own order:
//!
//! * **arrival** — a thick expanding ring, gone in ≤400 ms. Motion onset, which
//!   is the channel the periphery actually has. This is not a decoration on the
//!   glyph's own pulse: it is an order of magnitude larger, because it is a
//!   claim about a *region* rather than about one call;
//! * **steady state** — a heavy broken ring with radial ticks around the region,
//!   sized by how far the failures spread and how many there are. Shape and
//!   position, decoded when the operator turns their head. No colour is needed
//!   to read it: a ring of ticks around a district is not a shape anything else
//!   in Polis draws;
//! * **persistence** — the ring holds full ink for the first half of the
//!   operation's life in the world and never fades below [`ALARM_FLOOR`]. A
//!   failure that scrolled out of the terminal is still on the map.
//!
//! # Why this is layer 4 and not layer 5
//!
//! PRD §10.3 gives layer 5 to *"the three states"* and PRD §11.2 defines exactly
//! three. A failed tool call is not one of them: it is §10.2's **outcome**
//! channel, which belongs to the agent layer, and promoting it would put a
//! fourth thing in the band whose whole job is that §11.1's three-way ordering
//! is legible. So the alarm is drawn in [`crate::plan::AGENT_BAND`], at its
//! ceiling — channel 168 against a base map clamped at 48, a luminance ratio of
//! about 5.6:1, which is more than enough to survive a box filter. Layer 5 is
//! spent on the states that own it, and it is spent hard: see
//! [`live::draw_attention`].
//!
//! # And why it does not swallow the map
//!
//! PRD §17's default failure mode is *"a beautiful swarm view that makes the
//! operator feel informed while telling them nothing actionable"*. An alarm that
//! covers a third of the city says only that Polis panics. Three bounds hold it:
//! clustering (one ring for a region, not one per call), [`ALARM_CAP`] (the
//! worst eight, by count), and [`ALARM_MAP_CAP`] (no ring wider than 9 % of the
//! map). `the_alarm_does_not_swallow_the_map` asserts the result on pixels.

// The same geometry lints `live` and `plan` silence, for the same reasons: the
// `as` casts here all land in a quantised sort key that is rounded on purpose
// (PRD §7.4 wants a *stable* order, not a precise one), or in a pixel index.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::many_single_char_names
)]

use std::time::{Duration, Instant};

use crate::live::{self, Mark};
use crate::raster::{Canvas, Px, Rgb};

/// The smallest alarm ring, as a fraction of the map's height.
///
/// # Why the alarm is measured against the map and not against a glyph
///
/// The first version sized the ring in glyph radii, and the glyph radius tracks
/// the **median block** — a building. Measured on the 600-pixel validation
/// frame that put a lone failure's ring at 22 px, which is a mark about a *lot*;
/// downsampled by ten it was two thumbnail pixels of arc and the whole exercise
/// had achieved nothing.
///
/// The alarm is not about a lot. Nobody standing three metres from the monitor
/// can act on "the failure was in `src/auth/mod.rs`" — they can act on **"that
/// district is in trouble"**, walk over, and read the glyphs. PRD §11.2 already
/// says the primary state is drawn "above the building **or district**"; failure
/// gets the same latitude, and it takes the district.
///
/// 4.5 % of the map height is about a third of a district on a real city, which
/// is a ring the eye resolves at any distance the screen is legible at.
pub const ALARM_MIN_FRACTION: f64 = 0.045;

/// …and the largest, as the same fraction. PRD §17: an alarm covering the map
/// says only that Polis panics.
pub const ALARM_MAP_CAP: f64 = 0.105;

/// A floor on the ring, in glyph radii, for the close-zoom case where the map
/// fraction would put the ring inside the glyphs it is about.
pub const ALARM_MIN: f64 = 4.2;

/// How close two failures must be to become one alarm, as a multiple of the
/// smallest ring's **diameter**.
///
/// The rule is the picture's: two failures whose rings would visibly overlap are
/// one region and get one ring, and two that would read as separate rings stay
/// separate. Slightly over 1 so that rings which would merely graze each other
/// still merge — a pair of tangent circles reads as a figure-of-eight, which
/// means nothing.
pub const ALARM_LINK: f64 = 1.15;

/// Clearance between the outermost failure in a cluster and its ring, in glyph
/// radii. The ring must not cross the glyphs it is about.
pub const ALARM_MARGIN: f64 = 2.2;

/// Ring stroke width as a fraction of the ring's own radius.
///
/// The number the thumbnail test actually turns on, and the reason it is a
/// *fraction* rather than a width. A box filter of factor `f` dilutes a stroke
/// of width `w` to `w / f` of a thumbnail pixel, so a fixed stroke gets quieter
/// as the map gets bigger — exactly backwards. Tying the stroke to the radius
/// makes the alarm's ink grow as `R²`, so a failure spread across a district
/// shouts louder than one confined to a corner of it, which is the true
/// statement.
///
/// At 0.2 the ring is 11 px on a 600-pixel map: wider than a thumbnail cell at
/// the measurement's factor of ten, so every cell the ring passes through is
/// **fully** red rather than a blend. That is the difference between peak 53 and
/// peak 70.
pub const ALARM_RING: f64 = 0.20;

/// How many arcs the ring is broken into, and how much of each period is inked.
///
/// A broken ring, because an unbroken one is what [`live::MarkKind::DoneVerified`]
/// draws in teal and PRD §11.4 forbids colour as the sole channel for anything. Eight
/// arcs at 72 % duty is still overwhelmingly ring — the gaps are a rhythm, not a
/// dotted line — and it costs 28 % of the area, which the spokes give back.
pub const ALARM_ARCS: usize = 8;
/// Fraction of each arc period that is inked. See [`ALARM_ARCS`].
pub const ALARM_DUTY: f64 = 0.72;

/// Radial spokes in the gaps, as a fraction of the ring radius.
///
/// They point **outward**, which is the difference between a mark that says
/// "there is something inside this ring" and one that says "this ring is a
/// thing". They are also the part of the silhouette nothing else in Polis has:
/// a ring is a `done` mark and a disc is a worker, but a ring that radiates is
/// only ever this. That is what makes the alarm identifiable with the colour
/// thrown away, which is what PRD §11.4 demands of every state and — until now —
/// of every state except the one that matters most.
pub const ALARM_TICK: f64 = 0.45;

/// How far past its own radius the arrival flare travels, as a multiple of it.
///
/// It has to clear the spokes — [`ALARM_TICK`] puts them at 1.45 R — or the
/// "expanding ring" never visibly leaves the mark and the onset is a flicker
/// rather than travel. At 1.9 the flare crosses the spoke tips a third of the
/// way through its 400 ms and is still growing when it fades out, which is the
/// motion PRD §11.4 is asking for.
pub const ALARM_FLARE: f64 = 1.9;

/// The lowest an alarm's ink ever falls, as a fraction of full weight.
///
/// Not zero, and that is the whole of "persistence": the operation mark
/// underneath fades to its band floor across
/// [`crate::frame::MARK_TTL`], because a mark is a memory; the alarm does not,
/// because *the build is still broken* is not a memory. It leaves the map when
/// the world drops the operation, and not before.
pub const ALARM_FLOOR: f64 = 0.55;

/// Normalised age at which the ink starts falling toward [`ALARM_FLOOR`].
pub const ALARM_HOLD: f64 = 0.5;

/// The most alarms drawn in one frame, worst first.
///
/// PRD §10.4 caps clouds because "forty threads means forty systems and the map
/// vanishes under haze"; the same argument applies to a ring drawn round a
/// district. Eight regions in trouble at once is already a session the operator
/// should be reading rather than glancing at.
pub const ALARM_CAP: usize = 8;

/// How much bigger a ring gets per failure it stands for, and the cap on that.
///
/// Stepped and shallow: the count is carried by the *glyphs inside the ring*,
/// which are countable; the ring only has to say "more of them here than there".
pub const ALARM_GROWTH: f64 = 0.055;
/// Failures past which [`ALARM_GROWTH`] stops adding.
pub const ALARM_GROWTH_CAP: u32 = 10;

/// A 32-gon on the unit circle, from a constant table.
///
/// Finer than [`live::CIRCLE`] because an alarm ring is several times a glyph's
/// radius, and a 16-gon at that size is visibly a polygon. No trigonometry
/// reaches the image (PRD §7.4), so it is a table.
const RING: [[f64; 2]; 32] = [
    [1.000, 0.000],
    [0.981, 0.195],
    [0.924, 0.383],
    [0.831, 0.556],
    [0.707, 0.707],
    [0.556, 0.831],
    [0.383, 0.924],
    [0.195, 0.981],
    [0.000, 1.000],
    [-0.195, 0.981],
    [-0.383, 0.924],
    [-0.556, 0.831],
    [-0.707, 0.707],
    [-0.831, 0.556],
    [-0.924, 0.383],
    [-0.981, 0.195],
    [-1.000, 0.000],
    [-0.981, -0.195],
    [-0.924, -0.383],
    [-0.831, -0.556],
    [-0.707, -0.707],
    [-0.556, -0.831],
    [-0.383, -0.924],
    [-0.195, -0.981],
    [0.000, -1.000],
    [0.195, -0.981],
    [0.383, -0.924],
    [0.556, -0.831],
    [0.707, -0.707],
    [0.831, -0.556],
    [0.924, -0.383],
    [0.981, -0.195],
];

/// One region of the map that is in trouble.
///
/// Built from [`Mark`]s rather than from the world, deliberately: the alarm is a
/// statement about *what is on the screen* — these glyphs, at these pixels,
/// after aggregation and after the per-thread mark budget — and computing it
/// from the world would let it disagree with the picture it is annotating.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Alarm {
    /// Centre of the failures it covers.
    pub at: Px,
    /// Ring radius in pixels, already clamped.
    pub radius: f64,
    /// How many failed operations are inside it.
    pub count: u32,
    /// Arrival pulse in `[0, 1]`, from the freshest failure.
    pub pulse: f64,
    /// Steady-state ink in `[ALARM_FLOOR, 1]`.
    pub weight: f64,
}

/// A cluster being accumulated. Kept separate from [`Alarm`] because the centre
/// moves while members are being added and the radius cannot be known until it
/// stops.
#[derive(Debug, Clone, Copy)]
struct Cluster {
    sum: [f64; 2],
    members: f64,
    count: u32,
    age: f64,
    pulse: f64,
}

impl Cluster {
    fn centre(self) -> Px {
        [self.sum[0] / self.members, self.sum[1] / self.members]
    }
}

/// Groups the failed marks of a frame into alarms.
///
/// Deterministic: the marks are visited in a fixed order (rounded position, then
/// glyph), so the same frame yields the same clusters on every run and every
/// machine. Greedy single-pass assignment — a mark joins the first cluster whose
/// running centre it is within [`ALARM_LINK`] of — because the alternative is
/// an iterative clustering whose output depends on floating-point tie order,
/// and PRD §7.4 does not allow that.
#[must_use]
pub fn alarms(marks: &[Mark], r: f64, map_height: f64) -> Vec<Alarm> {
    let mut failed: Vec<&Mark> = marks
        .iter()
        .filter(|m| m.outcome == polis_events::Outcome::Failed)
        .collect();
    if failed.is_empty() {
        return Vec::new();
    }
    failed.sort_by(|a, b| {
        (a.at[0].round() as i64)
            .cmp(&(b.at[0].round() as i64))
            .then((a.at[1].round() as i64).cmp(&(b.at[1].round() as i64)))
            .then((a.glyph as u8).cmp(&(b.glyph as u8)))
    });

    let floor = (map_height * ALARM_MIN_FRACTION).max(ALARM_MIN * r);
    let link = ALARM_LINK * 2.0 * floor;
    let mut clusters: Vec<Cluster> = Vec::new();
    let mut members: Vec<Vec<Px>> = Vec::new();
    for m in &failed {
        let mut joined = false;
        for (c, pts) in clusters.iter_mut().zip(members.iter_mut()) {
            let centre = c.centre();
            let dx = m.at[0] - centre[0];
            let dy = m.at[1] - centre[1];
            if dx.mul_add(dx, dy * dy) <= link * link {
                c.sum[0] += m.at[0];
                c.sum[1] += m.at[1];
                c.members += 1.0;
                c.count = c.count.saturating_add(m.count.max(1));
                c.age = c.age.min(m.age);
                c.pulse = c.pulse.max(m.pulse);
                pts.push(m.at);
                joined = true;
                break;
            }
        }
        if !joined {
            clusters.push(Cluster {
                sum: m.at,
                members: 1.0,
                count: m.count.max(1),
                age: m.age,
                pulse: m.pulse,
            });
            members.push(vec![m.at]);
        }
    }

    let cap = (map_height * ALARM_MAP_CAP).max(floor);
    let mut out: Vec<Alarm> = clusters
        .iter()
        .zip(members.iter())
        .map(|(c, pts)| {
            let centre = c.centre();
            let spread = pts
                .iter()
                .map(|p| {
                    let dx = p[0] - centre[0];
                    let dy = p[1] - centre[1];
                    dx.mul_add(dx, dy * dy).sqrt()
                })
                .fold(0.0f64, f64::max);
            let grown =
                f64::from(c.count.min(ALARM_GROWTH_CAP)).mul_add(ALARM_GROWTH, 1.0) - ALARM_GROWTH;
            let radius = (ALARM_MARGIN.mul_add(r, spread).max(floor) * grown).min(cap);
            Alarm {
                at: centre,
                radius,
                count: c.count,
                pulse: c.pulse.clamp(0.0, 1.0),
                weight: ink_weight(c.age),
            }
        })
        .collect();
    // Worst first, so what the cap drops is the mildest region and never the
    // freshest one. Ties break on position so the choice is stable.
    out.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then(b.weight.total_cmp(&a.weight))
            .then((a.at[0].round() as i64).cmp(&(b.at[0].round() as i64)))
            .then((a.at[1].round() as i64).cmp(&(b.at[1].round() as i64)))
    });
    out.truncate(ALARM_CAP);
    out
}

/// Full ink for the first [`ALARM_HOLD`] of the mark's life, then down to
/// [`ALARM_FLOOR`] and no further.
fn ink_weight(age: f64) -> f64 {
    let age = age.clamp(0.0, 1.0);
    if age <= ALARM_HOLD {
        return 1.0;
    }
    let into = (age - ALARM_HOLD) / (1.0 - ALARM_HOLD);
    (1.0 - ALARM_FLOOR).mul_add(-into, 1.0)
}

/// One stroke of an alarm: a polyline, the width to draw it at, and its ink.
///
/// # Why the shape is a value rather than a drawing call
///
/// Polis has **two** rasterisers. [`crate::live`] draws to a [`Canvas`] — that
/// is the headless path every measurement in this crate runs through — and
/// `polis-app`'s map view draws to an `egui::Painter` on the GPU. The window is
/// the product; the canvas is the evidence. If the two hold separate copies of a
/// notation they will drift, and the drift will always be in the direction of
/// the one nobody is measuring.
///
/// So the alarm's geometry is computed once, here, in device pixels, and both
/// rasterisers consume it. Neither owns it.
#[derive(Debug, Clone, PartialEq)]
pub struct AlarmStroke {
    /// The polyline, in device pixels. Two points is a segment.
    pub points: Vec<Px>,
    /// Stroke width in device pixels.
    pub width: f64,
    /// Failure ink, already faded to the alarm's weight.
    pub ink: Rgb,
}

/// Every stroke of one alarm, in draw order: the broken ring, its spokes, and
/// the arrival flare when one is running.
#[must_use]
pub fn strokes(alarm: Alarm, r: f64) -> Vec<AlarmStroke> {
    let ink = live::fade(live::AGENT_FAILED, live::AGENT_FLOOR, alarm.weight);
    let width = (alarm.radius * ALARM_RING)
        .max(r * 0.6)
        .max(live::MIN_STROKE);
    let at = |u: [f64; 2], radius: f64| {
        [
            u[0].mul_add(radius, alarm.at[0]),
            u[1].mul_add(radius, alarm.at[1]),
        ]
    };
    let per = RING.len() / ALARM_ARCS;
    let inked = ((per as f64) * ALARM_DUTY).round().max(1.0) as usize;
    let mut out = Vec::with_capacity(ALARM_ARCS * 2 + 1);

    // The broken ring. Arcs are polylines off the 32-gon so the gaps land at
    // fixed angles: a rhythm that does not rotate frame to frame is a shape, and
    // one that does is an animation nobody asked for.
    for arc in 0..ALARM_ARCS {
        let points = (0..=inked)
            .map(|k| at(RING[(arc * per + k) % RING.len()], alarm.radius))
            .collect();
        out.push(AlarmStroke { points, width, ink });
    }

    // Spokes in the gaps, pointing out. The one part of the silhouette nothing
    // else in Polis draws, and the part that survives greyscale.
    let tick = alarm.radius * ALARM_TICK;
    for arc in 0..ALARM_ARCS {
        let u = RING[(arc * per + inked) % RING.len()];
        out.push(AlarmStroke {
            points: vec![
                at(u, width.mul_add(-0.4, alarm.radius)),
                at(u, alarm.radius + tick),
            ],
            width: (width * 0.85).max(live::MIN_STROKE),
            ink,
        });
    }

    // PRD §11.4's arrival: motion, at the scale of the region rather than of the
    // glyph. It expands out through the spokes and is gone in 400 ms.
    if alarm.pulse > 0.0 {
        let p = alarm.pulse.clamp(0.0, 1.0);
        let flare = alarm.radius * ALARM_FLARE.mul_add(1.0 - p, 1.0);
        let mut points: Vec<Px> = RING.iter().map(|u| at(*u, flare)).collect();
        points.push(points[0]);
        out.push(AlarmStroke {
            points,
            width: (width * 1.6 * p).max(live::MIN_STROKE),
            ink: live::fade(live::AGENT_FAILED, live::AGENT_FLOOR, p),
        });
    }
    out
}

/// Draws the alarms of one frame. Layer 4, under the operation glyphs.
pub fn draw(canvas: &mut Canvas, alarms: &[Alarm], r: f64) -> Duration {
    let start = Instant::now();
    for alarm in alarms {
        for stroke in strokes(*alarm, r) {
            canvas.polyline(&stroke.points, stroke.width, stroke.ink, 1.0);
        }
    }
    start.elapsed()
}

/// The ink an alarm is drawn in, for tests and for the census.
#[must_use]
pub fn alarm_ink(weight: f64) -> Rgb {
    live::fade(live::AGENT_FAILED, live::AGENT_FLOOR, weight)
}

#[cfg(test)]
mod tests {
    use super::*;
    use polis_events::{Glyph, Outcome};

    fn failed(at: Px, age: f64) -> Mark {
        Mark::single(at, Glyph::FilledTriangle, Outcome::Failed, age, 0.0)
    }

    #[test]
    fn only_failures_raise_an_alarm() {
        let marks = vec![
            Mark::single([100.0, 100.0], Glyph::FilledSquare, Outcome::Done, 0.0, 0.0),
            Mark::single(
                [140.0, 100.0],
                Glyph::HollowCircle,
                Outcome::Pending,
                0.0,
                0.0,
            ),
        ];
        assert!(alarms(&marks, 8.0, 600.0).is_empty());
        assert_eq!(alarms(&[failed([100.0, 100.0], 0.0)], 8.0, 600.0).len(), 1);
    }

    #[test]
    fn nearby_failures_are_one_region_and_distant_ones_are_two() {
        let r = 8.0;
        let close = vec![failed([200.0, 200.0], 0.0), failed([210.0, 205.0], 0.0)];
        assert_eq!(alarms(&close, r, 600.0).len(), 1);
        let far = vec![failed([100.0, 100.0], 0.0), failed([400.0, 400.0], 0.0)];
        assert_eq!(alarms(&far, r, 600.0).len(), 2);
    }

    /// The ring must not cross the glyphs it is about, at any spread.
    #[test]
    fn the_ring_clears_every_failure_it_covers() {
        let r = 8.0;
        let pts = [[200.0, 200.0], [230.0, 210.0], [215.0, 235.0]];
        let marks: Vec<Mark> = pts.iter().map(|p| failed(*p, 0.0)).collect();
        let a = alarms(&marks, r, 600.0);
        assert_eq!(a.len(), 1);
        for p in pts {
            let dx = p[0] - a[0].at[0];
            let dy = p[1] - a[0].at[1];
            let d = dx.mul_add(dx, dy * dy).sqrt();
            assert!(
                a[0].radius >= d + r,
                "ring {:.1} does not clear a glyph at {d:.1} + {r}",
                a[0].radius
            );
        }
    }

    /// PRD §17: an alarm that covers the map says only that Polis panics.
    #[test]
    fn a_ring_is_bounded_by_the_map_however_far_the_failures_spread() {
        let marks: Vec<Mark> = (0..12)
            .map(|i| failed([40.0 + f64::from(i) * 45.0, 300.0], 0.0))
            .collect();
        for a in alarms(&marks, 8.0, 600.0) {
            assert!(
                a.radius <= 600.0 * ALARM_MAP_CAP + 0.001,
                "ring of {:.1} px on a 600 px map",
                a.radius
            );
        }
    }

    /// The alarm is a statement about a *region*, so it scales with the map and
    /// not with the median building. A glyph-scaled ring was the first version
    /// and it measured two thumbnail pixels; see [`ALARM_MIN_FRACTION`].
    #[test]
    fn a_ring_is_district_scale_and_not_building_scale() {
        let one = alarms(&[failed([300.0, 300.0], 0.0)], 5.3, 600.0);
        assert_eq!(one.len(), 1);
        assert!(
            one[0].radius >= 600.0 * ALARM_MIN_FRACTION - 0.001,
            "a lone failure drew a {:.1} px ring on a 600 px map",
            one[0].radius
        );
        // …and it follows the output size, so the notation reads the same on a
        // 4K second monitor as on the validation frame.
        let big = alarms(&[failed([600.0, 600.0], 0.0)], 5.3, 1200.0);
        assert!(big[0].radius > one[0].radius * 1.8);
    }

    /// Persistence, which is half of what the salience fix is: the operation
    /// mark underneath fades toward its band floor, the alarm does not.
    #[test]
    fn an_alarm_never_fades_below_its_floor() {
        assert!((ink_weight(0.0) - 1.0).abs() < f64::EPSILON);
        assert!((ink_weight(ALARM_HOLD) - 1.0).abs() < f64::EPSILON);
        assert!((ink_weight(1.0) - ALARM_FLOOR).abs() < 1e-9);
        let old = alarm_ink(ink_weight(1.0));
        assert!(
            i32::from(old[0]) - i32::from(old[1]).max(i32::from(old[2])) > 40,
            "an hour-old failure stopped being red: {old:?}"
        );
    }

    /// The cap drops the mildest region, never the worst one.
    #[test]
    fn the_cap_keeps_the_worst_regions() {
        let mut marks = Vec::new();
        for i in 0..(ALARM_CAP + 4) {
            let x = 40.0 + (i % 4) as f64 * 140.0;
            let y = 40.0 + (i / 4) as f64 * 140.0;
            // The last region gets three failures; every other gets one.
            let n = if i + 1 == ALARM_CAP + 4 { 3 } else { 1 };
            for k in 0..n {
                marks.push(failed([x + f64::from(k), y], 0.0));
            }
        }
        let a = alarms(&marks, 6.0, 600.0);
        assert_eq!(a.len(), ALARM_CAP);
        assert_eq!(a[0].count, 3, "the worst region was not kept first");
    }

    /// PRD §7.4: the same frame draws the same bytes.
    #[test]
    fn clustering_is_deterministic() {
        let marks: Vec<Mark> = (0..24)
            .map(|i| {
                failed(
                    [
                        90.0 + f64::from(i % 5) * 37.0,
                        90.0 + f64::from(i % 7) * 41.0,
                    ],
                    f64::from(i) / 48.0,
                )
            })
            .collect();
        let a = alarms(&marks, 7.0, 600.0);
        let mut shuffled = marks.clone();
        shuffled.reverse();
        assert_eq!(a, alarms(&shuffled, 7.0, 600.0));

        let mut c1 = Canvas::new(600, 600, [0, 0, 0]);
        let mut c2 = Canvas::new(600, 600, [0, 0, 0]);
        draw(&mut c1, &a, 7.0);
        draw(&mut c2, &a, 7.0);
        assert_eq!(c1.pixels, c2.pixels);
    }

    /// PRD §11.4's arrival, at the scale of a region.
    ///
    /// > The **arrival** of an attention mark is a brief pulse (≤400ms). That is
    /// > what catches the eye when the operator is not looking at the screen.
    ///
    /// The test is geometric on purpose: a pulse that only changed the ink would
    /// be a second colour channel, and colour is exactly the thing the periphery
    /// cannot see. So the flare has to reach measurably **further** from the
    /// centre while it runs, and the ink must be the same failure red throughout.
    #[test]
    fn an_arrival_flares_by_geometry_and_not_by_colour() {
        let r = 6.0;
        let reach = |pulse: f64| {
            let mut c = Canvas::new(400, 400, [0, 0, 0]);
            let mut a = alarms(&[failed([200.0, 200.0], 0.0)], r, 400.0);
            a[0].pulse = pulse;
            draw(&mut c, &a, r);
            let mut far = 0.0f64;
            // Hue as the two channel ratios. `fade` scales all three channels
            // by one factor and the rasteriser antialiases toward black by
            // another, so both leave `g/r` and `b/r` alone: a change in either
            // is a change of *ink*, which is the thing under test.
            let mut hue = [0.0f64; 2];
            let mut lit = 0.0f64;
            for (i, p) in c.pixels.as_chunks::<3>().0.iter().enumerate() {
                if p == &[0, 0, 0] {
                    continue;
                }
                let dx = (i % 400) as f64 - 200.0;
                let dy = (i / 400) as f64 - 200.0;
                far = far.max(dx.mul_add(dx, dy * dy).sqrt());
                // Only pixels the stroke actually covered; a 3 % edge pixel is
                // a rounding artefact, not a colour.
                if p[0] >= 120 {
                    hue[0] += f64::from(p[1]) / f64::from(p[0]);
                    hue[1] += f64::from(p[2]) / f64::from(p[0]);
                    lit += 1.0;
                }
            }
            (far, [hue[0] / lit.max(1.0), hue[1] / lit.max(1.0)])
        };
        // The ring *expands as it fades*, which is the shape of an onset: it
        // leaves the mark and travels outward. So it is widest in the middle of
        // its 400 ms, not at `pulse == 1`.
        let (rest, rest_hue) = reach(0.0);
        let (mid, mid_hue) = reach(0.35);
        assert!(
            mid > rest * 1.4,
            "the flare reaches {mid:.1} px against {rest:.1} at rest — not a motion cue"
        );
        assert!(
            (mid_hue[0] - rest_hue[0]).abs() < 0.02 && (mid_hue[1] - rest_hue[1]).abs() < 0.02,
            "the pulse smuggled a second colour in: {mid_hue:?} vs {rest_hue:?}"
        );
        // Monotone outward across the window, so the eye sees travel and not a
        // flicker.
        assert!(reach(0.8).0 < mid && reach(0.15).0 > mid);
    }

    /// The claim the whole module rests on: an alarm is *area*, and it is many
    /// times the area of the glyph it surrounds.
    #[test]
    fn an_alarm_is_two_orders_of_magnitude_more_ink_than_a_glyph() {
        let r = 8.0;
        let mut glyph_only = Canvas::new(400, 400, [0, 0, 0]);
        // A fresh mark at a building is drawn at exactly `r` — see
        // `live::draw_mark`, whose size term is 1.0 at age 0, scale 1, count 1.
        live::draw_glyph(
            &mut glyph_only,
            [200.0, 200.0],
            r,
            Glyph::FilledTriangle,
            live::outcome_ink(Outcome::Failed),
        );
        let mut with_alarm = Canvas::new(400, 400, [0, 0, 0]);
        draw(
            &mut with_alarm,
            &alarms(&[failed([200.0, 200.0], 0.0)], r, 400.0),
            r,
        );
        let ink = |c: &Canvas| {
            c.pixels
                .as_chunks::<3>()
                .0
                .iter()
                .filter(|p| p.iter().copied().max().unwrap_or(0) > 0)
                .count()
        };
        let glyph = ink(&glyph_only);
        let alarm = ink(&with_alarm);
        assert!(
            alarm > glyph * 4,
            "alarm inked {alarm} px against the glyph's {glyph}; that is not area"
        );
    }
}
