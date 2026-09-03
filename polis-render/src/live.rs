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

/// The **widest** a contour stroke may be per iso band, in output pixels,
/// measured inward from the boundary.
///
/// The outer contour is the boldest, which is the opposite of the hatch and
/// deliberate: the fringe boundary is the cloud's silhouette, and a silhouette
/// is the only part of any mark that survives being looked at from across the
/// room (PRD §1).
///
/// A ceiling rather than a width, because a fixed width does not survive
/// contact with a small cloud. A three-pixel ring around a ten-pixel blob is
/// 60 % of the blob, so the notation tuned on one large validation cloud
/// behaved as a **fill** on the small ones real territories actually produce —
/// measured at 68–84 % of the banded region inked. See
/// [`CLOUD_CONTOUR_FRACTION`].
pub const CLOUD_CONTOUR_WIDTH: [f64; 3] = [3.0, 2.0, 2.0];

/// A contour is at most this fraction of its own band's characteristic radius.
///
/// The band's radius is estimated from its own pixels — `2 · area / edge`, which
/// is `R` for a disc — so the rule needs nothing from the caller and holds at
/// every zoom. A ring of width `w` on a blob of radius `R` inks about `2w/R` of
/// it, so a tenth keeps the contour near a fifth of the band whatever the size,
/// and [`CLOUD_CONTOUR_WIDTH`] still caps it so a large cloud gets the bold
/// silhouette and not a proportionally enormous one.
pub const CLOUD_CONTOUR_FRACTION: f64 = 0.10;

/// The iso thresholds, in **kernels overlapping here** (ADR-0020).
///
/// Absolute, never normalised against the observed field maximum: normalising
/// collapses every ordinary territory into a single fringe band, which is the
/// mush §10.4 forbids.
///
/// # Why the fringe sits *below* one kernel
///
/// It used to sit above one, at 1.3, on the argument that "two overlapping
/// kernels is the cheapest honest definition of a region rather than a point".
/// Measured on real sessions that argument fails twice over.
///
/// PRD §6.4 asks for a territory to be "**wide and diffuse with three
/// observations**, tightening as evidence accumulates". At 1.3 three
/// observations a bandwidth apart draw **nothing at all**, because no point in
/// the plane has 1.3 kernels over it — so the one case §6.4 names as the
/// uncertainty signal was the one case the layer could not draw.
///
/// And where it did draw, it drew a *sliver*: on three overlaid real sessions
/// the whole banded region came to about a thousand pixels, thin enough that
/// the three-pixel outer contour inked 68–84 % of it. The layer passed every
/// synthetic fill test and behaved as a fill on real evidence, because those
/// tests were run on one large cloud and real territories are small.
///
/// At 0.55 a lone kernel bands out to roughly half its own bandwidth, so one
/// wide uncertain territory reads as one wide uncertain shape; the body and core
/// still need genuine overlap. Measured over 192 frames of six overlaid real
/// sessions, this and [`CLOUD_CONTOUR_FRACTION`] together took the inked share
/// of the banded ground from 68–84 % to **28 %**, and the median luminance of
/// the city showing through from `+40.1` levels to **`0.000`** — the same
/// number it has with no cloud drawn at all.
pub const CLOUD_ISO: [f64; 3] = [0.55, 1.60, 3.20];

/// The band index meaning "outside the fringe" in a per-pixel band map.
pub const NO_BAND: u8 = u8::MAX;

/// The [`thread_slot`] meaning "nobody named an owner for this pixel", which
/// draws in the neutral [`CLOUD_TONES`].
///
/// Outside [`THREAD_HUES`]'s range on purpose, so it can never be a hue.
pub const NO_TINT: u8 = u8::MAX;

/// How much the hatch spacing opens up where two or more territories overlap.
///
/// Overlap is drawn as a **cross**-hatch: the primary direction plus its
/// perpendicular. That is the shape channel saying "two territories claim this
/// ground", and it is deliberately not a colour or a tone, both of which are
/// already spoken for by the band.
///
/// The spacing has to open up or the notation defeats itself. Two directions at
/// the core band's 6 px spacing ink 44 % of the region, and 35 % is where
/// `plan`'s fill assertion draws the line between a texture and a fog. At 1.8×
/// each direction lays down about 14 %, the pair about 26 %, and the base map
/// underneath still reads at its own luminance — measured, not assumed.
pub const CLOUD_OVERLAP_SPACING: f64 = 1.8;

/// How many territories have to reach fringe level at a point before it is
/// drawn as contested ground.
///
/// Two. One thread working hard is dense; two threads in one place is the thing
/// PRD §11.2c fires on, and it is visible here **before** a write collides —
/// which is the whole reason §6.4 calls field addition "the contention signal"
/// rather than "a rendering convenience".
pub const CLOUD_CROWD: u8 = 2;

/// How fast a tweened cloud field chases the world's, per presentation second.
///
/// Six, which is a time constant of about 170 ms: long enough that a territory
/// arriving reads as an arrival rather than a cut, short enough that the field
/// has settled inside a second and the window can stop asking for frames (PRD
/// §13.1's idle budget). The **slow** part of a cloud — PRD §6.3's 90-second
/// contraction half-life — is the world's decay of the kernel weights, and it
/// reaches this layer through the weights rather than through the tween. Slowing
/// the tween to imitate it would smear the two together and make a territory
/// that merely *moved* look like one that was fading.
///
/// > **Interpolate everything.** Events arrive discretely; tween agent
/// > positions, cloud density, and building heights between updates. Cheap, and
/// > it is the entire difference between "alive" and "steppy." (PRD §13)
///
/// The field is tweened rather than the kernels because kernels come and go:
/// PRD §6.3 decays weights and drops them under a floor, so a kernel-by-kernel
/// tween needs identity the world does not promise. Lattice cells always exist,
/// so lerping the lattice handles appearance, disappearance and drift with one
/// rule and no bookkeeping.
pub const CLOUD_TWEEN_RATE: f64 = 6.0;

/// Field level below which a tweened cloud has finished dissipating and its
/// lattice is dropped.
///
/// A tenth of the fringe threshold. Anything below this cannot reach a band, so
/// keeping the lattice alive would only cost the idle budget PRD §13.1 caps at
/// 2 % of one core.
pub const CLOUD_GONE: f32 = 0.13;

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
// Identity — which thread a mark belongs to.
//
// > i'd like one thread to be one color, slightly different to the others,
// > matching that color also in the rail with a rectangle and a matching hover.
//
// This is a *fourth* channel next to PRD §10.1's shape, §10.2's outcome colour
// and position, and it is the one the map did not have: nine threads drew in
// one neutral grey, so the only way to tell whose trail crossed whose was to
// follow it back to an anchor. See [`thread_slot`] for the identity rule and
// [`THREAD_HUES`] for how far apart the hues are and how that was measured.
// ---------------------------------------------------------------------------

/// The identity hue ring: one entry per slot, as authored.
///
/// # What these numbers are
///
/// Twelve hues, evenly spaced around the CIELAB hue circle at a **constant**
/// `L* = 45` and `C* = 25`, rounded to sRGB. Constant lightness is the whole
/// design, and it is forced by PRD §10.3 rather than chosen: the base map is
/// confined to channel 48 and layer 4 gets `97–168`, so a palette that
/// separated its members by *brightness* would either walk out of the agent
/// band or walk into the base map's. Every separation below is therefore hue
/// and chroma, and the ring is authored once and re-levelled per role by
/// [`thread_ink`] — so a trail and a cloud and a rail swatch are the same hue
/// at three brightnesses rather than three colours that happen to look alike.
///
/// # Measured separation, at the luminance each role is actually drawn at
///
/// CIEDE2000 over all 66 pairs, computed by
/// `thread_hues_are_far_enough_apart_at_the_luminance_they_are_drawn_at`:
///
/// | role | peak channel | worst pair | ΔE00 |
/// |---|---:|---|---:|
/// | rail swatch / agent body | 168 | slot 10 ↔ 11 | **9.80** |
/// | anchor | 162 | 10 ↔ 11 | 9.54 |
/// | trail | 144 | 0 ↔ 11 | 8.84 |
/// | tether | 130 | 0 ↔ 11 | 7.99 |
/// | cloud body | 80 | 0 ↔ 11 | 6.17 |
/// | cloud core | 84 | 0 ↔ 11 | 5.82 |
/// | cloud fringe | 74 | 0 ↔ 11 | **5.46** |
///
/// The cloud band is the worst case and cannot be otherwise: `49–84` is a
/// twelfth of the range the agent band gets, and chroma shrinks with it. 5.46
/// is several times the ~1.0 just-noticeable difference and is what that band
/// physically affords; the layer that has to carry identity from across the
/// room is the rail swatch and the agent body, at 9.8.
///
/// # Twelve, and what twelve costs
///
/// Slots are handed out by hashing, so two threads *can* land on one hue —
/// with nine threads on screen and twelve slots that is about three of the
/// thirty-six pairs. That is the price of [`thread_slot`]'s stability rule and
/// it is paid on purpose: raising the ring to twenty slots would only take the
/// expected number of distinct colours among nine threads from 6.6 to 7.4
/// while cutting the worst pair from 9.8 to 5.4 — trading a distinction the
/// operator can make for one they cannot. PRD §11.4 covers the collision:
/// colour is never the only channel, and a shared hue is disambiguated by the
/// rail's own thread name, by the row's swatch sitting next to it, and by the
/// two threads' clouds being in different places.
pub const THREAD_HUES: [Rgb; 12] = [
    [147, 91, 99],  // 14°  rose
    [144, 94, 80],  // 44°  terracotta
    [132, 101, 67], // 74°  amber-brown
    [113, 108, 65], // 104° olive
    [90, 113, 74],  // 134° moss
    [65, 117, 92],  // 164° jade
    [39, 118, 114], // 194° teal
    [30, 116, 133], // 224° cyan-blue
    [56, 112, 145], // 254° steel blue
    [90, 106, 147], // 284° indigo
    [119, 98, 138], // 314° violet
    [139, 92, 121], // 344° magenta
];

/// FNV-1a's 32-bit offset basis, written out (ADR-0029).
const FNV_OFFSET: u32 = 2_166_136_261;
/// FNV-1a's 32-bit prime.
const FNV_PRIME: u32 = 16_777_619;

/// The hue slot a thread owns, from its **own identity** and nothing else.
///
/// # Why a hash and not a counter
///
/// An index into the live thread list is free and wrong. The list is sorted and
/// re-sorted as threads arrive, finish and are retired, so the colour of a
/// thread the operator is watching changes when an unrelated thread starts —
/// and the one thing this channel is for is the operator learning *"the blue
/// one is the refactor"* inside a minute. A palette that reshuffles destroys
/// that faster than no palette at all, because it teaches something false.
///
/// So the slot is a pure function of the thread's id: it is fixed before the
/// thread's first event, it is the same in the window and in the headless
/// renderer, it survives a restart, and no other thread's lifetime can move it.
///
/// FNV-1a over the id's bytes, written out rather than taken from
/// `std::hash::DefaultHasher`, whose algorithm is explicitly not stable across
/// Rust releases (ADR-0029) — a toolchain bump must not repaint the city.
#[must_use]
pub fn thread_slot(id: &str) -> u8 {
    let mut h = FNV_OFFSET;
    for b in id.as_bytes() {
        h ^= u32::from(*b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    (h % THREAD_HUES.len() as u32) as u8
}

/// The authored hue for a slot. Out-of-range slots wrap, so no caller can panic
/// on an identity it did not compute itself.
#[must_use]
pub fn thread_hue(slot: u8) -> Rgb {
    THREAD_HUES[slot as usize % THREAD_HUES.len()]
}

/// A thread's hue at a role's own brightness.
///
/// `role` is one of this module's band constants — [`AGENT_BODY`],
/// [`AGENT_TRAIL`], [`AGENT_TETHER`], [`AGENT_ANCHOR`], a [`CLOUD_TONES`] entry
/// — and only its **peak channel** is read. The result is the thread's hue
/// scaled so its own peak lands there, which is exactly the rule
/// `polis_app::palette`'s band clamp already applies: brightness is the peak
/// channel, so re-pegging the peak moves a colour between layers without
/// touching its hue.
///
/// That is what keeps one thread one colour *across* layers. Its cloud is dim
/// because clouds are dim (PRD §10.3), not because a different colour was
/// chosen for it, and the operator reads "same thread" off a cloud and a trail
/// that are eleven levels apart in brightness.
///
/// Integer arithmetic, deliberately: this runs per mark per frame and PRD §7.4
/// wants the same bytes on every machine.
#[must_use]
pub fn thread_ink(slot: u8, role: Rgb) -> Rgb {
    thread_ink_at(slot, role, IDENTITY_CHROMA)
}

/// How far a thread's non-peak channels are pulled down from its peak.
///
/// `0` leaves the hue as authored; `1` would drive every non-peak channel to
/// zero. Chroma is the one identity channel that is **free** here: PRD §10.3
/// budgets *brightness*, and [`thread_ink`] already pegs the peak channel to the
/// role's own, so deepening the hue spends nothing the layer owns.
///
/// It is not cosmetic. The operator's report was *"i cant distinguish threads
/// from eachother"* on a map whose clouds are drawn at channel 56–84, where a
/// muted hue and a grey are nearly the same thing. Their trails and tethers had
/// just been hidden to quiet the map, and those were carrying most of the
/// ownership signal — so the ambient shape had to carry it instead.
pub const IDENTITY_CHROMA: u32 = 45;

/// [`thread_ink`] with an explicit chroma lift, in percent.
#[must_use]
pub fn thread_ink_at(slot: u8, role: Rgb, chroma: u32) -> Rgb {
    let peak = u32::from(role.iter().copied().max().unwrap_or(0));
    let hue = thread_hue(slot);
    let m = u32::from(hue.iter().copied().max().unwrap_or(0)).max(1);
    let mut out = [0u8; 3];
    for (o, c) in out.iter_mut().zip(hue) {
        let scaled = (u32::from(c) * peak + m / 2) / m;
        // Deepen everything that is not the peak, so the hue reads at a
        // brightness where a desaturated one would not. The peak is untouched,
        // which is what keeps the band ladder — and §10.3's budget — exact.
        let deepened = if scaled >= peak {
            scaled
        } else {
            let gap = peak - scaled;
            scaled.saturating_sub(gap * chroma / 100)
        };
        *o = deepened.min(peak) as u8;
    }
    out
}

/// A thread's three cloud iso tones (PRD §10.4), fringe → body → core.
///
/// The tones keep [`CLOUD_TONES`]'s brightnesses exactly — the band ladder is a
/// contrast budget and identity does not get to spend it — and only their hue
/// changes.
#[must_use]
pub fn thread_cloud_tones(slot: u8) -> [Rgb; 3] {
    [
        thread_ink(slot, CLOUD_TONES[0]),
        thread_ink(slot, CLOUD_TONES[1]),
        thread_ink(slot, CLOUD_TONES[2]),
    ]
}

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

/// PRD §11.4's arrival pulse from an age in seconds: `1` at onset, `0` at
/// [`PULSE_SECS`].
///
/// Here rather than in [`crate::frame`] because the **window** needs it too, and
/// two copies of "how long is an arrival" is how one surface ends up pulsing for
/// twice as long as another.
#[must_use]
pub fn pulse_at(age_secs: f64) -> f64 {
    if age_secs >= PULSE_SECS || age_secs < 0.0 {
        0.0
    } else {
        1.0 - age_secs / PULSE_SECS
    }
}

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

/// How many segments a tether's bow is drawn as.
///
/// Eight, which is what the curve needs and what the dash divides evenly — and
/// no more, because a tether is drawn once per worker and one thread on this
/// machine has a hundred of them.
const TETHER_STEPS: usize = 8;

/// The most tethers one thread draws, running workers first.
///
/// Sixteen is where a fan stops being countable. Past it the answer to *"which
/// workers are this thread's"* is a texture rather than a list, and the exact
/// count is in the rail anyway — see [`draw_tether`] for why the tether is now
/// an answer to a question instead of an ambient decoration.
///
/// Public because two renderers draw this notation and PRD §10 allows one
/// visual language: `polis_app::mapview` and [`crate::frame`] rank workers the
/// same way and stop at the same number, so the window and a recorded frame
/// show the same sixteen hands.
pub const TETHERS_PER_THREAD: usize = 16;

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
pub(crate) const MIN_STROKE: f64 = 2.0;

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
    /// Reach, in device pixels. This is PRD §6.4's bandwidth —
    /// `base * (1 / sqrt(effective_n))`, clamped — projected to the screen, and
    /// it is the **only** thing that makes a cloud soft. Three observations put
    /// down three wide kernels that sum to a broad fringe with no core; thirty
    /// put down thirty narrow ones that stack into a tight core. Nothing else
    /// in this module encodes uncertainty, because nothing else has to.
    pub radius: f64,
    /// Weight after PRD §6.3's decay.
    pub weight: f64,
    /// Which territory dropped it.
    ///
    /// Kernels sharing an id are **one thread's field**. The layer needs the
    /// distinction for one reason and it is PRD §6.4's: *"overlap is field
    /// addition — two territories overlapping is just a denser region, which is
    /// exactly the contention signal."* Density alone cannot tell "one thread
    /// working hard here" from "two threads in the same place", and those are
    /// not the same news. Summing per thread first and counting how many
    /// threads reach fringe level at each point tells them apart, and costs one
    /// scratch lattice.
    pub thread: u16,
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
    /// Whose trail — [`thread_slot`] of the owning thread.
    pub tint: u8,
}

/// A worker tied back to its thread, drawn **only for the thread the operator
/// is asking about**. See [`draw_tether`].
#[derive(Debug, Clone, Copy)]
pub struct Tether {
    /// The thread's anchor.
    pub anchor: Px,
    /// The worker.
    pub worker: Px,
    /// Whether the worker is still running. A finished worker's tether is drawn
    /// dimmer, thinner and dashed rather than dropped, so a thread does not
    /// appear to shed limbs while the operator is looking straight at it.
    pub running: bool,
    /// How far, and which way, this tether bows off the straight line, in
    /// `[-1, 1]`.
    ///
    /// Workers of one thread are usually working in **one place**, so their
    /// tethers share both endpoints and stack into a single opaque ribbon that
    /// says only "this thread delegates". Fanning them apart turns that ribbon
    /// back into a countable number of hands, which is the thing worth knowing
    /// once the operator has asked.
    pub spread: f64,
    /// Whose hand — [`thread_slot`] of the thread this tether belongs to.
    ///
    /// One thread's tethers are on screen at a time, so this no longer has to
    /// separate one fan from another. It still has to match: the line and the
    /// worker at the end of it and the rail's swatch are one colour, which is
    /// how the operator confirms the line landed where they thought.
    pub tint: u8,
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
/// # Why a mark has no [`thread_slot`], when everything else does
///
/// Identity colours the thread's *continuous* things — its cloud, its trail,
/// its tethers, its agent body — and stops at the mark. A mark is not the
/// thread, it is one thing the thread did, and PRD §10 opens by forbidding the
/// conflation of channels:
///
/// > **Shape encodes what, colour encodes how it went. Never conflate them.**
///
/// There is exactly one colour slot on a glyph and two candidates for it, so
/// this is a ranking and not a preference. *"How it went"* outranks *"whose it
/// is"*: an operator scanning the map for red is asking which work failed, and
/// a failure repainted in its owner's hue is a failure they cannot see. Whose
/// it is, they can already read — off the trail it sits on, the cloud it sits
/// in, and the agent that left it, all three of which carry the hue.
///
/// The same reasoning keeps [`Agent::outcome`] on the agent's centre disc while
/// its body takes the thread hue: two marks, two channels, no pixel asked to
/// mean both.
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
    /// Whose agent — [`thread_slot`] of the thread it belongs to.
    ///
    /// It colours the **body**, never the centre disc: [`Mark`] and this
    /// struct's [`Agent::outcome`] keep PRD §10.2's channel. See
    /// [`draw_agent`].
    pub tint: u8,
}

/// Which of PRD §11.2's three states a mark is.
///
/// Four variants for three states because *done, unverified* is really "needs
/// review" and persists — PRD §17's open question 2, which
/// [`polis_world::attention::AttentionKind::rank`] answers from the corpus: it
/// is **52.7 %** of thread-samples across the operator's six largest sessions,
/// so it is the *modal* way a session ends and cannot be a colour-only variant
/// of its opposite. It is not a fourth state, and it does get a shape of its
/// own:
///
/// | variant | silhouette | reads as |
/// |---|---|---|
/// | [`MarkKind::DoneVerified`] | ring with a filled centre | sealed |
/// | [`MarkKind::DoneUnverified`] | ring, hollow, inside a **broken** outer ring | open |
///
/// Closed versus open, which is the distinction with the colour thrown away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkKind {
    /// (a) Needs decision — a standing pin, amber, persistent.
    NeedsDecision,
    /// (b) Done, verified — a ring with a filled centre, teal, decaying.
    DoneVerified,
    /// (b′) Done, unverified — hollow, inside a broken outer ring. Persists.
    DoneUnverified,
    /// (c) Contention — a link joining two threads across the map.
    Contention,
}

impl MarkKind {
    /// PRD §11.1's ordering, as the renderer sees it. Lower is more important.
    ///
    /// The same numbers [`polis_world::attention::AttentionKind::rank`] returns,
    /// restated here because the renderer uses them for a second thing the world
    /// does not care about: **overdraw order**. §11.1 has to hold in the picture
    /// as well as in the list, so marks are painted worst-last and a contention
    /// end is never hidden under a pin.
    #[must_use]
    pub fn rank(self) -> u8 {
        match self {
            Self::Contention => 0,
            Self::NeedsDecision => 1,
            Self::DoneUnverified => 2,
            Self::DoneVerified => 3,
        }
    }
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
    /// Escalation in `[0, 1]` —
    /// [`polis_world::attention::Attention::urgency`], `0` at arrival and `1`
    /// after five minutes of nobody dealing with it.
    ///
    /// Spent on **area**, never on colour or on motion: the pulse owns motion
    /// and it is over in 400 ms, so this is the only channel that separates a
    /// pin raised a second ago from one that has stood since lunch. A mark that
    /// has been ignored is physically bigger, which survives distance and
    /// survives greyscale.
    pub urgency: f64,
    /// Whether [`AttentionMark::at`] is where the work actually is.
    ///
    /// `false` when the mark had to fall back to the civic square because the
    /// thread has no territory and no trail the city has geometry for. PRD
    /// §11.2a's pin stands "above the building **or district**" and an
    /// unconverged thread has neither — but *"an agent blocked on a human"* is
    /// the one state that must never fail to draw, so it is drawn anyway and the
    /// notation says how sure it is. See
    /// [`crate::salience::ALARM_UNSITED`].
    pub sited: bool,
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
    /// [`thread_slot`] per [`CloudKernel::thread`], so a cloud is drawn in its
    /// own thread's hue.
    ///
    /// A side table rather than a field on the kernel, because
    /// `CloudKernel::thread` is a **per-frame group index** the field sampler
    /// sorts on, and identity is not: two threads may hash to one hue and must
    /// still be summed as two territories, or `crowd` — PRD §6.4's contention
    /// signal — would report contested ground as one busy thread. Colour is
    /// looked up through this table exactly once, at paint time.
    pub cloud_tints: Vec<u8>,
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
    /// Regions of the map that are failing — PRD §10.2's colour channel given
    /// the area it needs to be seen from across the room. See
    /// [`crate::salience`].
    pub alarms: Vec<crate::salience::Alarm>,
    /// The agents themselves.
    pub agents: Vec<Agent>,
    /// Attention marks.
    pub attention: Vec<AttentionMark>,
    /// PRD §6.2's status rail: what has no place on the map. Chrome, drawn
    /// outside the map frame, and **never empty when something was dropped**.
    pub rail: Vec<RailRow>,
    /// Why the map has the clouds it has — and, more usefully, why it does not
    /// have the ones it does not. See [`CloudCensus`].
    pub cloud: CloudCensus,
}

/// What [`polis_world::territory::select_clouds`] decided, carried onto the
/// frame so the picture can say it out loud.
///
/// The selection has always returned these counts and nothing has ever read
/// them, which is how the map arrived at *"nine threads, no clouds"* with no
/// way to ask why. Every thread lands in exactly one of these buckets, so
/// `shown + unplaced + dormant + capped` is the thread count, and a zero in
/// `shown` always has a reason beside it.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CloudCensus {
    /// Threads whose territory got a cloud.
    pub shown: usize,
    /// Kernels those territories contributed, bridges included. Zero here with
    /// a non-zero `shown` means the field was empty for a reason the selection
    /// could not see — a dead weight, or a camera the territory is off.
    pub kernels: usize,
    /// The widest kernel this frame put on the canvas, in **output pixels**.
    ///
    /// The selection can hand the layer six territories and the picture still
    /// show nothing, because PRD §6.4's bandwidth is a world-space quantity and
    /// the camera has the last word on it. A territory two world units across
    /// is a legible cloud at the district tier and a sub-pixel smudge fitted to
    /// the whole city, and the difference is a factor the operator controls
    /// with the scroll wheel. Which is why this is a *census* field and not a
    /// debug print: `shown: 6, widest: 0.8 px` is the whole diagnosis of an
    /// empty sky, and no other number in the system says it.
    pub widest_px: f64,
    /// Threads with no claim, no lobes, or no kernels: not converged.
    pub unplaced: usize,
    /// Converged but quiet longer than the policy's dormancy window.
    pub dormant: usize,
    /// Would have been drawn and lost to PRD §10.4's cap.
    pub capped: usize,
}

impl CloudCensus {
    /// Threads accounted for.
    #[must_use]
    pub fn threads(&self) -> usize {
        self.shown + self.unplaced + self.dormant + self.capped
    }

    /// Whether anything was withheld. `false` means every thread on the map has
    /// its cloud and the layer owes the operator no explanation.
    #[must_use]
    pub fn withheld(&self) -> bool {
        self.unplaced + self.dormant + self.capped > 0
    }

    /// Whether the clouds that *were* selected are too small to draw.
    ///
    /// A contour needs a couple of pixels of radius before it is a stroke
    /// rather than a dot; below this module's minimum stroke width the layer is
    /// doing everything right and producing nothing visible.
    #[must_use]
    pub fn sub_pixel(&self) -> bool {
        self.shown > 0 && self.widest_px < MIN_STROKE
    }

    /// The reason, in short phrases — one per line for a narrow panel, joined
    /// by [`Self::reason`] for a wide one.
    ///
    /// The count comes first and the dominant reason next; nothing is omitted,
    /// because a diagnostic that rounds is a diagnostic that lies. Every phrase
    /// fits the status rail's own column width, which is what stops the fix for
    /// one silence from becoming a line that runs off its own panel.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        let mut parts = vec![format!("{} CLOUDS", self.shown)];
        if self.sub_pixel() {
            parts.push(format!("{:.1} PX: ZOOM IN", self.widest_px));
        }
        let mut rest = [
            (self.unplaced, "UNCONVERGED"),
            (self.dormant, "DORMANT"),
            (self.capped, "OVER CAP"),
        ];
        rest.sort_by_key(|(n, _)| std::cmp::Reverse(*n));
        for (n, why) in rest {
            if n > 0 {
                parts.push(format!("{n} {why}"));
            }
        }
        parts
    }

    /// The same, on one line: *"3 CLOUDS · 6 UNCONVERGED"*. For the window's
    /// status strip, which has the width for it.
    #[must_use]
    pub fn reason(&self) -> String {
        self.lines().join(" · ")
    }
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
    /// Threads the cloud policy withheld a cloud from, and why.
    ///
    /// A thread can be perfectly well placed — an anchor, a trail, marks on
    /// real buildings — and still have no cloud, because PRD §10.4's selection
    /// dropped it. That is a different statement from [`Self::UnplacedThread`]
    /// and it needs its own row: *"no clouds"* was a silent state, and an
    /// operator looking at nine threads and an empty sky could not tell an
    /// un-converged territory from a dormant one from the cap.
    NoCloud,
}

impl RailKind {
    /// The word the rail prints for this kind.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::UnplacedThread => "NO TERRITORY",
            Self::UnplacedOps => "UNPLACED OPS",
            Self::UnattributedWorkers => "UNATTRIBUTED",
            Self::NoCloud => "NO CLOUD",
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
///
/// This is the un-tweened path: the field is sampled and drawn in one go, which
/// is what a still image wants. An animation wants [`CloudTween`] between the
/// two halves.
pub fn draw_clouds(canvas: &mut Canvas, frame: &LiveFrame) -> Duration {
    let start = Instant::now();
    let rows = (frame.map_height as usize).min(canvas.height);
    if rows == 0 || canvas.width == 0 {
        return start.elapsed();
    }
    let Some(field) = CloudField::sample(&frame.clouds, canvas.width, rows) else {
        return start.elapsed();
    };
    paint_cloud_bands(canvas, &field.bands_tinted(&frame.cloud_tints));
    start.elapsed()
}

/// Layer 3, from a field somebody else already sampled — normally a
/// [`CloudTween`]'s.
///
/// `tints` is `LiveFrame::cloud_tints`; an empty slice draws the neutral tones.
pub fn draw_cloud_field(canvas: &mut Canvas, field: &CloudField, tints: &[u8]) -> Duration {
    let start = Instant::now();
    paint_cloud_bands(canvas, &field.bands_tinted(tints));
    start.elapsed()
}

/// The summed density of a set of territories on a lattice, and how many of
/// them reach fringe level at each cell.
///
/// This is PRD §10.4's *"offscreen R16F density texture"* at a size that fits
/// the territories rather than the screen, and [`CloudField::bands`] is the
/// threshold pass. Keeping the two apart is what lets [`CloudTween`] interpolate
/// the field — the thing §13 asks to be tweened — rather than interpolating the
/// picture of it, which would cross-fade two sets of contours into mush.
///
/// # The second channel, and why it is not just more density
///
/// `density` is the sum over every kernel, which is PRD §6.4's field addition
/// and therefore already makes overlap denser. It cannot, on its own, tell
/// **one** thread working hard in a corner from **two** threads standing on
/// each other: both are a high number. `crowd` counts how many territories
/// separately reach fringe level at a cell, so the second reading gets a
/// notation of its own ([`paint_cloud_bands`] crosses the hatch) and the first
/// does not.
#[derive(Debug, Clone, PartialEq)]
pub struct CloudField {
    /// Left edge of the covered rectangle, in canvas pixels.
    pub x0: usize,
    /// Top edge of the covered rectangle, in canvas pixels.
    pub y0: usize,
    /// Width of the covered rectangle, in canvas pixels.
    pub width: usize,
    /// Height of the covered rectangle, in canvas pixels.
    pub height: usize,
    /// Lattice columns.
    pub grid_x: usize,
    /// Lattice rows.
    pub grid_y: usize,
    /// Summed density, row-major, `grid_x * grid_y`.
    ///
    /// Unbounded on purpose: `N` overlapping kernels sum to about `N`, and
    /// [`CLOUD_ISO`] is read against a fixed per-kernel reference rather than
    /// against this array's maximum (ADR-0020).
    pub density: Vec<f32>,
    /// How many territories reach [`CLOUD_ISO`]`[0]` here, row-major.
    ///
    /// Fractional only because [`CloudTween`] lerps it; `sample` produces whole
    /// numbers.
    pub crowd: Vec<f32>,
    /// Which [`CloudKernel::thread`] contributes the most density here.
    ///
    /// The cell's *owner*, and the only thing a per-thread cloud colour can be
    /// keyed on: `density` is a sum over threads by construction (PRD §6.4's
    /// field addition) and a sum has no hue. Argmax rather than a blend,
    /// because blending two territories' hues would invent a third thread; the
    /// place where they genuinely overlap is already drawn as contested ground
    /// by the crossed hatch, which is a shape channel and stays one.
    ///
    /// Discrete, so [`CloudTween`] carries it rather than lerping it — the
    /// midpoint between thread 2 and thread 5 is not thread 3.
    pub owner: Vec<u16>,
}

impl CloudField {
    /// Sums the kernels of every territory over their own bounding rectangle,
    /// clipped to a `width * rows` canvas.
    ///
    /// Returns `None` when the kernels are all dead or fall entirely outside.
    #[must_use]
    pub fn sample(kernels: &[CloudKernel], width: usize, rows: usize) -> Option<Self> {
        let live = |k: &CloudKernel| k.weight > 0.0 && k.radius > 0.0;
        let mut lo = [f64::INFINITY; 2];
        let mut hi = [f64::NEG_INFINITY; 2];
        for k in kernels.iter().filter(|k| live(k)) {
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

        // The field is evaluated on a coarse lattice and bilinearly resampled,
        // which is PRD §10.4's offscreen texture at a size that fits the region
        // rather than the screen. A contour then comes out as a smooth curve
        // instead of a staircase of lattice cells.
        let grid_x = (w / 3).clamp(2, 256);
        let grid_y = (h / 3).clamp(2, 256);
        let sx = w as f64 / grid_x as f64;
        let sy = h as f64 / grid_y as f64;
        let n = grid_x * grid_y;
        let mut density = vec![0.0f32; n];
        let mut crowd = vec![0.0f32; n];
        // Who owns each cell, and by how much — the running argmax over the
        // per-thread sums the scratch lattice already computes, so ownership
        // costs one comparison per written cell and no second pass.
        let mut owner = vec![0u16; n];
        let mut owned = vec![0.0f32; n];
        // One thread's field at a time, into a scratch lattice that is zeroed
        // again over the same span it was written. Reusing one buffer is what
        // keeps the cost the sum of the kernels' areas rather than
        // `threads × lattice`.
        let mut scratch = vec![0.0f32; n];

        let mut order: Vec<usize> = (0..kernels.len()).filter(|i| live(&kernels[*i])).collect();
        order.sort_by_key(|i| kernels[*i].thread);
        let fringe = CLOUD_ISO[0] as f32;

        let mut i = 0;
        while i < order.len() {
            let thread = kernels[order[i]].thread;
            let (mut tx0, mut ty0, mut tx1, mut ty1) = (grid_x, grid_y, 0usize, 0usize);
            let mut j = i;
            while j < order.len() && kernels[order[j]].thread == thread {
                let k = &kernels[order[j]];
                let gx0 =
                    (((k.at[0] - k.radius - x0 as f64) / sx).floor().max(0.0) as usize).min(grid_x);
                let gx1 = ((((k.at[0] + k.radius - x0 as f64) / sx).ceil().max(0.0) as usize) + 1)
                    .min(grid_x);
                let gy0 =
                    (((k.at[1] - k.radius - y0 as f64) / sy).floor().max(0.0) as usize).min(grid_y);
                let gy1 = ((((k.at[1] + k.radius - y0 as f64) / sy).ceil().max(0.0) as usize) + 1)
                    .min(grid_y);
                for gy in gy0..gy1 {
                    let py = (gy as f64 + 0.5).mul_add(sy, y0 as f64);
                    let dy = (py - k.at[1]) / k.radius;
                    for gx in gx0..gx1 {
                        let px = (gx as f64 + 0.5).mul_add(sx, x0 as f64);
                        let dx = (px - k.at[0]) / k.radius;
                        scratch[gy * grid_x + gx] +=
                            (k.weight * kernel(dx.mul_add(dx, dy * dy))) as f32;
                    }
                }
                tx0 = tx0.min(gx0);
                ty0 = ty0.min(gy0);
                tx1 = tx1.max(gx1);
                ty1 = ty1.max(gy1);
                j += 1;
            }
            for gy in ty0..ty1 {
                for gx in tx0..tx1 {
                    let c = gy * grid_x + gx;
                    let v = scratch[c];
                    if v > 0.0 {
                        // PRD §6.4: overlap is field addition. The sum is the
                        // density; the count is what says whose.
                        density[c] += v;
                        if v >= fringe {
                            crowd[c] += 1.0;
                        }
                        if v > owned[c] {
                            owned[c] = v;
                            owner[c] = thread;
                        }
                        scratch[c] = 0.0;
                    }
                }
            }
            i = j;
        }

        Some(Self {
            x0,
            y0,
            width: w,
            height: h,
            grid_x,
            grid_y,
            density,
            crowd,
            owner,
        })
    }

    /// Whether the two fields cover the same rectangle at the same lattice
    /// resolution, and can therefore be lerped cell by cell.
    #[must_use]
    pub fn aligned_with(&self, other: &Self) -> bool {
        self.x0 == other.x0
            && self.y0 == other.y0
            && self.width == other.width
            && self.height == other.height
            && self.grid_x == other.grid_x
            && self.grid_y == other.grid_y
    }

    /// The largest value in the field. One full-weight kernel peaks just under
    /// 1.0, so this reads directly as "kernels deep at the hottest point".
    #[must_use]
    pub fn peak(&self) -> f32 {
        self.density.iter().copied().fold(0.0, f32::max)
    }

    /// The value at a canvas pixel, bilinearly off the lattice.
    #[must_use]
    pub fn at(&self, x: f64, y: f64) -> f32 {
        let sx = self.width as f64 / self.grid_x as f64;
        let sy = self.height as f64 / self.grid_y as f64;
        let fx = (((x - self.x0 as f64) / sx) - 0.5).clamp(0.0, (self.grid_x - 1) as f64);
        let fy = (((y - self.y0 as f64) / sy) - 0.5).clamp(0.0, (self.grid_y - 1) as f64);
        bilinear(&self.density, self.grid_x, self.grid_y, fx, fy)
    }

    /// The pixel rectangle this field still has cloud in, if any.
    ///
    /// "Still" is [`CLOUD_GONE`]: below it a cell cannot reach a band however
    /// the thresholds move, so it is ground the cloud has left.
    #[must_use]
    fn live_bounds(&self) -> Option<(usize, usize, usize, usize)> {
        let sx = self.width as f64 / self.grid_x as f64;
        let sy = self.height as f64 / self.grid_y as f64;
        let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0usize, 0usize);
        for (i, d) in self.density.iter().enumerate() {
            if *d <= CLOUD_GONE {
                continue;
            }
            let (gx, gy) = (i % self.grid_x, i / self.grid_x);
            // One cell either side, because the lattice is resampled bilinearly
            // and a cell's influence reaches its neighbours' centres.
            let a = (gx as f64 - 1.0).mul_add(sx, self.x0 as f64).max(0.0) as usize;
            let b = (gy as f64 - 1.0).mul_add(sy, self.y0 as f64).max(0.0) as usize;
            let c = (gx as f64 + 2.0).mul_add(sx, self.x0 as f64).max(0.0) as usize;
            let d = (gy as f64 + 2.0).mul_add(sy, self.y0 as f64).max(0.0) as usize;
            x0 = x0.min(a);
            y0 = y0.min(b);
            x1 = x1.max(c.min(self.x0 + self.width));
            y1 = y1.max(d.min(self.y0 + self.height));
        }
        (x1 > x0 && y1 > y0).then_some((x0, y0, x1, y1))
    }

    /// An empty field over the union of this field's live ground and `other`'s
    /// whole rectangle, at this module's usual lattice resolution.
    #[must_use]
    fn union_with(&self, other: &Self) -> Self {
        let (mut x0, mut y0, mut x1, mut y1) = (
            other.x0,
            other.y0,
            other.x0 + other.width,
            other.y0 + other.height,
        );
        if let Some((ax, ay, bx, by)) = self.live_bounds() {
            x0 = x0.min(ax);
            y0 = y0.min(ay);
            x1 = x1.max(bx);
            y1 = y1.max(by);
        }
        let (w, h) = (x1 - x0, y1 - y0);
        let grid_x = (w / 3).clamp(2, 256);
        let grid_y = (h / 3).clamp(2, 256);
        Self {
            x0,
            y0,
            width: w,
            height: h,
            grid_x,
            grid_y,
            density: vec![0.0; grid_x * grid_y],
            crowd: vec![0.0; grid_x * grid_y],
            owner: vec![0; grid_x * grid_y],
        }
    }

    /// Resamples this field onto `to`'s rectangle and lattice.
    ///
    /// Both grids live in the same canvas-pixel space, so this is a plain
    /// bilinear lookup through pixel coordinates. Cells of `to` that this field
    /// does not cover come back zero, which is the right answer: the cloud was
    /// not there.
    ///
    /// `owner` is a **label** and is resampled by nearest neighbour, never
    /// bilinearly: half way between thread 2 and thread 5 is not thread 3.
    #[must_use]
    fn resampled_onto(&self, to: &Self) -> (Vec<f32>, Vec<f32>, Vec<u16>) {
        let n = to.grid_x * to.grid_y;
        let mut density = vec![0.0f32; n];
        let mut crowd = vec![0.0f32; n];
        let mut owner = vec![0u16; n];
        let sx = to.width as f64 / to.grid_x as f64;
        let sy = to.height as f64 / to.grid_y as f64;
        let (msx, msy) = (
            self.width as f64 / self.grid_x as f64,
            self.height as f64 / self.grid_y as f64,
        );
        for gy in 0..to.grid_y {
            let py = (gy as f64 + 0.5).mul_add(sy, to.y0 as f64);
            let fy = ((py - self.y0 as f64) / msy) - 0.5;
            if fy < -1.0 || fy > self.grid_y as f64 {
                continue;
            }
            let fy = fy.clamp(0.0, (self.grid_y - 1) as f64);
            for gx in 0..to.grid_x {
                let px = (gx as f64 + 0.5).mul_add(sx, to.x0 as f64);
                let fx = ((px - self.x0 as f64) / msx) - 0.5;
                if fx < -1.0 || fx > self.grid_x as f64 {
                    continue;
                }
                let fx = fx.clamp(0.0, (self.grid_x - 1) as f64);
                let c = gy * to.grid_x + gx;
                density[c] = bilinear(&self.density, self.grid_x, self.grid_y, fx, fy);
                crowd[c] = bilinear(&self.crowd, self.grid_x, self.grid_y, fx, fy);
                let (nx, ny) = (fx.round() as usize, fy.round() as usize);
                owner[c] =
                    self.owner[ny.min(self.grid_y - 1) * self.grid_x + nx.min(self.grid_x - 1)];
            }
        }
        (density, crowd, owner)
    }

    /// Thresholds the field into [`CLOUD_ISO`]'s bands, per canvas pixel.
    ///
    /// The clouds come out in this module's neutral [`CLOUD_TONES`]. Callers
    /// that know whose territory is whose pass the identity table to
    /// [`Self::bands_tinted`] instead.
    #[must_use]
    pub fn bands(&self) -> BandMap {
        self.bands_tinted(&[])
    }

    /// [`Self::bands`], with each thread's cloud in its own hue.
    ///
    /// `tints` is indexed by [`CloudKernel::thread`] and holds
    /// [`thread_slot`]s — `LiveFrame::cloud_tints`. An empty slice, or an index
    /// past its end, falls back to the neutral tones, so a caller that has no
    /// identity to offer gets exactly the picture it got before.
    #[must_use]
    pub fn bands_tinted(&self, tints: &[u8]) -> BandMap {
        let (w, h) = (self.width, self.height);
        let sx = w as f64 / self.grid_x as f64;
        let sy = h as f64 / self.grid_y as f64;
        let lx = (self.grid_x - 1) as f64;
        let ly = (self.grid_y - 1) as f64;
        let mut cells = vec![NO_BAND; w * h];
        let mut crowd = vec![0u8; w * h];
        let mut owner = vec![0u8; w * h];
        // One territory on the map is the ordinary case, and then the second
        // lattice is all zeroes and resampling it per pixel is pure waste.
        let contested = self
            .crowd
            .iter()
            .any(|c| *c >= f32::from(CLOUD_CROWD) - 0.5);
        for y in 0..h {
            let fy = (((y as f64 + 0.5) / sy) - 0.5).clamp(0.0, ly);
            for x in 0..w {
                let fx = (((x as f64 + 0.5) / sx) - 0.5).clamp(0.0, lx);
                let d = bilinear(&self.density, self.grid_x, self.grid_y, fx, fy);
                if let Some(band) = iso_band(f64::from(d)) {
                    cells[y * w + x] = band as u8;
                    // Nearest cell, not bilinear: an owner is a label.
                    let g = fy.round() as usize * self.grid_x + fx.round() as usize;
                    owner[y * w + x] = self
                        .owner
                        .get(g)
                        .and_then(|t| tints.get(*t as usize))
                        .copied()
                        .unwrap_or(NO_TINT);
                    if contested {
                        let c = bilinear(&self.crowd, self.grid_x, self.grid_y, fx, fy);
                        // Round rather than floor: the lerp between one
                        // territory and two spends half its time nearer each, so
                        // the cross-hatch arrives at the halfway point instead
                        // of waiting for the very last frame.
                        crowd[y * w + x] = c.round().clamp(0.0, 255.0) as u8;
                    }
                }
            }
        }
        BandMap {
            x0: self.x0,
            y0: self.y0,
            width: w,
            height: h,
            cells,
            crowd,
            tint: owner,
        }
    }
}

/// The field between world updates (PRD §13's *"interpolate everything"*).
///
/// # Why the lattice and not the kernels
///
/// Tweening kernel by kernel needs kernel identity, and the world does not
/// promise it: PRD §6.3 decays weights and drops a kernel once it falls under a
/// floor, so the set changes shape between snapshots. Lattice cells always
/// exist. Lerping them handles a territory appearing, dissipating, tightening
/// as its bandwidth falls, and drifting across the map — with one rule and no
/// bookkeeping.
///
/// # Why not tween the picture instead
///
/// Cross-fading two band maps interpolates *contours*, which is exactly the
/// mush PRD §10.4 forbids: halfway between two positions you get both sets of
/// rings at half strength. Interpolating the field and thresholding afterwards
/// gives one set of rings that move, which is what a weather chart does.
#[derive(Debug, Clone, Default)]
pub struct CloudTween {
    field: Option<CloudField>,
}

impl CloudTween {
    /// Advances toward `target` by `dt` presentation seconds and returns the
    /// field to draw, or `None` once the last cloud has dissipated.
    ///
    /// `target` of `None` means the world has no visible territory; the current
    /// field decays in place rather than vanishing, so a thread that finishes
    /// does not take its cloud off the map in one frame.
    pub fn advance(
        &mut self,
        target: Option<CloudField>,
        dt: f64,
        rate: f64,
    ) -> Option<&CloudField> {
        let k = if dt <= 0.0 {
            0.0
        } else {
            // The same critically-damped chase `frame` uses for every other
            // tween, written out here so this module keeps its promise not to
            // reach for a transcendental.
            (dt * rate / dt.mul_add(rate, 1.0)) as f32
        };
        match (self.field.take(), target) {
            (None, None) => self.field = None,
            // A brand-new cloud starts at zero and rises, so a territory eases
            // in rather than popping the moment it converges (PRD §6.2).
            (None, Some(mut t)) => {
                for (d, c) in t.density.iter_mut().zip(t.crowd.iter_mut()) {
                    *d *= k;
                    *c *= k;
                }
                self.field = Some(t);
            }
            (Some(mut cur), None) => {
                let mut peak = 0.0f32;
                for (d, c) in cur.density.iter_mut().zip(cur.crowd.iter_mut()) {
                    *d -= *d * k;
                    *c -= *c * k;
                    peak = peak.max(*d);
                }
                self.field = (peak > CLOUD_GONE).then_some(cur);
            }
            (Some(cur), Some(t)) => {
                // The two fields rarely share a rectangle — a territory that
                // drifts, or gains a lobe, moves the bounding box under it — so
                // the tween runs on a lattice covering **both**, and cropping to
                // the target's would teleport whatever the target no longer
                // covers. The union is taken against the current field's *live*
                // bounds rather than its rectangle, so the ground a cloud is
                // leaving is released as it decays instead of being carried for
                // ever.
                let mut merged = if cur.aligned_with(&t) {
                    t.clone()
                } else {
                    cur.union_with(&t)
                };
                let (od, oc, oo) = cur.resampled_onto(&merged);
                let (td, tc, to) = t.resampled_onto(&merged);
                for (i, ((d, c), o)) in merged
                    .density
                    .iter_mut()
                    .zip(merged.crowd.iter_mut())
                    .zip(merged.owner.iter_mut())
                    .enumerate()
                {
                    *d = (td[i] - od[i]).mul_add(k, od[i]);
                    *c = (tc[i] - oc[i]).mul_add(k, oc[i]);
                    // The owner is the label of whichever side has the density
                    // here, and the target wins a tie. A cloud handing ground
                    // over therefore changes hue at the moment the new
                    // territory's field is the larger one, rather than
                    // cross-fading through a colour neither thread has.
                    *o = if td[i] >= od[i] { to[i] } else { oo[i] };
                }
                self.field = Some(merged);
            }
        }
        self.field.as_ref()
    }

    /// The field as it stands, without advancing it.
    #[must_use]
    pub fn field(&self) -> Option<&CloudField> {
        self.field.as_ref()
    }

    /// Drops the tween — the camera jumped, or the replay was scrubbed, and
    /// easing across the cut would draw a cloud sliding through the city.
    pub fn reset(&mut self) {
        self.field = None;
    }
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
    /// How many territories reach fringe level at each pixel. `>= CLOUD_CROWD`
    /// is contested ground.
    pub crowd: Vec<u8>,
    /// The owning thread's [`thread_slot`] per pixel, or [`NO_TINT`] where the
    /// caller offered no identity table.
    pub tint: Vec<u8>,
}

impl BandMap {
    /// The band at a canvas pixel, or [`NO_BAND`] outside the rectangle.
    #[must_use]
    pub fn at(&self, x: usize, y: usize) -> u8 {
        self.index(x, y).map_or(NO_BAND, |i| self.cells[i])
    }

    /// The ink for one banded pixel: the owning thread's hue at that band's
    /// brightness, or the neutral tone where there is no owner.
    #[must_use]
    pub fn tone(&self, i: usize, band: u8) -> Rgb {
        let band = CLOUD_TONES[band as usize % CLOUD_TONES.len()];
        match self.tint.get(i).copied() {
            Some(t) if t != NO_TINT => thread_ink(t, band),
            _ => band,
        }
    }

    /// How many territories claim a canvas pixel.
    #[must_use]
    pub fn crowd_at(&self, x: usize, y: usize) -> u8 {
        self.index(x, y).map_or(0, |i| self.crowd[i])
    }

    fn index(&self, x: usize, y: usize) -> Option<usize> {
        if x < self.x0 || y < self.y0 {
            return None;
        }
        let (dx, dy) = (x - self.x0, y - self.y0);
        (dx < self.width && dy < self.height).then_some(dy * self.width + dx)
    }
}

/// The density field of a set of kernels, thresholded into [`CLOUD_ISO`]'s
/// bands, over the kernels' bounding rectangle clipped to the canvas.
///
/// Returns `None` when the kernels fall entirely outside the canvas. This is
/// [`CloudField::sample`] followed by [`CloudField::bands`], kept as one call
/// for the many places that want the picture and not the field.
#[must_use]
pub fn band_map(kernels: &[CloudKernel], width: usize, rows: usize) -> Option<BandMap> {
    Some(CloudField::sample(kernels, width, rows)?.bands())
}

/// How wide each band's contour may be, from the band's own geometry.
///
/// `2 · area / edge` is the radius of a disc with that area and that perimeter,
/// and it degrades sensibly for the shapes a density field actually makes: for a
/// long thin annulus it returns the annulus's thickness, which is exactly the
/// number a contour must not exceed.
#[must_use]
pub fn contour_steps(bands: &BandMap) -> [isize; 3] {
    let (w, h) = (bands.width, bands.height);
    let mut area = [0usize; 3];
    let mut edge = [0usize; 3];
    for y in 0..h {
        for x in 0..w {
            let band = bands.cells[y * w + x];
            if band == NO_BAND {
                continue;
            }
            let k = band as usize;
            area[k] += 1;
            let differs = |nx: isize, ny: isize| -> bool {
                if nx < 0 || ny < 0 || nx >= w as isize || ny >= h as isize {
                    return true;
                }
                bands.cells[ny as usize * w + nx as usize] != band
            };
            let (ix, iy) = (x as isize, y as isize);
            if differs(ix - 1, iy)
                || differs(ix + 1, iy)
                || differs(ix, iy - 1)
                || differs(ix, iy + 1)
            {
                edge[k] += 1;
            }
        }
    }
    let mut out = [1isize; 3];
    for k in 0..3 {
        if edge[k] == 0 {
            continue;
        }
        let radius = 2.0 * area[k] as f64 / edge[k] as f64;
        let want = (radius * CLOUD_CONTOUR_FRACTION).round();
        out[k] = want.clamp(1.0, CLOUD_CONTOUR_WIDTH[k].max(1.0)) as isize;
    }
    out
}

/// Paints a band map as nested contours plus a hatch that tightens toward the
/// core, crossed where territories overlap.
///
/// Nothing here is a fill, so the base map under a cloud is not lifted — it is
/// left alone and shows through between the strokes. The contour is found by
/// comparing a pixel's band with its neighbours a stroke-width away, which gives
/// a closed curve of the right thickness for free and cannot leak: a band
/// boundary is a boundary in the array.
pub fn paint_cloud_bands(canvas: &mut Canvas, bands: &BandMap) {
    paint_cloud_bands_into(canvas, bands, [0, 0]);
}

/// [`paint_cloud_bands`] onto a canvas that covers only part of the map.
///
/// `origin` is where the canvas's top-left sits in the band map's coordinates.
/// The **hatch phase is still taken from the band map's own coordinates**, not
/// the canvas's: the hatch is a texture anchored to the map, and re-phasing it
/// when the cloud's bounding rectangle happens to move would make it swim under
/// a territory that is merely growing.
pub fn paint_cloud_bands_into(canvas: &mut Canvas, bands: &BandMap, origin: [usize; 2]) {
    let step = contour_steps(bands);
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
            let contested = bands.crowd[y * bands.width + x] >= CLOUD_CROWD;
            let hatched = !contour && {
                let width = CLOUD_HATCH_WIDTH[band as usize];
                let mut spacing = CLOUD_HATCH_SPACING[band as usize];
                // One direction for one territory; two crossed for contested
                // ground, at a spacing that keeps the *ink* about where it was
                // so the weave reads without fogging the city under it.
                let a = CLOUD_HATCH[0].mul_add(cx as f64, CLOUD_HATCH[1] * cy as f64);
                if contested {
                    spacing *= CLOUD_OVERLAP_SPACING;
                    let b = CLOUD_HATCH[1].mul_add(cx as f64, -(CLOUD_HATCH[0] * cy as f64));
                    a.rem_euclid(spacing) < width || b.rem_euclid(spacing) < width
                } else {
                    a.rem_euclid(spacing) < width
                }
            };
            if contour || hatched {
                let (Some(px), Some(py)) = (cx.checked_sub(origin[0]), cy.checked_sub(origin[1]))
                else {
                    continue;
                };
                // The band decides the brightness, the owner decides the hue,
                // and neither can take the other's channel: two threads' clouds
                // are the same three tones apart in tone and a hue apart in
                // identity, so "core versus fringe" still reads in greyscale.
                cloud_pixel(canvas, px, py, bands.tone(y * bands.width + x, band));
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
    // Under the glyphs, over everything else: the alarm is a ring *around* the
    // failures, and a mark it covered would be a mark the operator could not
    // then read. See [`crate::salience`] for why this is layer 4 and not 5.
    crate::salience::draw(canvas, &frame.alarms, r);
    for m in &frame.marks {
        draw_mark(canvas, *m, r);
    }
    for a in &frame.agents {
        draw_agent(canvas, *a, r);
    }
    start.elapsed()
}

/// Layer 5 — the three attention states (PRD §11.2), in PRD §11.1's order.
///
/// # The ordering is an ordering of *paint*, not only of a list
///
/// `contention > needs-decision > done`. A list can express that with a sort;
/// a picture cannot, because whatever is painted last is what the operator sees
/// where two marks overlap. So the passes run worst-**last**:
///
/// 1. contention **links** — long bows across the map, drawn underneath so a
///    relation does not hide the things it relates;
/// 2. `done`, verified then unverified;
/// 3. `needs decision` — the primary state;
/// 4. contention **ends** — the two places work is being destroyed, on top of
///    everything, including a pin that happens to stand in one of them.
///
/// Splitting contention across the first and last pass is the whole point: it
/// is the only state that is a relation, and the relation belongs at the bottom
/// while its terminals belong at the top.
///
/// # The beacons, and why §11.1 needed them
///
/// Between the passes sit two sets of [`crate::salience`] rings — one per
/// pending decision, two per contention. They are the same machinery
/// [`AttentionMark`]'s red sibling has had since M4, and they are here because
/// the ordering §11.1 states was true of the *sort key* and false of the
/// *picture*: measured on a live frame with a thread genuinely blocked on a
/// human, amber held 243 px against decoration's 34 213, and the band PRD §10.3
/// reserves for these three states had measured 0.000 % of map area in every
/// frame ever rendered. A pin is a mark about a *building*; a ring is a mark
/// about a *district*, and a district is what survives being seen from a chair
/// on the other side of the room.
pub fn draw_attention(canvas: &mut Canvas, frame: &LiveFrame) -> Duration {
    let start = Instant::now();
    let r = glyph_radius(frame.unit, frame.map_height);
    let (decisions, contention) = crate::salience::beacons(&frame.attention, r, frame.map_height);
    for m in &frame.attention {
        if m.kind == MarkKind::Contention {
            draw_contention_link(canvas, *m, r);
        }
    }
    for m in &frame.attention {
        if m.kind == MarkKind::DoneVerified {
            draw_done(canvas, *m, r);
        }
    }
    for m in &frame.attention {
        if m.kind == MarkKind::DoneUnverified {
            draw_done(canvas, *m, r);
        }
    }
    // The primary state, region first and pin second: the ring says *this
    // district is waiting on you* from across the room, and the pin says which
    // building once the operator has walked over.
    crate::salience::draw(canvas, &decisions, r);
    for m in &frame.attention {
        if m.kind == MarkKind::NeedsDecision {
            draw_pin(canvas, *m, r);
        }
    }
    // …and contention's, over the pins, because §11.1 puts it above them and
    // because it is the only state that claims two districts at once.
    crate::salience::draw(canvas, &contention, r);
    for m in &frame.attention {
        if m.kind == MarkKind::Contention {
            draw_contention_ends(canvas, *m, r);
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
/// rather than a counter. See this module's `LEG_BOW`.
pub fn draw_trail(canvas: &mut Canvas, trail: &Trail, style: TrailStyle, r: f64) {
    if trail.steps.len() < 2 {
        return;
    }
    let ttl = trail.ttl.max(1e-6);
    // Whose trail. Identity is the hue; age is still the tone ramp, so a fresh
    // trail and a stale one are the same colour at two brightnesses.
    let ink_of = |t: f64| fade(thread_ink(trail.tint, AGENT_TRAIL), AGENT_FLOOR, t);
    let repeats = leg_repeats(&trail.steps);
    let mut path: Vec<Px> = Vec::with_capacity(BOW_CHORDS + 1);
    for (leg, pair) in trail.steps.windows(2).enumerate() {
        let (a, b) = (pair[0], pair[1]);
        // A segment is as old as its *newer* end: the eye reads a stroke as one
        // object, and grading it by its older end makes the whole trail look
        // staler than it is.
        let age = (b.age / ttl).clamp(0.0, 1.0);
        let fresh = 1.0 - age;
        let ink = ink_of(0.75f64.mul_add(fresh, 0.25));
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
            let ink = ink_of(0.75f64.mul_add(fresh, 0.25));
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
/// `MAX_LEG_BOW`, which turns `A → B → A` into a lens and `A → B → A → B → A`
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

/// One tether: a bowed line from a thread's anchor to one of its workers,
/// drawn **only for the thread the operator is asking about**.
///
/// # §17's test, run honestly, and what it cost the fan
///
/// > **Test for every visual element: does it change a decision?** If not, cut
/// > it. (PRD §17)
///
/// A tether says "this worker belongs to that thread". The two decisions this
/// product exists to accelerate are *unblock* and *redirect* (PRD §1), and
/// neither is reached by knowing which of a hundred workers belongs to which of
/// nine threads: nobody redirects an agent because it has hands. Drawn
/// ambiently it is one **line across the whole map** per worker — the one
/// geometry that cannot be decluttered by position, because it is everywhere by
/// construction. Measured on the operator's own repository, nine live threads
/// over `qurio-toolset`, the ambient fan was the largest single consumer of the
/// top of the contrast range while the state the product is for held 0.2 % of
/// it.
///
/// So ownership moved to the channel that was already carrying it and costs no
/// area at all: **colour**. [`thread_slot`] gives a thread one hue for its
/// whole life, the worker's own body is drawn in it at the brightest level
/// layer 4 has ([`AGENT_BODY`], ΔE00 9.8 between the worst pair), and the rail
/// prints the same triple beside the thread's name and its worker count. The
/// glance is answered without a line.
///
/// # The line comes back when you ask — PRD §12's fuzzy above, exact below
///
/// > **Fuzzy above, exact below.** Soft cloud edges are honest for the ambient
/// > layer and useless when acting. (PRD §12)
///
/// Ownership has exactly that shape. The ambient reading is a hue, which is
/// fuzzy and cheap and right for the glance; the exact reading — *these
/// sixteen, and no others* — is a question about **one** thread, and it is
/// asked by hovering or selecting it. Then, and only then, that thread's
/// tethers are drawn, capped at [`TETHERS_PER_THREAD`] with the running
/// workers first. Eight other threads' lines are never in the way of the
/// answer, which is the part the ambient fan could not do at any brightness.
///
/// Callers are what enforce this: `polis_app::mapview` draws tethers for the
/// emphasised thread only, and [`crate::frame::FrameRenderer`] emits them only
/// for the thread [`crate::frame::FrameRenderer::tether`] names. A frame nobody
/// is interrogating has no tethers in it.
///
/// # Brightness, now that it is an answer
///
/// It used to sit at 0.16 of the band — invisible on its own and blinding in
/// bulk, which is the signature of a mark that was being tuned for the wrong
/// population. One thread's sixteen lines can afford to be *read*: 0.70 of the
/// band running, 0.34 finished. That is peak 120 and 108, under
/// [`AGENT_TRAIL`]'s 144 and [`AGENT_BODY`]'s 168, so the thing being
/// identified stays louder than the line identifying it, and far under
/// [`crate::plan::ATTENTION_BAND`], which layer 5 owns alone.
///
/// # Running versus finished is a **shape** difference
///
/// Tone alone was three grey levels apart, and PRD §11.4 is explicit that
/// peripheral vision is poor at exactly that discrimination. A finished
/// worker's tether is **dashed** and a running one solid, so the count of hands
/// still on the job is legible from the silhouette, at thumbnail size, and in a
/// colour-blind reading. Dashed rather than dropped, so a thread does not
/// appear to shed limbs while the operator is looking straight at it.
pub fn draw_tether(canvas: &mut Canvas, tether: Tether, r: f64) {
    let (width, tone) = if tether.running {
        ((r * 0.10).max(MIN_STROKE * 0.8), 0.70)
    } else {
        ((r * 0.07).max(MIN_STROKE * 0.6), 0.34)
    };
    let ink = fade(thread_ink(tether.tint, AGENT_TETHER), AGENT_FLOOR, tone);
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
    let mut pts = Vec::with_capacity(TETHER_STEPS + 1);
    for i in 0..=TETHER_STEPS {
        let t = i as f64 / TETHER_STEPS as f64;
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
    if tether.running {
        canvas.polyline(&pts, width, ink, 1.0);
    } else {
        // One segment on, one off — four dashes along the bow. The gap has to be
        // at least a stroke width or the rasteriser's coverage closes it again
        // and the dash is a solid line that merely cost more; an eighth of a
        // tether is many times that at every zoom the notation is drawn at.
        //
        // It is also *cheaper* than the solid line it replaces, which matters:
        // a finished worker is the common case in a long session.
        let mut i = 0;
        while i + 1 < pts.len() {
            canvas.polyline(&pts[i..i + 2], width, ink, 1.0);
            i += 2;
        }
    }
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
    // Two marks, two channels. The **body** is the thread's hue — identity,
    // the thing the operator tracks across the map — and the centre disc keeps
    // PRD §10.2's outcome colour, so a failing worker is still red while it
    // moves without the whole agent changing colour every time a tool call
    // lands. See [`Mark`] for why the ranking goes this way and not the other.
    let ink = outcome_ink(agent.outcome);
    let body = thread_ink(
        agent.tint,
        if agent.body == Body::Main {
            AGENT_ANCHOR
        } else {
            AGENT_BODY
        },
    );
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

/// **(a) Needs decision** — the primary state, and the one the product is for.
///
/// > persistent, amber, drawn as a standing pin above the building or district
/// > […] Persists until resolved. (PRD §11.2a)
///
/// Four parts, and only the second is what the PRD names outright:
///
/// * a **plate** on the ground — a heavy ring at the foot of the pin. It is the
///   part that survives a box filter, because a 4–5 px stroke is still half a
///   thumbnail pixel where a 2 px one is a fifth of one, and it is the part that
///   says *this district*, since the pin's head is deliberately not over the
///   thing it points at;
/// * the **pin** — stem and a diamond head. The one silhouette in this module
///   that no operation glyph uses, so a pending decision can never be misread as
///   an edit;
/// * a **standing halo** whose radius grows with
///   [`AttentionMark::urgency`] — a broken ring that is absent on a pin
///   raised a second ago and wide on one that has stood for five minutes. This
///   is the channel PRD §11.4 leaves out and the corpus demands: 61 % of the
///   operator's measured waits were already past a minute and 26 % past fifteen,
///   and until now every one of them drew an identical mark;
/// * the **arrival flare**, ≤400 ms, expanding, thick. §11.4's motion onset.
fn draw_pin(canvas: &mut Canvas, mark: AttentionMark, r: f64) {
    let ink = fade(
        ATTN_DECISION,
        ATTENTION_FLOOR,
        0.35f64.mul_add(mark.weight, 0.65),
    );
    let u = mark.urgency.clamp(0.0, 1.0);
    // The plate: the pin is planted *here*, and this is the ink that carries to
    // the far side of the room.
    stroke_circle(canvas, mark.at, r * 1.35, (r * 0.60).max(MIN_STROKE), ink);

    let h = r * 1.1f64.mul_add(u, 4.0);
    let head = [mark.at[0], mark.at[1] - h];
    canvas.segment(mark.at, head, (r * 0.40).max(MIN_STROKE), ink, 1.0);
    let (hw, hh) = (0.30f64.mul_add(u, 1.05) * r, 0.45f64.mul_add(u, 1.50) * r);
    canvas.fill_polygon(
        &[
            [head[0], head[1] - hh],
            [head[0] + hw, head[1]],
            [head[0], head[1] + hh],
            [head[0] - hw, head[1]],
        ],
        ink,
        1.0,
    );
    if u > 0.05 {
        // Broken rather than solid, so an escalating pin never turns into the
        // ring `done` draws. Area, not brightness: the ink is the same at one
        // second and at an hour, and only the geometry has grown.
        let radius = r * 2.4f64.mul_add(u, 1.9);
        let pts: Vec<Px> = CIRCLE
            .iter()
            .chain(std::iter::once(&CIRCLE[0]))
            .map(|c| [c[0].mul_add(radius, head[0]), c[1].mul_add(radius, head[1])])
            .collect();
        dashed_path(canvas, &pts, r * 1.5, 0.55, (r * 0.34).max(MIN_STROKE), ink);
    }
    if mark.pulse > 0.0 {
        let p = mark.pulse.clamp(0.0, 1.0);
        stroke_circle(
            canvas,
            head,
            r * 4.2f64.mul_add(1.0 - p, 1.6),
            (r * 0.5 * p).max(MIN_STROKE),
            ink,
        );
    }
}

/// **(b) Done** — teal, decaying, main agents only.
///
/// The two variants are **closed** and **open**, not two shades of teal: PRD
/// §17's open question 2 asks whether *done, unverified* deserves a state of its
/// own, and the corpus answers 52.7 % — the modal way a session ends — so the
/// distinction has to survive being read without colour. Verified is a ring with
/// its centre filled; unverified is a hollow ring inside a broken outer one, and
/// it grows with [`AttentionMark::urgency`] like a pin does, because it is
/// "needs review" and nobody has reviewed it.
///
/// Done is the *smallest* of the three marks by design. §11.1: "Done costs
/// nothing." It is legible when the operator looks at the map and it does not
/// compete for a glance from across the room, which is the entire reason §11.2
/// warns that `done` otherwise "floods the display within the hour and buries
/// state (a)".
fn draw_done(canvas: &mut Canvas, mark: AttentionMark, r: f64) {
    let ink = fade(ATTN_DONE, ATTENTION_FLOOR, 0.6f64.mul_add(mark.weight, 0.4));
    stroke_circle(canvas, mark.at, r * 1.6, (r * 0.34).max(MIN_STROKE), ink);
    if mark.kind == MarkKind::DoneUnverified {
        let u = mark.urgency.clamp(0.0, 1.0);
        let radius = r * 0.9f64.mul_add(u, 2.4);
        let pts: Vec<Px> = CIRCLE
            .iter()
            .chain(std::iter::once(&CIRCLE[0]))
            .map(|c| {
                [
                    c[0].mul_add(radius, mark.at[0]),
                    c[1].mul_add(radius, mark.at[1]),
                ]
            })
            .collect();
        dashed_path(canvas, &pts, r * 1.3, 0.5, (r * 0.32).max(MIN_STROKE), ink);
    } else {
        // Sealed. Nothing else on the attention layer has a filled centre.
        canvas.disc(mark.at, r * 0.55, ink, 1.0);
    }
    if mark.pulse > 0.0 {
        let p = mark.pulse.clamp(0.0, 1.0);
        stroke_circle(
            canvas,
            mark.at,
            r * 3.4f64.mul_add(1.0 - p, 1.6),
            (r * 0.26 * p).max(MIN_STROKE),
            ink,
        );
    }
}

/// The severity notation: stroke weight and dash rhythm, never four reds.
///
/// > Severity […] carried by stroke weight and dash, so it is not a colour-only
/// > distinction. (PRD §11.4)
fn contention_stroke(severity: Option<Severity>, r: f64) -> (f64, f64) {
    match severity.unwrap_or(Severity::High) {
        Severity::Critical => (r * 0.62, 1.0),
        Severity::High => (r * 0.48, 1.0),
        Severity::Medium => (r * 0.38, 0.55),
        Severity::Low => (r * 0.30, 0.25),
    }
}

/// The bowed arc joining the two threads.
///
/// > This is a **relation between two threads, not a property of one**, so it is
/// > drawn as a link joining them across the map, not a badge on a dot. It is
/// > also the only state that can pull the eye to two places at once. (PRD
/// > §11.2c)
///
/// Which is also why contention is the largest mark on the layer without any
/// special pleading: it is the only one whose size is set by the *distance
/// between two places*, and it is the only state where work is being destroyed
/// while the operator is not looking.
fn draw_contention_link(canvas: &mut Canvas, mark: AttentionMark, r: f64) {
    let Some(other) = mark.other else {
        return;
    };
    let ink = fade(
        ATTN_CONTENTION,
        ATTENTION_FLOOR,
        0.4f64.mul_add(mark.weight, 0.6),
    );
    let (width, duty) = contention_stroke(mark.severity, r);
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
}

/// The two terminals, painted last of everything on layer 5.
///
/// PRD §11.1 puts contention above the other two states, and where two marks
/// land on one building that has to mean *this one is on top*. A pin standing in
/// a district that is also being clobbered is the case: the clobber wins.
fn draw_contention_ends(canvas: &mut Canvas, mark: AttentionMark, r: f64) {
    let Some(other) = mark.other else {
        return;
    };
    let ink = fade(
        ATTN_CONTENTION,
        ATTENTION_FLOOR,
        0.4f64.mul_add(mark.weight, 0.6),
    );
    let (width, _) = contention_stroke(mark.severity, r);
    for end in [mark.at, other] {
        canvas.disc(end, r * 1.0, ink, 1.0);
        // A collar, so a terminal is a *target* rather than a bead — and so the
        // two ends of one link read as a pair at thumbnail size, which is what
        // "pull the eye to two places at once" needs.
        stroke_circle(canvas, end, r * 2.1, (width * 0.8).max(MIN_STROKE), ink);
        if mark.pulse > 0.0 {
            let p = mark.pulse.clamp(0.0, 1.0);
            stroke_circle(
                canvas,
                end,
                r * 4.0f64.mul_add(1.0 - p, 2.4),
                (r * 0.5 * p).max(MIN_STROKE),
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

/// Bilinear lookup into a row-major lattice at fractional cell coordinates.
///
/// `fx` and `fy` are already clamped to `[0, grid - 1]` by every caller, which
/// is what makes the `+1` neighbours safe to clamp rather than bounds-check per
/// axis inside the loop.
pub(crate) fn bilinear(grid: &[f32], grid_x: usize, grid_y: usize, fx: f64, fy: f64) -> f32 {
    let c0 = fx.floor();
    let r0 = fy.floor();
    let tx = (fx - c0) as f32;
    let ty = (fy - r0) as f32;
    let c0 = (c0 as usize).min(grid_x - 1);
    let r0 = (r0 as usize).min(grid_y - 1);
    let c1 = (c0 + 1).min(grid_x - 1);
    let r1 = (r0 + 1).min(grid_y - 1);
    let a = grid[r0 * grid_x + c0];
    let b = grid[r0 * grid_x + c1];
    let c = grid[r1 * grid_x + c0];
    let d = grid[r1 * grid_x + c1];
    let top = (b - a).mul_add(tx, a);
    let bot = (d - c).mul_add(tx, c);
    (bot - top).mul_add(ty, top)
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

    // -----------------------------------------------------------------------
    // Identity: how far apart the thread hues actually are, in CIELAB
    // -----------------------------------------------------------------------

    /// sRGB -> CIELAB (D65), so the palette can be judged in a space where
    /// distance means something.
    ///
    /// Written out rather than pulled in: this is the only place in the
    /// workspace that needs a colour-appearance model, it is a dozen lines, and
    /// the alternative is a dependency in the tree of the crate that draws every
    /// frame.
    fn lab(rgb: Rgb) -> [f64; 3] {
        fn lin(c: u8) -> f64 {
            let c = f64::from(c) / 255.0;
            if c <= 0.040_45 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        }
        fn f(t: f64) -> f64 {
            if t > 0.008_856 {
                t.cbrt()
            } else {
                7.787f64.mul_add(t, 16.0 / 116.0)
            }
        }
        let (r, g, b) = (lin(rgb[0]), lin(rgb[1]), lin(rgb[2]));
        let x = 0.180_437_5f64.mul_add(b, 0.412_456_4f64.mul_add(r, 0.357_576_1 * g)) / 0.950_47;
        let y = 0.072_175_0f64.mul_add(b, 0.212_672_9f64.mul_add(r, 0.715_152_2 * g));
        let z = 0.950_304_1f64.mul_add(b, 0.019_333_9f64.mul_add(r, 0.119_192_0 * g)) / 1.088_83;
        let (fx, fy, fz) = (f(x), f(y), f(z));
        [
            116.0f64.mul_add(fy, -16.0),
            500.0 * (fx - fy),
            200.0 * (fy - fz),
        ]
    }

    /// CIEDE2000. The perceptual distance the palette is designed against, and
    /// the number the module docs' table reports.
    fn de2000(a: Rgb, b: Rgb) -> f64 {
        let (l1, l2) = (lab(a), lab(b));
        let (c1, c2) = (l1[1].hypot(l1[2]), l2[1].hypot(l2[2]));
        let cb = f64::midpoint(c1, c2);
        let g = 0.5 * (1.0 - (cb.powi(7) / (cb.powi(7) + 25f64.powi(7))).sqrt());
        let (a1p, a2p) = ((1.0 + g) * l1[1], (1.0 + g) * l2[1]);
        let (c1p, c2p) = (a1p.hypot(l1[2]), a2p.hypot(l2[2]));
        let hp = |ap: f64, bp: f64| {
            if ap == 0.0 && bp == 0.0 {
                0.0
            } else {
                bp.atan2(ap).to_degrees().rem_euclid(360.0)
            }
        };
        let (h1p, h2p) = (hp(a1p, l1[2]), hp(a2p, l2[2]));
        let dlp = l2[0] - l1[0];
        let dcp = c2p - c1p;
        let dhp = if c1p * c2p == 0.0 {
            0.0
        } else if (h2p - h1p).abs() <= 180.0 {
            h2p - h1p
        } else if h2p > h1p {
            h2p - h1p - 360.0
        } else {
            h2p - h1p + 360.0
        };
        let dbighp = 2.0 * (c1p * c2p).sqrt() * (dhp.to_radians() / 2.0).sin();
        let lbp = f64::midpoint(l1[0], l2[0]);
        let cbp = f64::midpoint(c1p, c2p);
        let hbp = if c1p * c2p == 0.0 {
            h1p + h2p
        } else if (h1p - h2p).abs() <= 180.0 {
            f64::midpoint(h1p, h2p)
        } else if h1p + h2p < 360.0 {
            (h1p + h2p + 360.0) / 2.0
        } else {
            (h1p + h2p - 360.0) / 2.0
        };
        let t = 0.20f64.mul_add(
            -(4.0f64.mul_add(hbp, -63.0)).to_radians().cos(),
            0.32f64.mul_add(
                (3.0f64.mul_add(hbp, 6.0)).to_radians().cos(),
                0.24f64.mul_add(
                    (2.0 * hbp).to_radians().cos(),
                    0.17f64.mul_add(-(hbp - 30.0).to_radians().cos(), 1.0),
                ),
            ),
        );
        let dth = 30.0 * (-(((hbp - 275.0) / 25.0).powi(2))).exp();
        let rc = 2.0 * (cbp.powi(7) / (cbp.powi(7) + 25f64.powi(7))).sqrt();
        let sl = 1.0 + (0.015 * (lbp - 50.0).powi(2)) / (20.0 + (lbp - 50.0).powi(2)).sqrt();
        let sc = 0.045f64.mul_add(cbp, 1.0);
        let sh = (0.015 * cbp).mul_add(t, 1.0);
        let rt = -(2.0 * dth).to_radians().sin() * rc;
        (rt * (dcp / sc) * (dbighp / sh))
            .mul_add(
                1.0,
                (dbighp / sh).mul_add(
                    dbighp / sh,
                    (dlp / sl).mul_add(dlp / sl, (dcp / sc).powi(2)),
                ),
            )
            .max(0.0)
            .sqrt()
    }

    /// The worst pair of thread hues at one role's brightness.
    fn worst_pair(role: Rgb) -> (f64, usize, usize) {
        let inks: Vec<Rgb> = (0..THREAD_HUES.len())
            .map(|i| thread_ink(i as u8, role))
            .collect();
        let mut worst = (f64::INFINITY, 0, 0);
        for i in 0..inks.len() {
            for j in i + 1..inks.len() {
                let d = de2000(inks[i], inks[j]);
                if d < worst.0 {
                    worst = (d, i, j);
                }
            }
        }
        worst
    }

    /// The measurement PRD §11.4 and the operator's *"slightly different to the
    /// others"* both come down to: with nine threads on screen, can two of them
    /// be told apart **at the brightness the map actually draws them**?
    ///
    /// The floors are the measured values, held to the nearest tenth so a
    /// careless edit to [`THREAD_HUES`] or to a band constant fails here rather
    /// than in front of the operator. The agent band is where identity has to
    /// read from across the room; the cloud band is `49-84` and physically
    /// cannot do better than this.
    #[test]
    fn thread_hues_are_far_enough_apart_at_the_luminance_they_are_drawn_at() {
        for (name, role, floor) in [
            ("rail swatch / agent body", AGENT_BODY, 9.8),
            ("anchor", AGENT_ANCHOR, 9.5),
            ("trail", AGENT_TRAIL, 8.8),
            ("tether", AGENT_TETHER, 7.9),
            ("cloud core", CLOUD_TONES[2], 5.8),
            ("cloud body", CLOUD_TONES[1], 6.1),
            ("cloud fringe", CLOUD_TONES[0], 5.4),
        ] {
            let (d, i, j) = worst_pair(role);
            // Printed, not only asserted: `cargo test -- --nocapture` is how the
            // table in `THREAD_HUES`'s docs is re-measured after any change to
            // the ring or to a band constant.
            println!(
                "{name:26} peak {:3}  worst dE00 {d:5.2}  slots {i}<->{j}",
                head(role)
            );
            assert!(
                d >= floor,
                "{name}: worst pair is slots {i} and {j} at dE00 {d:.2}, under the measured \
                 floor {floor} - {:?} against {:?}",
                thread_ink(i as u8, role),
                thread_ink(j as u8, role)
            );
        }
    }

    /// Every thread ink is inside the band of the role it was levelled to. A
    /// palette that carries identity still may not spend PRD §10.3's contrast
    /// budget: a thread's cloud has to stay a cloud.
    #[test]
    fn a_threads_colour_never_leaves_the_band_of_the_thing_it_is_drawn_on() {
        for slot in 0..THREAD_HUES.len() as u8 {
            for role in [AGENT_BODY, AGENT_TRAIL, AGENT_TETHER, AGENT_ANCHOR] {
                let h = head(thread_ink(slot, role));
                assert!(
                    (AGENT_BAND.0..=AGENT_BAND.1).contains(&h),
                    "slot {slot} on {role:?} came out at {h}, outside {AGENT_BAND:?}"
                );
            }
            for tone in CLOUD_TONES {
                let h = head(thread_ink(slot, tone));
                assert!(
                    (CLOUD_BAND.0..=CLOUD_BAND.1).contains(&h),
                    "slot {slot} on cloud tone {tone:?} came out at {h}, outside {CLOUD_BAND:?}"
                );
            }
        }
    }

    /// A role's brightness is the role's, not the thread's: every thread's body
    /// sits at exactly one peak, so the band ladder still reads in greyscale and
    /// no thread is quietly louder than another.
    #[test]
    fn identity_spends_hue_and_chroma_and_never_the_contrast_budget() {
        for role in [AGENT_BODY, AGENT_TRAIL, AGENT_TETHER, AGENT_ANCHOR] {
            let want = head(role);
            for slot in 0..THREAD_HUES.len() as u8 {
                assert_eq!(
                    head(thread_ink(slot, role)),
                    want,
                    "slot {slot} moved the peak of {role:?}"
                );
            }
        }
    }

    /// The whole point of hashing the id instead of indexing a list: a thread's
    /// colour cannot move because another thread started, finished or was
    /// retired.
    #[test]
    fn a_threads_hue_does_not_move_when_another_thread_comes_or_goes() {
        let ids = [
            "4f3a1c22-0e5b-4b8a-9d21-6c7e5f0a1b2c",
            "9b2e77d0-1111-4aaa-8bbb-ccccddddeeee",
            "0000aaaa-2222-4ccc-8ddd-eeeeffff0000",
        ];
        let first: Vec<u8> = ids.iter().map(|i| thread_slot(i)).collect();
        // Every subset, in every order, and the answers never move - because
        // there is no list to be in.
        for perm in [[2usize, 0, 1], [1, 2, 0], [0, 2, 1]] {
            for k in perm {
                assert_eq!(thread_slot(ids[k]), first[k]);
            }
        }
        assert_eq!(thread_slot(""), thread_slot(""));
    }

    /// The hash is pinned by literal values (ADR-0029): if `DefaultHasher` crept
    /// in, or the constants were retyped, the city would repaint itself on a
    /// toolchain bump and nothing else would notice.
    #[test]
    fn the_identity_hash_is_written_out_and_pinned() {
        assert_eq!(thread_slot(""), (FNV_OFFSET % 12) as u8);
        assert_eq!(thread_slot("a"), 4);
        assert_eq!(thread_slot("polis"), 4);
        assert_eq!(thread_slot("4f3a1c22-0e5b-4b8a-9d21-6c7e5f0a1b2c"), 1);
        // Out of range wraps rather than panicking, so a stale slot from an old
        // snapshot cannot take the window down.
        assert_eq!(thread_hue(NO_TINT), thread_hue(NO_TINT % 12));
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

    /// Two threads, two clouds, two colours — and neither of them anywhere near
    /// the base map's band.
    ///
    /// The operator's first complaint was that nine threads all looked the
    /// same. This is the layer where that is hardest to fix: PRD §10.3 gives a
    /// cloud channels `49-84`, which is a twelfth of the range the agent band
    /// gets, so it is also the layer where a careless fix would reach for
    /// brightness and take the base map's contrast with it.
    #[test]
    fn two_threads_clouds_are_two_colours_and_neither_lifts_the_base_map() {
        let mut c = Canvas::new(320, 200, [20, 21, 24]);
        let mut kernels: Vec<CloudKernel> = Vec::new();
        for (group, cx) in [(0u16, 80.0f64), (1, 240.0)] {
            for i in 0..9 {
                kernels.push(CloudKernel {
                    at: [
                        f64::from(i % 3).mul_add(20.0, cx - 20.0),
                        f64::from(i / 3).mul_add(20.0, 80.0),
                    ],
                    radius: 40.0,
                    weight: 1.0,
                    thread: group,
                });
            }
        }
        let frame = LiveFrame {
            unit: 30.0,
            map_height: 200.0,
            clouds: kernels,
            // Slot 0 and slot 6 — opposite sides of the hue circle, which is
            // what two threads that hash apart look like.
            cloud_tints: vec![0, 6],
            ..LiveFrame::default()
        };
        draw_clouds(&mut c, &frame);

        let mut left: Vec<Rgb> = Vec::new();
        let mut right: Vec<Rgb> = Vec::new();
        for (i, p) in c.pixels.as_chunks::<3>().0.iter().enumerate() {
            if p == &[20, 21, 24] {
                continue;
            }
            let px = *p;
            // Still opaque marks inside the cloud band: identity may not spend
            // one level of PRD §10.3's budget.
            let h = head(px);
            assert!(
                (CLOUD_BAND.0..=CLOUD_BAND.1).contains(&h),
                "a tinted cloud pixel came out at {h}: {px:?}"
            );
            if i % 320 < 160 {
                left.push(px);
            } else {
                right.push(px);
            }
        }
        assert!(
            left.len() > 200 && right.len() > 200,
            "both clouds have to be on the canvas: {} and {} px",
            left.len(),
            right.len()
        );
        // Each side draws only its own thread's three tones, and the two sets
        // are disjoint — which is the whole claim.
        let want_l = thread_cloud_tones(0);
        let want_r = thread_cloud_tones(6);
        for px in &left {
            assert!(want_l.contains(px), "left cloud drew {px:?}, not slot 0's");
        }
        for px in &right {
            assert!(want_r.contains(px), "right cloud drew {px:?}, not slot 6's");
        }
        for a in want_l {
            assert!(!want_r.contains(&a), "the two threads share a tone {a:?}");
        }
    }

    /// Without a tint table the picture is exactly the one this module drew
    /// before identity existed. A caller with nothing to say about whose cloud
    /// it is gets the neutral tones, not a guess.
    #[test]
    fn a_cloud_with_no_identity_offered_is_the_neutral_tone_it_always_was() {
        let kernels: Vec<CloudKernel> = (0..9)
            .map(|i| CloudKernel {
                at: [
                    f64::from(i % 3).mul_add(20.0, 60.0),
                    f64::from(i / 3).mul_add(20.0, 60.0),
                ],
                radius: 40.0,
                weight: 1.0,
                thread: 0,
            })
            .collect();
        let field = CloudField::sample(&kernels, 200, 200).expect("a field");
        let mut plain = Canvas::new(200, 200, [20, 21, 24]);
        let mut empty = Canvas::new(200, 200, [20, 21, 24]);
        paint_cloud_bands(&mut plain, &field.bands());
        paint_cloud_bands(&mut empty, &field.bands_tinted(&[]));
        assert_eq!(plain.pixels, empty.pixels);
        for p in plain.pixels.as_chunks::<3>().0 {
            if p != &[20, 21, 24] {
                assert!(CLOUD_TONES.contains(&[p[0], p[1], p[2]]), "{p:?}");
            }
        }
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
                thread: 0,
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
                thread: 0,
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
        let trail = Trail {
            steps,
            ttl: 300.0,
            tint: 0,
        };
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
            let trail = Trail {
                steps,
                ttl: 300.0,
                tint: 0,
            };
            draw_trail(&mut c, &trail, TrailStyle::Timed, r);
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
            let trail = Trail {
                steps,
                ttl: 300.0,
                tint: 0,
            };
            draw_trail(&mut c, &trail, style, 9.0);
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
            draw_contention_link(
                &mut c,
                AttentionMark {
                    kind: MarkKind::Contention,
                    at: [40.0, 100.0],
                    other: Some([260.0, 100.0]),
                    severity: Some(severity),
                    pulse: 0.0,
                    weight: 1.0,
                    urgency: 0.0,
                    sited: true,
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
                    urgency: 0.0,
                    sited: true,
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
                thread: 0,
            }],
            trails: vec![Trail {
                tint: 0,
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
                tint: 0,
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
                urgency: 0.0,
                sited: true,
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
                thread: 0,
            }],
            trails: vec![Trail {
                tint: 0,
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
                tint: 0,
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
                tint: 0,
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
            urgency: 0.0,
            sited: true,
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
                tint: 0,
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
                    tint: 0,
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
                tint: 0,
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
                urgency: 0.0,
                sited: true,
            });
        }
        let mut c = Canvas::new(1400, 1400, [20, 21, 24]);
        // One warm pass so the measurement is not the first-touch page faults on
        // a 5.9 MB buffer.
        draw_agents(&mut c, &frame, TrailStyle::Timed);
        // The **best** of five, not the last of five. This is a wall clock in
        // a debug build on a machine that is running the rest of the suite on
        // its other twenty-three cores, and a single sample measures the load as
        // much as the layer: the same frame has come out at 49 ms alone and
        // 105 ms under `cargo test --workspace`. The claim is what the code can
        // do, so the least contended sample is the estimator, and the run is
        // repeated rather than the budget widened.
        let mut timings = draw(&mut c, &frame, TrailStyle::Timed);
        for _ in 0..4 {
            let again = draw(&mut c, &frame, TrailStyle::Timed);
            if again.budgeted() < timings.budgeted() {
                timings = again;
            }
        }
        // PRD §13.1 budgets the agent and attention layers at **4 ms**, and that
        // is a claim about the shipped build; it is asserted unchanged below.
        //
        // The debug allowance is not a product number, it is a smoke test, and
        // it has to survive being measured on a machine whose other
        // twenty-three cores are running the rest of the suite. Best-of-five on
        // this frame comes out at 43 ms alone and has been seen at 105 ms under
        // `cargo test --workspace`; 80 ms sat between those two and turned a
        // performance assertion into a coin toss. 150 ms still fails on a real
        // regression — the frame would have to slow by 3.5× — and does not fail
        // on a busy machine.
        let budget = if cfg!(debug_assertions) {
            Duration::from_millis(150)
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

    // -----------------------------------------------------------------------
    // Layer 3 — the field, the bands, the overlap and the tween
    // -----------------------------------------------------------------------

    /// A ring of kernels for one territory, `n` of them at `radius`.
    fn territory(thread: u16, centre: Px, spread: f64, radius: f64, n: usize) -> Vec<CloudKernel> {
        (0..n)
            .map(|i| {
                // A deterministic scatter with no trigonometry: the golden ratio
                // in two coprime directions, which is `plan`'s own trick.
                let a = (i as f64) * 0.618_033_99;
                let b = (i as f64) * 0.381_966_01;
                CloudKernel {
                    at: [
                        (a.fract() - 0.5).mul_add(spread, centre[0]),
                        (b.fract() - 0.5).mul_add(spread, centre[1]),
                    ],
                    radius,
                    weight: 1.0,
                    thread,
                }
            })
            .collect()
    }

    /// How many pixels are in one iso band.
    ///
    /// A fold rather than `filter(..).count()` because the band index is a `u8`
    /// and clippy reads that shape as a byte census it would like a crate for.
    fn count_band(cells: &[u8], band: u8) -> usize {
        cells.iter().fold(0, |n, b| n + usize::from(*b == band))
    }

    fn banded(bands: &BandMap) -> usize {
        bands.cells.iter().filter(|b| **b != NO_BAND).count()
    }

    fn inked(canvas: &Canvas, background: Rgb) -> usize {
        canvas
            .pixels
            .as_chunks::<3>()
            .0
            .iter()
            .filter(|p| **p != background)
            .count()
    }

    /// PRD §6.4: *"Overlap is field addition. Two territories overlapping is
    /// just a denser region, which is exactly the contention signal."*
    ///
    /// Both halves of that sentence are tested, because only the first one falls
    /// out of the arithmetic. The field adds — so two territories in one place
    /// reach a higher band than one does. And the layer can still **tell them
    /// apart**, which addition alone cannot: one thread working twice as hard
    /// and two threads standing on each other are the same number and are not
    /// the same news.
    #[test]
    fn two_territories_in_one_place_are_denser_and_are_marked_as_contested() {
        let one = territory(0, [150.0, 150.0], 40.0, 55.0, 9);
        let mut both = one.clone();
        both.extend(territory(1, [175.0, 150.0], 40.0, 55.0, 9));

        let solo = CloudField::sample(&one, 300, 300).expect("a field");
        let pair = CloudField::sample(&both, 300, 300).expect("a field");
        assert!(
            pair.peak() > solo.peak() * 1.5,
            "the field did not add: {} against {}",
            pair.peak(),
            solo.peak()
        );

        let solo_bands = solo.bands();
        let pair_bands = pair.bands();
        assert!(
            solo_bands.crowd.iter().all(|c| *c < CLOUD_CROWD),
            "one territory was drawn as contested ground"
        );
        assert!(
            pair_bands.crowd.iter().any(|c| *c >= CLOUD_CROWD),
            "two overlapping territories left no contested ground"
        );
        // And the higher band is reached where they overlap.
        assert!(
            count_band(&pair_bands.cells, 2) > count_band(&solo_bands.cells, 2),
            "overlap did not deepen the core"
        );
    }

    /// The contested notation is a **shape**: the hatch crosses.
    ///
    /// Measured by relabelling rather than by moving anything. The two frames
    /// carry the *same kernels at the same places with the same weights*, so
    /// the density field, the bands and every contour are identical to the bit;
    /// the only difference is whether the kernels claim to belong to one
    /// territory or two. Anything that changes in the image is therefore the
    /// overlap notation and nothing else.
    #[test]
    fn contested_ground_crosses_the_hatch_and_costs_no_extra_ink() {
        let bg: Rgb = [20, 21, 24];
        let mut one = territory(0, [150.0, 150.0], 40.0, 55.0, 9);
        one.extend(territory(0, [175.0, 150.0], 40.0, 55.0, 9));
        let mut two = one.clone();
        for k in two.iter_mut().skip(9) {
            k.thread = 1;
        }

        let a = CloudField::sample(&one, 300, 300).expect("a field").bands();
        let b = CloudField::sample(&two, 300, 300).expect("a field").bands();
        assert_eq!(a.cells, b.cells, "relabelling must not move a single band");

        let mut ca = Canvas::new(300, 300, bg);
        let mut cb = Canvas::new(300, 300, bg);
        paint_cloud_bands(&mut ca, &a);
        paint_cloud_bands(&mut cb, &b);
        assert_ne!(ca.pixels, cb.pixels, "the overlap is not drawn at all");

        // Ink where the other frame has none, in both directions: the crossing
        // hatch adds a second family of strokes and gives back some of the
        // first, which is what keeps the *quantity* of ink about where it was.
        let (mut only_b, mut only_a) = (0usize, 0usize);
        for (pa, pb) in ca
            .pixels
            .as_chunks::<3>()
            .0
            .iter()
            .zip(cb.pixels.as_chunks::<3>().0.iter())
        {
            if pa == &bg && pb != &bg {
                only_b += 1;
            }
            if pa != &bg && pb == &bg {
                only_a += 1;
            }
        }
        assert!(only_b > 40, "the crossing strokes are missing: {only_b}");
        assert!(only_a > 40, "the spacing did not open up: {only_a}");
        // The whole reason the spacing opens up: crossing two hatches at the
        // original spacing would double the ink and fog the city under it.
        let (ia, ib) = (inked(&ca, bg), inked(&cb, bg));
        assert!(
            ib < ia * 5 / 4,
            "contested ground inked {ib} against {ia}: that is a second fill"
        );
    }

    /// PRD §6.4's bandwidth rule, made visible: *"wide and diffuse with three
    /// observations; tightens as evidence accumulates."*
    ///
    /// `polis_world` computes `bandwidth = base / sqrt(effective_n)`, clamped,
    /// and hands it over as [`CloudKernel::radius`]. The claim this test makes is
    /// the renderer's half of the bargain — that the number **reaches the
    /// picture**: the same territory observed three times and thirty times draws
    /// two visibly different shapes, wide-and-coreless against tight-and-cored.
    /// If the layer ever normalised the field, or fixed the radius, or clamped
    /// the bands to a shape, this test is what would notice.
    #[test]
    fn the_cloud_is_wide_when_the_evidence_is_thin_and_tight_when_it_is_not() {
        let base = 60.0;
        let measure = |n: usize| -> (usize, usize) {
            // `base / sqrt(n)`, exactly as `polis_world::territory::bandwidth`.
            let radius = base / (n as f64).sqrt();
            let kernels = territory(0, [150.0, 150.0], 30.0, radius, n);
            let bands = CloudField::sample(&kernels, 300, 300)
                .expect("a field")
                .bands();
            (banded(&bands), count_band(&bands.cells, 2))
        };
        let (thin_area, thin_core) = measure(3);
        let (thick_area, thick_core) = measure(30);
        assert!(thin_area > 0, "three observations drew nothing at all");
        assert_eq!(
            thin_core, 0,
            "three observations claimed a core: the uncertainty is not being drawn"
        );
        assert!(
            thick_core > 0,
            "thirty observations never resolved a core: the evidence is not being drawn"
        );
        assert!(
            thick_area < thin_area,
            "the cloud did not tighten as evidence accumulated: {thick_area} against {thin_area}"
        );
    }

    /// PRD §6.4: *"Multi-lobed shapes come free … truthful, where a bounding
    /// rectangle would falsely claim the empty space between."*
    #[test]
    fn a_thread_working_in_two_places_gets_two_lobes_and_not_a_rectangle() {
        let mut kernels = territory(0, [80.0, 150.0], 30.0, 30.0, 8);
        kernels.extend(territory(0, [230.0, 150.0], 30.0, 30.0, 8));
        let bands = CloudField::sample(&kernels, 310, 300)
            .expect("a field")
            .bands();
        let at = |x: usize| bands.at(x, 150);
        assert_ne!(at(80), NO_BAND, "the left lobe is missing");
        assert_ne!(at(230), NO_BAND, "the right lobe is missing");
        assert_eq!(
            at(155),
            NO_BAND,
            "the empty space between the lobes was claimed"
        );
    }

    /// A contour is a line on a map, not a coat of paint, and it has to stay one
    /// at the size real territories come out at.
    ///
    /// The fixed three-pixel outer contour was tuned on a single large
    /// validation cloud. On the operator's own sessions a banded region is a few
    /// hundred pixels, and measured there the fixed width inked 68–84 % of it —
    /// the layer passing every synthetic fill test and behaving as a fill on real
    /// evidence.
    #[test]
    fn a_contour_narrows_on_a_small_cloud_and_never_widens_past_its_ceiling() {
        let small = CloudField::sample(&territory(0, [60.0, 60.0], 6.0, 14.0, 4), 300, 300)
            .expect("a field")
            .bands();
        let large = CloudField::sample(&territory(0, [150.0, 150.0], 90.0, 120.0, 24), 300, 300)
            .expect("a field")
            .bands();
        let (s, l) = (contour_steps(&small), contour_steps(&large));
        assert_eq!(s[0], 1, "a small cloud got a fat contour: {s:?}");
        assert!(
            l[0] > s[0],
            "a large cloud drew the same hairline as a small one: {l:?} against {s:?}"
        );
        for (k, step) in l.iter().enumerate() {
            assert!(
                *step <= CLOUD_CONTOUR_WIDTH[k] as isize,
                "band {k} exceeded its ceiling: {l:?}"
            );
        }
        // And the point of the whole exercise, on pixels: a cloud big enough
        // for "fog" to mean anything leaves most of its own ground to the city.
        // (A twenty-pixel speck cannot — a ring around it *is* most of it — and
        // a speck is a marker, not a fog.)
        let bg: Rgb = [20, 21, 24];
        let mut canvas = Canvas::new(300, 300, bg);
        paint_cloud_bands(&mut canvas, &large);
        let share = 100 * inked(&canvas, bg) / banded(&large).max(1);
        assert!(
            share <= 35,
            "the cloud inks {share}% of its own banded region: that is a fill"
        );
        assert!(share >= 4, "the cloud inks {share}%: invisible");
    }

    /// PRD §13: *"tween agent positions, cloud density, and building heights
    /// between updates … the entire difference between alive and steppy."*
    #[test]
    fn a_cloud_eases_in_and_dissipates_instead_of_appearing_and_vanishing() {
        let kernels = territory(0, [150.0, 150.0], 40.0, 55.0, 9);
        let target = CloudField::sample(&kernels, 300, 300).expect("a field");
        let full = target.peak();

        let mut tween = CloudTween::default();
        let dt = 1.0 / 24.0;
        let first = tween
            .advance(Some(target.clone()), dt, CLOUD_TWEEN_RATE)
            .expect("a field")
            .peak();
        assert!(
            first > 0.0 && first < full * 0.5,
            "a cloud arrived at {first} of {full} in one frame: that is a pop"
        );
        // …and gets there.
        for _ in 0..60 {
            tween.advance(Some(target.clone()), dt, CLOUD_TWEEN_RATE);
        }
        let settled = tween.field().expect("a field").peak();
        assert!(
            (settled - full).abs() < full * 0.02,
            "the tween never arrived: {settled} of {full}"
        );

        // The world drops the territory. It dissipates rather than vanishing,
        // and it does eventually go — PRD §10.4's "let dormant territories
        // dissipate entirely".
        let mut frames = 0;
        while tween.advance(None, dt, CLOUD_TWEEN_RATE).is_some() {
            frames += 1;
            assert!(frames < 600, "the cloud never dissipated");
        }
        assert!(
            frames > 12,
            "the cloud vanished in {frames} frames: that is a cut, not a fade"
        );
    }

    /// A territory that drifts changes the lattice under it, and the tween has
    /// to carry the old field across rather than restart.
    ///
    /// This is the case a kernel-by-kernel tween cannot handle at all, and the
    /// reason the lattice is what gets interpolated.
    #[test]
    fn the_tween_carries_the_field_across_a_change_of_lattice() {
        let here = CloudField::sample(&territory(0, [90.0, 150.0], 40.0, 55.0, 9), 400, 300)
            .expect("a field");
        let there = CloudField::sample(&territory(0, [300.0, 150.0], 40.0, 55.0, 9), 400, 300)
            .expect("a field");
        assert!(!here.aligned_with(&there), "the test needs two lattices");

        let mut tween = CloudTween::default();
        for _ in 0..60 {
            tween.advance(Some(here.clone()), 1.0 / 24.0, CLOUD_TWEEN_RATE);
        }
        let settled = tween.field().expect("a field").peak();

        // One frame toward the new place. The field must be *between* the two,
        // not either of them, and the old mass must still be on the map.
        let moved = tween
            .advance(Some(there.clone()), 1.0 / 24.0, CLOUD_TWEEN_RATE)
            .expect("a field")
            .clone();
        assert!(
            moved.at(90.0, 150.0) > settled * 0.5,
            "the cloud teleported: nothing left where it was"
        );
        assert!(
            moved.at(300.0, 150.0) > 0.0,
            "the cloud has not started moving"
        );
        assert!(
            moved.at(300.0, 150.0) < there.at(300.0, 150.0) * 0.5,
            "the cloud arrived in one frame"
        );
    }

    /// PRD §11.4: peripheral vision is poor at colour. So "this worker is still
    /// running" is carried by the tether's **line style**, not by three levels of
    /// grey inside one band.
    #[test]
    fn a_finished_workers_tether_is_dashed_and_a_running_one_is_solid() {
        let bg: Rgb = [20, 21, 24];
        let draw_one = |running: bool| -> Vec<bool> {
            let mut c = Canvas::new(300, 60, bg);
            draw_tether(
                &mut c,
                Tether {
                    tint: 0,
                    anchor: [20.0, 30.0],
                    worker: [280.0, 30.0],
                    running,
                    spread: 0.0,
                },
                12.0,
            );
            // Is there ink in this column at all?
            (0..300)
                .map(|x| {
                    (0..60).any(|y| {
                        let i = (y * 300 + x) * 3;
                        c.pixels[i..i + 3] != bg
                    })
                })
                .collect()
        };
        let solid = draw_one(true);
        let dashed = draw_one(false);
        let runs = |cols: &[bool]| {
            cols.windows(2)
                .filter(|w| w[0] && !w[1])
                .count()
                .max(usize::from(cols[cols.len() - 1]))
        };
        assert_eq!(runs(&solid), 1, "a running tether is one unbroken line");
        assert!(
            runs(&dashed) >= 3,
            "a finished worker's tether is not dashed: {} runs",
            runs(&dashed)
        );
        assert!(
            dashed.iter().filter(|c| **c).count() < solid.iter().filter(|c| **c).count(),
            "the dashed tether is not sparser than the solid one"
        );
    }

    /// The tether was made **brighter** when it stopped being ambient, and the
    /// thing that has to stay true is the ordering: the line that identifies a
    /// worker may not outshine the worker, the trail, or anything in layer 5.
    ///
    /// Measured on rendered pixels rather than argued from the constants,
    /// because `fade` and the rasteriser's coverage both sit between the two.
    #[test]
    fn an_asked_for_tether_reads_without_outshining_what_it_identifies() {
        let peak_of = |f: &dyn Fn(&mut Canvas)| -> u8 {
            let mut c = Canvas::new(300, 60, [0, 0, 0]);
            f(&mut c);
            c.pixels.iter().copied().max().unwrap_or(0)
        };
        let tether = |running: bool| {
            move |c: &mut Canvas| {
                draw_tether(
                    c,
                    Tether {
                        tint: 0,
                        anchor: [20.0, 30.0],
                        worker: [280.0, 30.0],
                        running,
                        spread: 0.0,
                    },
                    12.0,
                );
            }
        };
        let running = peak_of(&tether(true));
        let finished = peak_of(&tether(false));
        let body = peak_of(&|c: &mut Canvas| {
            draw_agent(
                c,
                Agent {
                    tint: 0,
                    at: [150.0, 30.0],
                    body: Body::Worker,
                    outcome: Outcome::Pending,
                    travel: 0.0,
                    heading: [1.0, 0.0],
                    waiting: false,
                },
                12.0,
            );
        });
        println!("tether running {running}, finished {finished}; worker body {body}");
        for (name, v) in [("running", running), ("finished", finished)] {
            assert!(
                (AGENT_BAND.0..=AGENT_BAND.1).contains(&v),
                "a {name} tether peaked at {v}, outside the agent band {AGENT_BAND:?}"
            );
        }
        assert!(
            finished < running,
            "a finished worker's tether ({finished}) is not quieter than a running one ({running})"
        );
        assert!(
            running < head(AGENT_TRAIL),
            "a tether ({running}) is not quieter than a trail ({})",
            head(AGENT_TRAIL)
        );
        assert!(
            running < body,
            "the line ({running}) outshines the worker it identifies ({body})"
        );
    }
}
