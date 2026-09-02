//! The static city plan: a repository rendered to a PNG (PRD §15 M1).
//!
//! This is the product path from a repository to an image. `polis snapshot` runs
//! it; the M1 gate no longer needs a harness of its own.
//!
//! # PRD §10.3's contrast budget is enforced here, not observed here
//!
//! > **Dynamic range, not omission.** Draw everything, but keep layers 1–2
//! > inside roughly the bottom fifth of the contrast range and spend the rest on
//! > layers 4–5. Weather charts do exactly this: the coastline is always drawn
//! > and always faint; the storm system gets the ink.
//!
//! The first renders did the opposite: saturated categorical district fills and
//! paper-white buildings owned the whole range, so the base map had nothing left
//! to give M4's clouds and M5's attention marks. The fix is a hard ceiling, and
//! it is a **structural** one rather than a palette convention, because a
//! palette convention is one careless constant away from being violated again:
//!
//! * every base-map draw goes through `MapInk`, which clamps each channel to
//!   [`BASE_MAP_CEILING`] before it reaches the canvas — unconditionally, in
//!   release as well as debug;
//! * because the rasteriser blends per channel and every channel starts and
//!   stays at or below the ceiling, **no blend, no antialiased edge and no
//!   box-downsample can lift a base-map pixel above it**. `Y` is a convex
//!   combination of the linearised channels, so `Y ≤ lin(ceiling)` and hence
//!   `L* ≤ L*(grey(ceiling))`. That is the invariant, and it is provable rather
//!   than measured;
//! * [`render_base_map`] draws the map layers with no text at all, so the
//!   invariant is testable on real pixels.
//!
//! ## The allocation
//!
//! Channel values are 8-bit sRGB; the `L*` column is CIE lightness of the grey
//! at that channel value, which bounds the `L*` of *any* colour with that
//! maximum channel.
//!
//! | Layer (PRD §10.3) | Channels | `L*` | M1 |
//! |---|---|---|---|
//! | 0 — sea, off-map terrain | 6–21 | 2–8 | drawn |
//! | 1 — terrain, vacant lots | 15–26 | 6–10 | drawn |
//! | 2 — city: ground, lots, roads, buildings | 9–48 | 3–20 | drawn |
//! | 3 — clouds (territory density) | 49–96 | 20–40 | **reserved, M4** |
//! | 4 — agents: workers, trails, tethers | 97–168 | 40–68 | **reserved, M4** |
//! | 5 — attention: the three states | 169–255 | 68–100 | **reserved, M5** |
//!
//! Layers 1–2 therefore occupy `L* ≤ 19.9` — the bottom fifth of the perceptual
//! range — and four fifths of it is untouched and waiting.
//!
//! **Text is not one of the five layers.** PRD §13 puts labels in a UI overlay
//! rather than in the map, and a label that cannot be read is not a label. So
//! the overlay is exempt from the ceiling, and is instead held inside the
//! *agent* band (`L* ≈ 47–62`) so that M5's attention marks still own the top,
//! and is area-capped by the declutterer in `draw_labels`.
//!
//! ## Colour space
//!
//! ADR-0021 asks that the colour-space convention be settled before any §10.3
//! tuning. For **this** path it is settled by construction: the rasteriser
//! writes 8-bit sRGB straight into the PNG, with no surface format and no
//! implicit encode. The budget is therefore stated in 8-bit sRGB channel space,
//! which is unambiguous here and is a value the wgpu path can reproduce after
//! converting to whatever `render_state.target_format` turns out to be — the
//! ceiling travels with the palette, not with the backend.
//!
//! # The presentation scheme
//!
//! Taken from the design bake-off's `voronoi-organic` packet, which lost on
//! structure and won on presentation:
//!
//! * **Hue families keyed on the top-level directory.** A package reads as one
//!   colour family whatever its internal depth, so `web/store/session` and
//!   `web/ui/panel` are visibly the same neighbourhood. Hue carries the package;
//!   a small lightness nudge separates siblings. All of it at low chroma: the
//!   ground is a tint, and the built mass carries the image.
//! * **In-map district labels**, decluttered, with the package skeleton and the
//!   monuments taking priority over ordinary quarters (PRD §8).
//! * **A drawn coastline.** Not the convex hull — the true boundary of the
//!   settled ground, with a soft shelf outside it so the city reads as a place
//!   in a landscape rather than a coin on a table.
//! * **A metrics footer**, from `accretion`'s renders: the numbers that decide
//!   whether the picture is a city or a diagram, printed on the picture.
//!
//! # Height is drawn, not stated
//!
//! PRD §7.3 makes building height — uncommitted diff lines — the primary encoded
//! quantity, and the first renders drew every building at one RGB value, so the
//! quantity the map exists to show was invisible. PRD §12 forbids orbit,
//! perspective and tilt, so height must be **encoded, not projected**. The three
//! channels used here are the standard flat-camera vocabulary:
//!
//! 1. **Cast shadow length**, proportional to height, from a fixed sun. A
//!    shadow is a lighting cue, not a camera tilt: the plan stays orthographic.
//! 2. **Roof lightness**, on a *fixed* reference rather than the observed
//!    maximum — for the same reason ADR-0020 fixes the iso thresholds. A ramp
//!    normalised per render would make a calm repository look dramatic and
//!    would move every building's tone when one file's diff changed.
//! 3. **Keyline weight**, which adds ink rather than brightness and so survives
//!    the downsample to thumbnail size.
//!
//! Roof form (PRD §7.3's silhouette variety) is drawn as facets, which is what
//! gives a building its second and third tone.
//!
//! # Nothing here reads a clock
//!
//! The footer prints structure, never a timing. A wall-clock number in the image
//! would make the PNG non-reproducible while the layout underneath was perfect —
//! which is exactly the class of leak PRD §7.4 exists to prevent, and it is a
//! mistake the bake-off actually made. Timings go to stdout.
//!
//! Nor does anything here reach for a transcendental. The terrain wash is
//! integer-hashed value noise with a polynomial fade, and the sun is a constant
//! unit vector — no `sin`, no `ln`, no `powf`, so the bytes are the same on
//! every libm.

// The middle of the pipeline is numeric geometry, and five lint families fire on
// nearly every line of it without telling us anything:
//
// * the `cast_*` family — every cast here lands in a bucket index or a quantised
//   sort key that is clamped or wrapped on purpose;
// * `float_cmp` — exact float comparison is how a determinism tie is broken
//   (PRD §7.4), and an approximate comparison there would be the bug;
// * `many_single_char_names` and `similar_names` — `a`, `b`, `c`, `n`, `p` are
//   the names the geometry itself uses;
// * `too_many_lines` — a pipeline stage read as one ordered sequence is clearer
//   than the same code cut into fragments each called once;
// * `assigning_clones` — the buffers reassigned here are rebuilt from scratch,
//   so `clone_from` would save nothing.
#![allow(
    clippy::assigning_clones,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::float_cmp,
    clippy::many_single_char_names,
    clippy::similar_names,
    clippy::too_many_lines
)]

use std::collections::{BTreeMap, BTreeSet};

use polis_events::LogicalPath;
use polis_layout::city::{City, Structure};
use polis_layout::{Point, RoadClass, RoofForm};

use crate::raster::{hsl, Canvas, Px, Rgb};

// ---------------------------------------------------------------------------
// PRD §10.3 — the contrast budget
// ---------------------------------------------------------------------------

/// The highest 8-bit sRGB channel value any **base-map** ink may take.
///
/// `L*` of the grey at 48 is 19.90, so every layer 1–2 pixel lands in the bottom
/// fifth of the perceptual range. Because relative luminance is a convex
/// combination of the linearised channels, a colour whose largest channel is `v`
/// can never be lighter than the grey at `v` — which is what makes a *channel*
/// ceiling a sound way to enforce an `L*` ceiling through arbitrary blending.
pub const BASE_MAP_CEILING: u8 = 48;

/// The lowest channel value reserved for layer 3 (clouds).
///
/// Nothing in M1 draws here. It is stated so that M4 has a documented band to
/// land in rather than a gap it has to guess at.
pub const CLOUD_BAND: (u8, u8) = (49, 96);

/// The band reserved for layer 4 (agents, trails, tethers). Unused in M1.
pub const AGENT_BAND: (u8, u8) = (97, 168);

/// The band reserved for layer 5 (attention). Unused in M1, and it owns the top
/// of the range: nothing else in the renderer may enter it.
pub const ATTENTION_BAND: (u8, u8) = (169, 255);

/// The highest channel any **ground** ink may take.
///
/// The band is split in two. Ground — terrain, district fill, lots, the shore —
/// gets the bottom half; the built mass and the road hierarchy get the top. A
/// roof darker than the field it stands in stops reading as a building, and a
/// base map whose brightest ink is its background has nothing left to draw a
/// city with, which is precisely how 28 of the available 48 levels came to be
/// the whole of the first render's range.
const GROUND_CEILING: u8 = 26;

/// The lowest channel a district tint may take.
///
/// Chroma is paid for out of the *darker* channels (see [`ground_tint`]), so a
/// floor is what stops a hue family turning into a saturated stripe.
const GROUND_FLOOR: u8 = 6;

/// The most chroma a district tint may carry, in channel levels.
///
/// Without a cap the construction below gives every hue the most chroma its own
/// channel window allows — and that is wildly unequal: at this ceiling a cyan
/// quarter can carry 24 levels where a blue one can only carry 14, so the cyan
/// districts shout and the blue ones whisper. A shared cap makes hue a
/// *categorical* channel again, which is what the directory tree needs it to be.
const GROUND_CHROMA: f64 = 18.0;

/// The luminance window every district tint is held inside.
///
/// Districts are separated by **hue**, not by lightness: lightness is the
/// budgeted quantity and hue is free. Holding every district inside a six-level
/// window is what leaves the whole upper half of the band to the buildings.
const GROUND_LUMA: (f64, f64) = (11.0, 19.0);

/// Deep water / far off-map ground. At the bottom of the range, as PRD §10.3's
/// weather chart has it: the sea is not drawn, it is what is left.
const SEA_DEEP: Rgb = [3, 4, 6];
/// The water margin immediately outside the shore. One narrow band, not a glow.
const SEA_MID: Rgb = [7, 8, 11];
/// The coastal shelf, where terrain meets the settled ground.
const SHELF: Rgb = [14, 16, 20];
/// Settled ground with nothing on it.
const LAND: Rgb = [19, 21, 24];
/// A vacant lot, gone to seed (PRD §7.5).
const VACANT: Rgb = [22, 24, 21];
/// The keyline between two blocks.
const BLOCKLINE: Rgb = [6, 7, 9];
/// Lot boundary.
const LOTLINE: Rgb = [29, 30, 33];
/// The tint a lot is lifted toward, out of its district's ground colour.
const LOT_LIFT: Rgb = [29, 30, 32];
/// Widest road. At the top of the base band: channel 48 is a perfectly visible
/// grey against black, and the arterial skeleton is what a city is read along.
const ROAD_HI: Rgb = [47, 48, 48];
/// Middle road.
const ROAD_MID: Rgb = [39, 40, 43];
/// Narrowest road.
const ROAD_LO: Rgb = [30, 31, 34];
/// The roof of the shortest building.
///
/// Every roof tone sits above every ground tone: the built mass is the lightest
/// thing in the base map, which is what makes an aerial read as a city rather
/// than as a coloured diagram with pebbles on it. `ROOF_LO` sits one level above
/// [`GROUND_CEILING`], so the ramp starts the moment the ground stops.
const ROOF_LO: Rgb = [29, 28, 26];
/// The roof of a building at [`HEIGHT_REFERENCE`] or above.
const ROOF_HI: Rgb = [48, 48, 45];
/// The lit edge of a tall building, and the only ink allowed to sit on the
/// base-map ceiling in every channel.
const ROOF_TOP: Rgb = [48, 48, 46];
/// The keyline around a roof.
const ROOF_EDGE: Rgb = [8, 8, 10];
/// What a cast shadow is mixed toward. Near the sea, so a shadow reads as an
/// absence of light rather than as a grey patch.
const SHADOW: Rgb = [4, 4, 6];
/// Monument (PRD §8): brass, and the only warm ink in the base map.
const MONU: Rgb = [48, 40, 20];
/// The monument's plinth.
const MONU_DIM: Rgb = [26, 22, 12];
/// Industrial ground (PRD §8) — deliberately dull, and deliberately *occupied*:
/// at the same luminance as settled land, so the eye slides off it instead of
/// falling into it.
const INDUS: Rgb = [17, 17, 16];
/// The hatch drawn across an industrial mass. One tone, one spacing, one angle:
/// uniform enough to carry no information and present enough to be ground.
const INDUS_HATCH: Rgb = [30, 30, 28];
/// Import route (PRD §9).
const STREET: Rgb = [20, 46, 43];
/// District and package border.
const BORDER: Rgb = [46, 45, 40];
/// The drawn coastline. Near the top of the band and one stroke wide: the
/// silhouette is what the squint test reads, and a faint *crisp* line reads
/// where a faint soft one does not.
const LIMIT: Rgb = [45, 47, 48];

// The text overlay. Not one of §10.3's five layers (PRD §13 puts text in a UI
// overlay), but still held below the attention band so M5 owns the top.
/// Label plate.
const PLATE: Rgb = [8, 9, 12];
/// District label text.
const LABEL: Rgb = [138, 143, 152];
/// Monument label text.
const LABEL_MONU: Rgb = [166, 140, 86];
/// Footer body text.
const FOOTER_TEXT: Rgb = [108, 113, 124];
/// Title text.
const TITLE: Rgb = [166, 172, 182];

/// The height that maps to `ROOF_HI` and the longest shadow.
///
/// Fixed, never the observed maximum (ADR-0020's lesson, applied to tone): a
/// ramp rescaled per render would make a quiet repository look dramatic, and
/// would move every building's tone the moment one file's diff changed — which
/// is the opposite of the spatial memory PRD §7.4 exists to protect.
pub const HEIGHT_REFERENCE: f32 = 22.0;

/// Where `polis_layout`'s **settled** register lands on the tonal ramp.
///
/// `polis_layout::buildings` builds height out of two registers: a settled one
/// running `BASE_HEIGHT..SETTLED_CEILING` off file size, and a work one running
/// far above it off uncommitted diff lines. A single linear ramp to
/// [`HEIGHT_REFERENCE`] squeezes the settled register — which is *every*
/// building on a clean checkout — into the bottom sixth of the tone range, and
/// that is why the earlier renders came out with one roof grey and 5 000
/// identical flecks. The ramp is therefore piecewise, one segment per register:
/// the settled register owns the bottom half of the tone range so file size is
/// legible on a quiet repository, and the work register owns the top half so
/// PRD §7.3 still holds — the tallest thing on the map is the biggest
/// unreviewed pile.
const SETTLED_TONE: f64 = 0.80;

/// Sun direction, as the offset a unit of height casts its shadow along.
///
/// Light from the north-west is the cartographic convention. A constant unit
/// vector, so no trigonometry reaches the image (PRD §7.4).
const SUN: [f64; 2] = [0.6, 0.8];

/// The axis across the sun: [`SUN`] turned a quarter, and still exactly unit.
///
/// Roof form is cut on this axis and height is cut on [`SUN`], so a decorative
/// facet and the encoded quantity never land on the same mark.
const RIDGE: [f64; 2] = [-0.8, 0.6];

// ---------------------------------------------------------------------------
// Ink that cannot leave the base-map band
// ---------------------------------------------------------------------------

/// Clamp a colour into the base-map band.
#[must_use]
pub fn base_ink(colour: Rgb) -> Rgb {
    [
        colour[0].min(BASE_MAP_CEILING),
        colour[1].min(BASE_MAP_CEILING),
        colour[2].min(BASE_MAP_CEILING),
    ]
}

/// A view of the canvas that can only paint base-map ink.
///
/// Every layer 1–2 draw goes through here. This is the enforcement point for
/// PRD §10.3, and it is a type rather than a convention because a convention is
/// one careless constant away from the render that lost the range in the first
/// place.
struct MapInk<'a> {
    canvas: &'a mut Canvas,
}

impl MapInk<'_> {
    fn fill_polygon(&mut self, points: &[Px], colour: Rgb, alpha: f64) {
        self.canvas.fill_polygon(points, base_ink(colour), alpha);
    }

    fn stroke_polygon(&mut self, points: &[Px], width: f64, colour: Rgb, alpha: f64) {
        self.canvas
            .stroke_polygon(points, width, base_ink(colour), alpha);
    }

    fn polyline(&mut self, points: &[Px], width: f64, colour: Rgb, alpha: f64) {
        self.canvas.polyline(points, width, base_ink(colour), alpha);
    }
}

/// Linear blend between two colours, `t` in `[0, 1]`.
#[must_use]
pub fn mix(a: Rgb, b: Rgb, t: f64) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    let ch = |i: usize| -> u8 {
        (f64::from(a[i]) + (f64::from(b[i]) - f64::from(a[i])) * t).round() as u8
    };
    [ch(0), ch(1), ch(2)]
}

// ---------------------------------------------------------------------------
// The view
// ---------------------------------------------------------------------------

/// A world-to-pixel transform that fits a bounding box into a canvas.
#[derive(Debug, Clone, Copy)]
pub struct View {
    scale: f64,
    offset_x: f64,
    offset_y: f64,
    height: f64,
}

impl View {
    /// Fit a world bounding box into `width × height` pixels with a margin.
    #[must_use]
    pub fn fit(lo: [f64; 2], hi: [f64; 2], width: usize, height: usize, margin: f64) -> Self {
        let dw = (hi[0] - lo[0]).max(1e-6);
        let dh = (hi[1] - lo[1]).max(1e-6);
        let scale = ((width as f64 - 2.0 * margin) / dw).min((height as f64 - 2.0 * margin) / dh);
        Self {
            scale,
            offset_x: margin - lo[0] * scale + ((width as f64 - 2.0 * margin) - dw * scale) * 0.5,
            offset_y: margin - lo[1] * scale + ((height as f64 - 2.0 * margin) - dh * scale) * 0.5,
            height: height as f64,
        }
    }

    /// World point to pixel. The world's `y` points up; the image's points down.
    #[must_use]
    pub fn at(&self, p: Point) -> Px {
        [
            f64::from(p.x) * self.scale + self.offset_x,
            self.height - (f64::from(p.y) * self.scale + self.offset_y),
        ]
    }

    /// Project a whole ring.
    #[must_use]
    pub fn ring(&self, points: &[Point]) -> Vec<Px> {
        points.iter().map(|p| self.at(*p)).collect()
    }

    /// Pixels per world unit.
    #[must_use]
    pub fn scale(&self) -> f64 {
        self.scale
    }
}

// ---------------------------------------------------------------------------
// District colour: hue by package, at low chroma
// ---------------------------------------------------------------------------

/// Rec. 709 luma on the sRGB-encoded bytes.
///
/// Deliberately *not* linearised relative luminance: PRD §10.3's budget is
/// stated in channel space, the weights are exact decimal constants, and no
/// transcendental is involved — so the same bytes come out on every libm, which
/// `powf` would not guarantee.
fn luma(c: [f64; 3]) -> f64 {
    0.2126f64.mul_add(c[0], 0.7152f64.mul_add(c[1], 0.0722 * c[2]))
}

/// The most chromatic colour of a given hue that fits the ground's window.
///
/// This is where PRD §10.3's budget is *spent* rather than merely respected.
/// Adding a constant to all three channels moves luminance and leaves hue
/// exactly where it was, so hue costs nothing out of the luminance budget:
/// districts can therefore be separated by hue at a luminance held inside a
/// six-level window, instead of by the lightness steps that the earlier palette
/// spent a third of its range on.
///
/// Construction: take the fully saturated colour of this hue, subtract its own
/// luma to get a pure chroma direction (zero luma, because the three weights sum
/// to one), then push out along that direction from a grey until either
/// [`GROUND_CEILING`] or [`GROUND_FLOOR`] binds. The grey is chosen so that both
/// bind at once, which is the most chroma this hue can carry, and is then
/// clamped into [`GROUND_LUMA`] so the ground never brightens into the
/// buildings' half of the band.
fn ground_tint(hue: f64, lift: f64) -> Rgb {
    let raw = hsl(hue, 1.0, 0.5);
    let base = [f64::from(raw[0]), f64::from(raw[1]), f64::from(raw[2])];
    let l = luma(base);
    let d = [base[0] - l, base[1] - l, base[2] - l];
    let hi = d.iter().copied().fold(f64::MIN, f64::max);
    let lo = d.iter().copied().fold(f64::MAX, f64::min);
    let cap = f64::from(GROUND_CEILING);
    let floor = f64::from(GROUND_FLOOR);
    let balanced = if hi - lo > 1e-9 {
        floor.mul_add(hi, cap * -lo) / (hi - lo)
    } else {
        f64::midpoint(GROUND_LUMA.0, GROUND_LUMA.1)
    };
    let y =
        (balanced.clamp(GROUND_LUMA.0, GROUND_LUMA.1) + lift).clamp(GROUND_LUMA.0, GROUND_LUMA.1);
    let head = if hi > 1e-9 { (cap - y) / hi } else { f64::MAX };
    let foot = if lo < -1e-9 {
        (y - floor) / -lo
    } else {
        f64::MAX
    };
    let reach = hi - lo;
    let cap_s = if reach > 1e-9 {
        GROUND_CHROMA / reach
    } else {
        f64::MAX
    };
    let s = head.min(foot).min(cap_s).max(0.0);
    let ch = |i: usize| -> u8 { s.mul_add(d[i], y).round().clamp(0.0, 255.0) as u8 };
    [ch(0), ch(1), ch(2)]
}

/// Hue by top-level directory, a small luminance nudge by depth: the directory
/// tree, as colour — at the most chroma the ground's window will carry.
///
/// Adjacent quarters of the same package read as one family, which is what makes
/// a package legible at a zoom where its individual districts are not. JUDGEMENT
/// recommended this scheme from `voronoi-organic`; the difference here is that
/// it is drawn as a **tint** whose largest channel never leaves the ground half
/// of the band, so the built mass carries the image and layers 3–5 keep their
/// allocation.
#[must_use]
pub fn district_colours(city: &City) -> BTreeMap<LogicalPath, Rgb> {
    let mut tops: Vec<&str> = city
        .layout
        .districts
        .keys()
        .map(|p| p.components().next().unwrap_or(""))
        .collect();
    tops.sort_unstable();
    tops.dedup();
    let n = tops.len().max(1) as f64;
    let industrial: BTreeSet<&LogicalPath> = city.industrial.iter().map(|m| &m.host).collect();

    let mut out = BTreeMap::new();
    for path in city.layout.districts.keys() {
        let top = path.components().next().unwrap_or("");
        let index = tops.iter().position(|t| *t == top).unwrap_or(0) as f64;
        // A per-district nudge inside the family, so siblings are told apart
        // without leaving the package's hue.
        let nudge = ((fnv1a64(path.as_str().as_bytes()) >> 11) & 0xFFFF) as f64 / 65_535.0;
        let hue = index / n + (nudge - 0.5) * (0.24 / n);
        let depth = path.depth() as f64;
        if industrial.contains(path) {
            out.insert(path.clone(), INDUS);
            continue;
        }
        // The nudge is a *luminance* nudge of at most a couple of levels: enough
        // to tell two quarters of one package apart at street level, far too
        // little to read as a second encoding at city zoom.
        let lift = (nudge - 0.5).mul_add(1.6, depth * 0.55).clamp(-1.4, 2.2);
        out.insert(path.clone(), base_ink(ground_tint(hue, lift)));
    }
    out
}

/// FNV-1a, so the colour nudge is stable without depending on the layout crate's
/// internals.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The bounding box of every block.
fn world_bounds(city: &City) -> ([f64; 2], [f64; 2]) {
    let mut lo = [f64::INFINITY, f64::INFINITY];
    let mut hi = [f64::NEG_INFINITY, f64::NEG_INFINITY];
    for block in &city.layout.blocks {
        for v in &block.boundary.vertices {
            lo[0] = lo[0].min(f64::from(v.x));
            lo[1] = lo[1].min(f64::from(v.y));
            hi[0] = hi[0].max(f64::from(v.x));
            hi[1] = hi[1].max(f64::from(v.y));
        }
    }
    if !lo[0].is_finite() {
        let e = f64::from(city.layout.extent).max(1.0);
        return ([-e, -e], [e, e]);
    }
    (lo, hi)
}

/// Which road segments separate two different districts, and which two different
/// top-level packages.
///
/// A district border in this city **is** a street — the districts tile the block
/// graph, so a border is exactly the set of road edges with a different district
/// on each side. Recovered here by matching block ring edges back onto road
/// nodes, so the renderer needs nothing the layout does not already publish.
fn borders(city: &City) -> (BTreeSet<usize>, BTreeSet<usize>) {
    let mut node_at: BTreeMap<(i64, i64), u32> = BTreeMap::new();
    for (i, n) in city.layout.roads.nodes.iter().enumerate() {
        node_at.insert(grid_key(n.position), u32::try_from(i).unwrap_or(u32::MAX));
    }
    let mut segment_at: BTreeMap<(u32, u32), usize> = BTreeMap::new();
    for (i, s) in city.layout.roads.segments.iter().enumerate() {
        segment_at.insert((s.from.0.min(s.to.0), s.from.0.max(s.to.0)), i);
    }
    let mut sides: BTreeMap<usize, BTreeSet<LogicalPath>> = BTreeMap::new();
    for block in &city.layout.blocks {
        let ring = &block.boundary.vertices;
        for i in 0..ring.len() {
            let a = node_at.get(&grid_key(ring[i]));
            let b = node_at.get(&grid_key(ring[(i + 1) % ring.len()]));
            if let (Some(&a), Some(&b)) = (a, b) {
                if let Some(&e) = segment_at.get(&(a.min(b), a.max(b))) {
                    sides.entry(e).or_default().insert(block.district.clone());
                }
            }
        }
    }
    let mut district = BTreeSet::new();
    let mut package = BTreeSet::new();
    for (e, set) in sides {
        if set.len() < 2 {
            continue;
        }
        district.insert(e);
        let tops: BTreeSet<&str> = set
            .iter()
            .map(|p| p.components().next().unwrap_or(""))
            .collect();
        if tops.len() > 1 {
            package.insert(e);
        }
    }
    (district, package)
}

/// A vertex, quantised so that two blocks meeting along an edge agree about
/// where its endpoints are.
type Key = (i64, i64);

/// An undirected edge between two quantised vertices, smaller endpoint first.
type Edge = (Key, Key);

/// One building, prepared for drawing: footprint in pixels, its centre, the
/// square root of its footprint area, where its height lands on the fixed ramp,
/// and the ground tone its shadow falls on.
type Massing<'a> = (&'a polis_layout::Building, Vec<Px>, Px, f64, f64, Rgb);

/// Quantised vertex key. Blocks are faces of one planar subdivision, so two
/// blocks that meet along an edge share its endpoints exactly; the quantisation
/// is belt-and-braces against a stage boundary rounding differently.
fn grid_key(p: Point) -> Key {
    (
        (f64::from(p.x) * 1_000.0).round() as i64,
        (f64::from(p.y) * 1_000.0).round() as i64,
    )
}

// ---------------------------------------------------------------------------
// The coastline
// ---------------------------------------------------------------------------

/// The true outline of the settled ground: every block edge with a block on one
/// side and nothing on the other, chained into rings.
///
/// This replaces the convex hull the first renders drew. The hull is why every
/// city came out as a near-regular convex polygon floating in black — an
/// *object* rather than a place — and it is the one thing the fresh-eyes review
/// said M1 threw away from the `accretion` prototype. Whatever silhouette the
/// layout produces, lobed or convex, this traces it exactly, including inlets
/// and interior voids.
///
/// Rings come out in a deterministic order (the walk starts from the smallest
/// unused quantised vertex and always takes the smallest unused neighbour), and
/// the result is a function of the block geometry alone.
#[must_use]
pub fn settlement_outline(city: &City) -> Vec<Vec<Point>> {
    let mut position: BTreeMap<Key, Point> = BTreeMap::new();
    let mut used: BTreeMap<Edge, u32> = BTreeMap::new();
    for block in &city.layout.blocks {
        let ring = &block.boundary.vertices;
        if ring.len() < 3 {
            continue;
        }
        for i in 0..ring.len() {
            let a = grid_key(ring[i]);
            let b = grid_key(ring[(i + 1) % ring.len()]);
            if a == b {
                continue;
            }
            position.insert(a, ring[i]);
            position.insert(b, ring[(i + 1) % ring.len()]);
            *used.entry((a.min(b), a.max(b))).or_insert(0) += 1;
        }
    }

    let mut adjacency: BTreeMap<Key, Vec<Key>> = BTreeMap::new();
    for ((a, b), count) in &used {
        if *count != 1 {
            continue;
        }
        adjacency.entry(*a).or_default().push(*b);
        adjacency.entry(*b).or_default().push(*a);
    }
    for neighbours in adjacency.values_mut() {
        neighbours.sort_unstable();
    }

    let mut visited: BTreeSet<Edge> = BTreeSet::new();
    let mut rings: Vec<Vec<Point>> = Vec::new();
    let starts: Vec<Key> = adjacency.keys().copied().collect();
    for start in starts {
        while let Some(first) = adjacency.get(&start).and_then(|ns| {
            ns.iter()
                .copied()
                .find(|n| !visited.contains(&edge(start, *n)))
        }) {
            let mut ring: Vec<Point> = Vec::new();
            let mut previous = start;
            let mut current = first;
            visited.insert(edge(start, first));
            if let Some(p) = position.get(&start) {
                ring.push(*p);
            }
            // The walk is bounded by the edge count: every step consumes one
            // unvisited edge, so it terminates whatever the graph looks like.
            for _ in 0..=used.len() {
                if let Some(p) = position.get(&current) {
                    ring.push(*p);
                }
                if current == start {
                    break;
                }
                let next = adjacency.get(&current).and_then(|ns| {
                    ns.iter()
                        .copied()
                        .find(|n| *n != previous && !visited.contains(&edge(current, *n)))
                        .or_else(|| {
                            ns.iter()
                                .copied()
                                .find(|n| !visited.contains(&edge(current, *n)))
                        })
                });
                let Some(next) = next else { break };
                visited.insert(edge(current, next));
                previous = current;
                current = next;
            }
            if ring.len() >= 4 && ring.first() == ring.last() {
                ring.pop();
            }
            if ring.len() >= 3 {
                rings.push(ring);
            }
        }
    }
    rings
}

fn edge(a: Key, b: Key) -> Edge {
    (a.min(b), a.max(b))
}

// ---------------------------------------------------------------------------
// Terrain wash
// ---------------------------------------------------------------------------

/// A value-noise lattice. Integer-hashed corners, polynomial fade — no
/// transcendental reaches the image (PRD §7.4).
struct Lattice {
    n: usize,
    v: Vec<f64>,
}

impl Lattice {
    fn new(n: usize, seed: u64) -> Self {
        let n = n.max(1);
        let mut v = Vec::with_capacity((n + 1) * (n + 1));
        for y in 0..=n {
            for x in 0..=n {
                let mut bytes = [0u8; 24];
                bytes[..8].copy_from_slice(&(x as u64).to_le_bytes());
                bytes[8..16].copy_from_slice(&(y as u64).to_le_bytes());
                bytes[16..].copy_from_slice(&seed.to_le_bytes());
                v.push((fnv1a64(&bytes) >> 11) as f64 / 9_007_199_254_740_992.0);
            }
        }
        Self { n, v }
    }

    /// Bilinear sample with a smoothstep fade. `u`, `v` in `[0, 1]`.
    fn at(&self, u: f64, v: f64) -> f64 {
        let n = self.n as f64;
        let fx = (u.clamp(0.0, 1.0) * n).min(n - 1e-9);
        let fy = (v.clamp(0.0, 1.0) * n).min(n - 1e-9);
        let ix = fx as usize;
        let iy = fy as usize;
        let tx = fx - ix as f64;
        let ty = fy - iy as f64;
        let sx = tx * tx * (3.0 - 2.0 * tx);
        let sy = ty * ty * (3.0 - 2.0 * ty);
        let row = self.n + 1;
        let a = self.v[iy * row + ix];
        let b = self.v[iy * row + ix + 1];
        let c = self.v[(iy + 1) * row + ix];
        let d = self.v[(iy + 1) * row + ix + 1];
        let top = a + (b - a) * sx;
        let bottom = c + (d - c) * sx;
        top + (bottom - top) * sy
    }
}

/// Fill the canvas with faint terrain rather than hard black.
///
/// > **Terrain / vacant lots** — barely visible. (PRD §10.3, layer 1)
///
/// Thirty per cent of every earlier render was one flat `#0C0D11`, which is what
/// made the settlement read as an object cut out and dropped on a background.
/// This is the ground the city sits in: a two-octave value noise at ±4 channel
/// levels around [`SEA_DEEP`], which is present, orientating, and *at the very
/// bottom of the band* — the sea has to be near zero for the land to have a
/// silhouette at all.
fn wash_terrain(canvas: &mut Canvas, seed: u64) {
    let coarse = Lattice::new(7, seed ^ 0x9E37_79B9_7F4A_7C15);
    let fine = Lattice::new(43, seed ^ 0xC2B2_AE3D_27D4_EB4F);
    let w = canvas.width;
    let h = canvas.height;
    if w == 0 || h == 0 {
        return;
    }
    let inv_w = 1.0 / w as f64;
    let inv_h = 1.0 / h as f64;
    for y in 0..h {
        let v = (y as f64 + 0.5) * inv_h;
        for x in 0..w {
            let u = (x as f64 + 0.5) * inv_w;
            let n = coarse.at(u, v) * 0.54 + fine.at(u, v) * 0.46;
            let lift = (n - 0.5) * 8.0;
            let i = (y * w + x) * 3;
            for (channel, base) in canvas.pixels[i..i + 3].iter_mut().zip(SEA_DEEP.iter()) {
                *channel = (f64::from(*base) + lift).clamp(0.0, f64::from(BASE_MAP_CEILING)) as u8;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Small geometry helpers
// ---------------------------------------------------------------------------

fn ring_centre(ring: &[Px]) -> Px {
    if ring.is_empty() {
        return [0.0, 0.0];
    }
    let mut sum = [0.0f64, 0.0f64];
    for p in ring {
        sum[0] += p[0];
        sum[1] += p[1];
    }
    let n = ring.len() as f64;
    [sum[0] / n, sum[1] / n]
}

fn ring_area(ring: &[Px]) -> f64 {
    let n = ring.len();
    if n < 3 {
        return 0.0;
    }
    let mut a = 0.0;
    for i in 0..n {
        let p = ring[i];
        let q = ring[(i + 1) % n];
        a += p[0] * q[1] - q[0] * p[1];
    }
    a.abs() * 0.5
}

fn translate(ring: &[Px], dx: f64, dy: f64) -> Vec<Px> {
    ring.iter().map(|p| [p[0] + dx, p[1] + dy]).collect()
}

fn scale_about(ring: &[Px], c: Px, f: f64) -> Vec<Px> {
    ring.iter()
        .map(|p| [c[0] + (p[0] - c[0]) * f, c[1] + (p[1] - c[1]) * f])
        .collect()
}

/// Sutherland–Hodgman against one half-plane: keep `n · p <= d`.
fn clip_half(ring: &[Px], nx: f64, ny: f64, d: f64) -> Vec<Px> {
    let n = ring.len();
    let mut out: Vec<Px> = Vec::with_capacity(n + 4);
    if n < 3 {
        return out;
    }
    for i in 0..n {
        let a = ring[i];
        let b = ring[(i + 1) % n];
        let da = nx * a[0] + ny * a[1] - d;
        let db = nx * b[0] + ny * b[1] - d;
        let inside_a = da <= 0.0;
        let inside_b = db <= 0.0;
        if inside_a {
            out.push(a);
        }
        if inside_a != inside_b {
            let t = da / (da - db);
            out.push([a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]);
        }
    }
    out
}

/// Where a building's height lands on the fixed ramp, in `[0, 1]`.
///
/// Piecewise, one segment per register — see [`SETTLED_TONE`].
#[must_use]
pub fn height_ramp(height: f32) -> f64 {
    let base = f64::from(polis_layout::buildings::BASE_HEIGHT);
    let settled = f64::from(polis_layout::buildings::SETTLED_CEILING).max(base + 1e-6);
    let top = f64::from(HEIGHT_REFERENCE).max(settled + 1e-6);
    let h = f64::from(height);
    if h <= settled {
        (((h - base) / (settled - base)) * SETTLED_TONE).clamp(0.0, SETTLED_TONE)
    } else {
        (SETTLED_TONE + ((h - settled) / (top - settled)) * (1.0 - SETTLED_TONE)).clamp(0.0, 1.0)
    }
}

// ---------------------------------------------------------------------------
// The plan
// ---------------------------------------------------------------------------

/// Draws the city plan.
///
/// `supersample` is the integer factor the image is drawn at before being box-
/// filtered down; 2 is the useful setting and 1 is for tests.
///
/// `streets` turns on PRD §9's import layer. **Off at this zoom by default**:
///
/// > Streets are a toggleable layer, off by default at the widest zoom.
///
/// This is the whole-city view, and the import graph drawn over the whole city
/// at once is the thing the first render got wrong — a mat of lines over the
/// plan that reads as noise rather than as information. The layer is drawn on
/// request (`polis snapshot --streets`), and when it is drawn it follows the
/// roads rather than cutting across the ground.
#[must_use]
pub fn render_plan(
    city: &City,
    structure: &Structure,
    title: &str,
    pixels: usize,
    supersample: usize,
    streets: bool,
) -> Canvas {
    let ss = supersample.max(1);
    let w = pixels * ss;
    let footer = w as f64 * 0.088;
    let mut canvas = Canvas::new(w, w, SEA_DEEP);
    let view = draw_map(&mut canvas, city, w, w - footer as usize, streets, ss);
    draw_labels(&mut canvas, city, &view, map_unit(city, &view), ss, pixels);
    draw_footer(&mut canvas, city, structure, title, w, footer);
    canvas.downsample(ss)
}

/// The map layers alone: no labels, no footer, no text of any kind.
///
/// This is what PRD §10.3's ceiling is asserted against, and the reason the
/// assertion is about real pixels rather than about the palette constants.
#[must_use]
pub fn render_base_map(city: &City, pixels: usize, supersample: usize, streets: bool) -> Canvas {
    let ss = supersample.max(1);
    let w = pixels * ss;
    let mut canvas = Canvas::new(w, w, SEA_DEEP);
    draw_map(&mut canvas, city, w, w, streets, ss);
    canvas.downsample(ss)
}

/// Every stroke is a fraction of the median block's diameter, so the drawing
/// reads the same at 90 files and at 5 000.
fn map_unit(city: &City, view: &View) -> f64 {
    let mut d: Vec<f64> = city
        .layout
        .blocks
        .iter()
        .map(|b| f64::from(b.boundary.area()).max(0.0).sqrt())
        .collect();
    d.sort_by(f64::total_cmp);
    view.scale() * if d.is_empty() { 1.0 } else { d[d.len() / 2] }
}

/// Layers 0–2, bottom to top. Returns the view it drew with.
///
/// `ss` is the supersample factor, and it is here for one reason: a stroke
/// narrower than an output pixel is *averaged away* by the box filter, so at
/// 5 000 files the whole road hierarchy dissolved into the ground and the base
/// map lost the brightest ink it has. Every line therefore gets a floor of
/// roughly one output pixel — [`hairline`] — rather than one supersampled one.
fn draw_map(
    canvas: &mut Canvas,
    city: &City,
    w: usize,
    map_height: usize,
    streets: bool,
    ss: usize,
) -> View {
    let hairline = ss as f64 * 0.95;
    let (lo, hi) = world_bounds(city);
    let view = View::fit(lo, hi, w, map_height, w as f64 * 0.022);
    let colours = district_colours(city);
    let unit = map_unit(city, &view);
    let industrial_hosts: BTreeSet<&LogicalPath> =
        city.industrial.iter().map(|m| &m.host).collect();

    // 0. The ground the city sits in. Faint terrain, never hard black.
    wash_terrain(canvas, city.digest());
    let mut ink = MapInk { canvas };

    // 1. The coastline, and it is drawn CRISP. Interior voids fill as land, so a
    //    hole in the settlement reads as unbuilt ground rather than as a hole
    //    punched in a disc.
    //
    //    The first version of this spread sixteen overlapping strokes over three
    //    block-widths to make the shore "soft". That blurred the one edge in the
    //    image that carries the silhouette — which is the thing the squint test
    //    actually reads — and it wrapped the island in a ring of outward Voronoi
    //    spikes into the bargain. What is drawn now is two narrow bands: a water
    //    margin and a shelf, each about a fifth of a block wide, with the sea at
    //    the very bottom of the band on one side and the settled ground well
    //    above it on the other. A weather-chart coastline is faint *and* clean.
    let outline = settlement_outline(city);
    for ring in &outline {
        let projected = view.ring(ring);
        ink.stroke_polygon(&projected, (unit * 0.44).max(hairline * 2.4), SEA_MID, 0.90);
        ink.stroke_polygon(&projected, (unit * 0.20).max(hairline * 1.5), SHELF, 0.95);
    }
    for ring in &outline {
        ink.fill_polygon(&view.ring(ring), LAND, 1.0);
    }

    // 2. Vacant lots, gone to seed (PRD §7.5, layer 1 — barely visible).
    for lot in city.layout.vacant_lots() {
        ink.fill_polygon(&view.ring(&lot.boundary.vertices), VACANT, 0.55);
    }

    // 3. The ground of each district, block by block. Districts tile: every
    //    block belongs to exactly one, so this layer has no gaps.
    for block in &city.layout.blocks {
        let ring = view.ring(&block.boundary.vertices);
        let colour = colours.get(&block.district).copied().unwrap_or(LAND);
        ink.fill_polygon(&ring, colour, 1.0);
        ink.stroke_polygon(&ring, (unit * 0.011).max(hairline * 0.6), BLOCKLINE, 0.70);
    }

    // 4. Lots: the buildable parcels, a shade lighter than the ground, so a
    //    block reads as a group of plots rather than as one painted cell.
    let block_district: BTreeMap<u32, &LogicalPath> = city
        .layout
        .blocks
        .iter()
        .map(|b| (b.id.0, &b.district))
        .collect();
    for lot in &city.layout.lots {
        let Some(district) = block_district.get(&lot.block.0) else {
            continue;
        };
        if industrial_hosts.contains(*district) {
            continue;
        }
        let ring = view.ring(&lot.boundary.vertices);
        let base = colours
            .get(*district)
            .map_or(LAND, |c| mix(*c, LOT_LIFT, 0.28));
        ink.fill_polygon(&ring, base, 1.0);
        ink.stroke_polygon(&ring, (unit * 0.010).max(hairline * 0.6), LOTLINE, 0.34);
    }

    // 5. Roads, by class, under the built mass — a building stands on the
    //    ground the street bounds, and drawing the network over the buildings
    //    is what flattened the earlier renders into a diagram.
    let (district_edges, package_edges) = borders(city);
    for (i, segment) in city.layout.roads.segments.iter().enumerate() {
        let (Some(a), Some(b)) = (
            city.layout.roads.nodes.get(segment.from.0 as usize),
            city.layout.roads.nodes.get(segment.to.0 as usize),
        ) else {
            continue;
        };
        let line = [view.at(a.position), view.at(b.position)];
        let (width, colour) = match segment.class {
            RoadClass::Arterial => (unit * 0.090, ROAD_HI),
            RoadClass::Street => (unit * 0.052, ROAD_MID),
            RoadClass::Alley => (unit * 0.030, ROAD_LO),
        };
        ink.polyline(&line, width.max(hairline), colour, 0.96);
        // The package border is the wayfinding skeleton (PRD §8), and a line is
        // the cheapest possible way to carry it: it costs area, not range, so
        // strengthening it makes the districts readable at thumbnail size
        // without lifting the base map's mean luminance at all.
        if package_edges.contains(&i) {
            ink.polyline(&line, (unit * 0.030).max(hairline * 1.4), BORDER, 0.95);
        } else if district_edges.contains(&i) {
            ink.polyline(&line, (unit * 0.013).max(hairline * 0.9), BORDER, 0.30);
        }
    }

    // 6. Buildings, in two passes: every shadow, then every roof. One pass
    //    would let a neighbour's shadow fall across a roof already drawn.
    let lot_block: BTreeMap<u32, u32> = city
        .layout
        .lots
        .iter()
        .map(|l| (l.id.0, l.block.0))
        .collect();
    let monuments: BTreeSet<&LogicalPath> = city.monuments.iter().map(|m| &m.path).collect();
    let mut drawn: Vec<Massing<'_>> = Vec::new();
    for building in city.layout.buildings.values() {
        let ground = lot_block
            .get(&building.lot.0)
            .and_then(|b| block_district.get(b));
        if let Some(d) = ground {
            if industrial_hosts.contains(*d) {
                continue;
            }
        }
        let ring = view.ring(&building.footprint.vertices);
        if ring.len() < 3 {
            continue;
        }
        let centre = ring_centre(&ring);
        let span = ring_area(&ring).max(0.0).sqrt().max(1.0);
        let t = height_ramp(building.height);
        let ground_colour = ground
            .and_then(|d| colours.get(*d))
            .copied()
            .unwrap_or(LAND);
        drawn.push((building, ring, centre, span, t, ground_colour));
    }

    // Shadow length is a fraction of the building's own footprint, so it holds
    // its proportion at 90 files and at 5 000, and it is the channel that
    // carries height at thumbnail size: at 5 000 files a roof is four pixels
    // across and its tone survives the box filter, while at 90 files the shadow
    // is what says "tower" from across the room. Both are drawn because the two
    // scales need different ones.
    for (_, ring, _, span, t, ground_colour) in &drawn {
        let reach = span * (0.10 + 0.80 * t);
        let dx = SUN[0] * reach;
        let dy = SUN[1] * reach;
        // Opaque, so overlapping shadows do not compound into a black hole; the
        // tint comes from the ground it falls on rather than from a fixed grey.
        let shade_colour = mix(*ground_colour, SHADOW, 0.76);
        let moved = translate(ring, dx, dy);
        for i in 0..ring.len() {
            let a = ring[i];
            let b = ring[(i + 1) % ring.len()];
            // A parallelogram by construction, so it can never fold into a
            // bowtie whatever the edge direction is.
            ink.fill_polygon(
                &[a, b, [b[0] + dx, b[1] + dy], [a[0] + dx, a[1] + dy]],
                shade_colour,
                1.0,
            );
        }
        ink.fill_polygon(&moved, shade_colour, 1.0);
    }

    for (building, ring, centre, span, t, _) in &drawn {
        let monument = monuments.contains(&building.path);
        let roof = if monument {
            MONU
        } else {
            mix(ROOF_LO, ROOF_HI, *t)
        };
        if monument {
            // A monument is an orientation anchor, so it gets a silhouette no
            // ordinary building has: a plinth wider than the building, the
            // building itself in brass, and a diamond finial that survives the
            // downsample to thumbnail size (PRD §8).
            ink.fill_polygon(&scale_about(ring, *centre, 1.34), MONU_DIM, 1.0);
        }
        ink.fill_polygon(ring, roof, 1.0);
        draw_roof_form(&mut ink, ring, *centre, roof, building.roof);
        draw_massing(&mut ink, ring, *centre, roof, *t, unit, hairline);
        // Keyline weight is the third height channel: it adds ink rather than
        // brightness, so it is still there after the box filter.
        ink.stroke_polygon(ring, (unit * 0.007).max(hairline * 0.5), ROOF_EDGE, 0.55);
        if monument {
            let r = span * 0.34;
            ink.fill_polygon(
                &[
                    [centre[0], centre[1] - r],
                    [centre[0] + r * 0.62, centre[1]],
                    [centre[0], centre[1] + r],
                    [centre[0] - r * 0.62, centre[1]],
                ],
                ROOF_HI,
                0.95,
            );
            ink.stroke_polygon(ring, (unit * 0.016).max(1.0), MONU, 0.9);
        }
    }

    // 7. Industrial masses (PRD §8): one dull mass each, never individual
    //    buildings — and *mass*, not absence. Drawn flat and slightly darker
    //    than the land, the zone read as a hole punched in the town, which is
    //    the opposite of "the eye slides off it": a hole is the first thing the
    //    eye finds. So it is filled at the settled ground's own luminance and
    //    then hatched at one spacing and one angle. A uniform hatch carries no
    //    information, which is exactly what §8 asks a vendored tree to carry,
    //    and it makes the area read as occupied.
    for block in &city.layout.blocks {
        if industrial_hosts.contains(&block.district) {
            let ring = view.ring(&block.boundary.vertices);
            ink.fill_polygon(&ring, INDUS, 1.0);
            // The hatch has a floor in *output* pixels: a stripe finer than the
            // box filter averages back into a flat plate, which is the failure
            // this is here to fix.
            hatch(
                &mut ink,
                &ring,
                (unit * 0.115).max(hairline * 7.0),
                INDUS_HATCH,
                0.60,
            );
            ink.stroke_polygon(
                &ring,
                (unit * 0.010).max(hairline * 0.6),
                mix(INDUS, INDUS_HATCH, 0.55),
                0.7,
            );
        }
    }

    // 8. The drawn coastline itself, over the ground it encloses: one stroke,
    //    near the top of the band, straddling the boundary so half of it lands
    //    on the shelf and half on the land. This single line is what gives the
    //    thumbnail a silhouette.
    for ring in &outline {
        ink.stroke_polygon(
            &view.ring(ring),
            (unit * 0.032).max(hairline * 1.6),
            LIMIT,
            0.94,
        );
    }

    // 9. Streets: cross-district imports routed along the roads (PRD §9). A
    //    toggleable layer, off at this zoom unless asked for, and drawn along
    //    the road network rather than across open ground. Width is the number of
    //    distinct import edges the relation carries, which is what PRD §9 asks
    //    the width to mean.
    if streets {
        let mut lines: Vec<_> = city.layout.streets.iter().collect();
        lines.sort_by_key(|s| {
            (
                std::cmp::Reverse(s.edge_count),
                s.from.clone(),
                s.to.clone(),
            )
        });
        lines.truncate((city.layout.blocks.len() / 9).clamp(6, 60));
        for street in lines {
            let line = view.ring(&street.polyline);
            let width = (unit * (0.010 + 0.004 * f64::from(street.edge_count.min(8)))).max(0.9);
            ink.polyline(&line, width, STREET, 0.60);
        }
    }
    view
}

/// The signed area of a ring in device space. Sign is the winding.
fn signed_area(ring: &[Px]) -> f64 {
    let n = ring.len();
    if n < 3 {
        return 0.0;
    }
    let mut a = 0.0;
    for i in 0..n {
        let p = ring[i];
        let q = ring[(i + 1) % n];
        a += p[0].mul_add(q[1], -(q[0] * p[1]));
    }
    a * 0.5
}

/// Massing: one lit side and one shaded side, both scaling with height.
///
/// This replaces the concentric set-backs the first render drew. Those made a
/// tall building into a **bullseye** — a nest of rectangles that reads as a
/// target or a picture frame, and that forces the eye to *count rings* to
/// compare two heights. It was also a second mark competing with PRD §7.3's
/// stepped roof form for the same notation.
///
/// What is drawn instead is the oldest flat-camera height cue there is: the
/// faces turned toward the light are brighter, the faces turned away are
/// darker, and the difference grows with height. There is no tilt and no
/// projection in it — nothing moves off its footprint, so PRD §12's orthographic
/// camera is untouched — and it is *monotone*, so two neighbours can be ranked
/// at a glance instead of counted.
fn draw_massing(
    ink: &mut MapInk<'_>,
    ring: &[Px],
    centre: Px,
    roof: Rgb,
    t: f64,
    unit: f64,
    hairline: f64,
) {
    if ring.len() < 3 {
        return;
    }
    // The half of the roof turned away from the sun, darkened in proportion to
    // height. `d` is the sun-axis coordinate of the centre, so the cut passes
    // through the middle of the building whatever shape it is.
    let d = SUN[0].mul_add(centre[0], SUN[1] * centre[1]);
    // Brighten the near half and darken the far half by the same amount, so
    // massing costs the map contrast rather than luminance: a building's mean
    // tone still says how tall it is, and its two halves say which way the light
    // is coming from.
    // `clip_half` keeps `n · p <= d`, so this half is the one the shadow runs
    // *away* from: the lit side.
    let lit = clip_half(ring, SUN[0], SUN[1], d);
    if lit.len() >= 3 {
        ink.fill_polygon(&lit, mix(roof, ROOF_TOP, 0.14 + 0.42 * t), 0.92);
    }
    let shaded = clip_half(ring, -SUN[0], -SUN[1], -d);
    if shaded.len() >= 3 {
        ink.fill_polygon(&shaded, mix(roof, ROOF_EDGE, 0.10 + 0.26 * t), 0.92);
    }
    // The lit rim: only the edges whose outward normal faces into the light, so
    // it is a highlight on one side of the building and never a ring around it.
    let outward = if signed_area(ring) >= 0.0 { 1.0 } else { -1.0 };
    let width = (unit * (0.012 + 0.085 * t)).max(hairline);
    let alpha = 0.40 + 0.55 * t;
    for i in 0..ring.len() {
        let a = ring[i];
        let b = ring[(i + 1) % ring.len()];
        let (ex, ey) = (b[0] - a[0], b[1] - a[1]);
        // Outward normal of a directed edge, sign fixed by the winding.
        let (nx, ny) = (outward * ey, -outward * ex);
        if nx.mul_add(-SUN[0], ny * -SUN[1]) <= 0.0 {
            continue;
        }
        ink.polyline(&[a, b], width, ROOF_TOP, alpha);
    }
}

/// Hatch a ring with parallel stripes at one spacing and one angle.
///
/// Each stripe is the ring clipped between two parallel half-planes, so the
/// hatch is exactly bounded by the shape and needs no mask. The stripe count is
/// capped: a degenerate ring must not turn into an unbounded loop.
fn hatch(ink: &mut MapInk<'_>, ring: &[Px], spacing: f64, colour: Rgb, alpha: f64) {
    if ring.len() < 3 || spacing <= 0.0 {
        return;
    }
    let axis = |p: Px| SUN[0].mul_add(p[0], SUN[1] * p[1]);
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for p in ring {
        lo = lo.min(axis(*p));
        hi = hi.max(axis(*p));
    }
    if !lo.is_finite() || !hi.is_finite() {
        return;
    }
    let half = spacing * 0.19;
    let steps = ((hi - lo) / spacing).ceil().clamp(0.0, 512.0) as u32;
    // The stripes are keyed to the world origin rather than to this ring's own
    // bounding box, so two adjacent industrial blocks share one hatch instead of
    // meeting at a seam.
    let first = (lo / spacing).floor();
    for k in 0..=steps {
        let d = (first + f64::from(k)) * spacing;
        let band = clip_half(ring, SUN[0], SUN[1], d + half);
        if band.len() < 3 {
            continue;
        }
        let band = clip_half(&band, -SUN[0], -SUN[1], -(d - half));
        if band.len() >= 3 {
            ink.fill_polygon(&band, colour, alpha);
        }
    }
}

/// Roof form (PRD §7.3's silhouette variety), as facets.
///
/// This is where a building gets its second and third tone. A flat roof keeps
/// one; a stepped roof gains a set-back wing; a pitched roof is split along a
/// ridge.
///
/// Every cut here is made on [`RIDGE`] — the axis **across** the sun — while
/// [`draw_massing`] cuts along it. Roof form is a hash of the path and height is
/// the quantity the map exists to show, so the two must not share a mark: on
/// perpendicular axes they compose into one legible little building instead of
/// fighting over the same shading.
fn draw_roof_form(ink: &mut MapInk<'_>, ring: &[Px], centre: Px, roof: Rgb, form: RoofForm) {
    let d = RIDGE[0].mul_add(centre[0], RIDGE[1] * centre[1]);
    match form {
        // A parapet: a thin inner line, deliberately *not* a filled inset.
        RoofForm::Flat => {
            let inner = scale_about(ring, centre, 0.86);
            if inner.len() >= 3 {
                ink.stroke_polygon(&inner, 0.8, mix(roof, ROOF_TOP, 0.5), 0.30);
            }
        }
        // A set-back wing on one side of the ridge: asymmetric, so it cannot be
        // mistaken for the massing.
        RoofForm::Stepped => {
            let wing = clip_half(ring, -RIDGE[0], -RIDGE[1], -d);
            if wing.len() >= 3 {
                ink.fill_polygon(
                    &scale_about(&wing, centre, 0.94),
                    mix(roof, ROOF_EDGE, 0.26),
                    0.70,
                );
            }
        }
        RoofForm::Pitched => {
            let near = clip_half(ring, RIDGE[0], RIDGE[1], d);
            let far = clip_half(ring, -RIDGE[0], -RIDGE[1], -d);
            if near.len() >= 3 {
                ink.fill_polygon(&near, mix(roof, ROOF_HI, 0.34), 0.70);
            }
            if far.len() >= 3 {
                ink.fill_polygon(&far, mix(roof, ROOF_EDGE, 0.24), 0.70);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Labels (the overlay — PRD §8, §13)
// ---------------------------------------------------------------------------

/// What a label names. Lower is more important, and importance is what survives
/// the declutterer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum LabelKind {
    /// PRD §8's orientation anchors, "always labelled at every zoom".
    Monument,
    /// A top-level package: the district skeleton.
    Package,
    /// An ordinary quarter.
    Quarter,
}

/// The shortest trailing run of path components that is not already on the map.
///
/// Six buildings called `lib.rs` labelled `SRC/LIB.RS` six times is the same
/// failure as labelling none of them: the label has to say *which* one. So the
/// name grows a component at a time until it is unique among the labels already
/// placed, and only falls back to the full path when even that is not enough.
fn shortest_unique_name(full: &str, placed: &BTreeSet<String>) -> String {
    if full.is_empty() {
        return "ROOT".to_owned();
    }
    let parts: Vec<&str> = full.split('/').collect();
    for take in 1..=parts.len() {
        let name = parts[parts.len() - take..].join("/");
        if !placed.contains(&name) {
            return name;
        }
    }
    full.to_owned()
}

struct Candidate<'a> {
    kind: LabelKind,
    rank: u64,
    at: Px,
    path: &'a LogicalPath,
}

/// Name the monuments and the largest quarters, decluttered.
///
/// > District boundaries, monuments, and the skyline profile are the wayfinding
/// > layer and should survive when everything else is decluttered away. (PRD §8)
///
/// Two thousand districts cannot all carry a label, so the declutterer is the
/// mechanism that makes PRD §8's "always labelled" and "stays readable"
/// compatible: monuments are placed first, the package skeleton second, and
/// ordinary quarters fill whatever room is left. What is dropped is always the
/// least structural thing on the map.
fn draw_labels(canvas: &mut Canvas, city: &City, view: &View, unit: f64, ss: usize, pixels: usize) {
    let block_by_id: BTreeMap<u32, &polis_layout::Block> =
        city.layout.blocks.iter().map(|b| (b.id.0, b)).collect();

    let mut candidates: Vec<Candidate<'_>> = Vec::new();
    for monument in &city.monuments {
        let Some(building) = city.layout.buildings.get(&monument.path) else {
            continue;
        };
        let at = view.at(building.footprint.centroid());
        candidates.push(Candidate {
            kind: LabelKind::Monument,
            rank: u64::from(monument.rank),
            at,
            path: &monument.path,
        });
    }
    for (path, district) in &city.layout.districts {
        let mut area = 0.0f64;
        let mut sum = [0.0f64, 0.0f64];
        for id in &district.blocks {
            if let Some(block) = block_by_id.get(&id.0) {
                let a = f64::from(block.boundary.area());
                let c = block.boundary.centroid();
                area += a;
                sum[0] += f64::from(c.x) * a;
                sum[1] += f64::from(c.y) * a;
            }
        }
        if area <= 0.0 || district.blocks.is_empty() {
            continue;
        }
        let centre = Point::new((sum[0] / area) as f32, (sum[1] / area) as f32);
        let package = path.components().count() <= 1;
        candidates.push(Candidate {
            kind: if package {
                LabelKind::Package
            } else {
                LabelKind::Quarter
            },
            // Largest first, as an integer key so the order cannot depend on a
            // float comparison that rounds differently at a stage boundary.
            rank: u64::MAX - (area * 1_000.0).clamp(0.0, 9e18) as u64,
            at: view.at(centre),
            path,
        });
    }
    candidates.sort_by(|a, b| {
        a.kind
            .cmp(&b.kind)
            .then_with(|| a.rank.cmp(&b.rank))
            .then_with(|| a.path.cmp(b.path))
    });

    // The cap is what keeps a 2 000-district repository from becoming a wall of
    // text; the collision test is what keeps the survivors readable. Monuments
    // get a sub-cap of their own rather than the whole budget, because a map
    // whose every label is a file name has lost the district skeleton PRD §8
    // asks to preserve.
    let cap = (pixels / 62).clamp(8, 28);
    let monument_cap = (cap / 4).max(3);
    let mut monuments_placed = 0usize;
    let size = (unit * 0.038).clamp(2.0 * ss as f64, 3.2 * ss as f64);
    let mut taken: Vec<(Px, f64, f64)> = Vec::new();
    let mut placed: BTreeSet<String> = BTreeSet::new();
    for candidate in candidates {
        if taken.len() >= cap {
            break;
        }
        if candidate.kind == LabelKind::Monument && monuments_placed >= monument_cap {
            continue;
        }
        let name = shortest_unique_name(candidate.path.as_str(), &placed);
        let at = candidate.at;
        let tw = Canvas::text_width(&name, size);
        let th = 10.0 * ss as f64;
        // The plate is wider and taller than the glyphs; the keep-out has to
        // clear the plate, or two labels touch and both become unreadable.
        if taken.iter().any(|(o, ow, oh)| {
            (o[0] - at[0]).abs() < (ow + tw) * 0.5 + 8.0 * ss as f64
                && (o[1] - at[1]).abs() < (oh + th) * 1.05
        }) {
            continue;
        }
        placed.insert(name.clone());
        if candidate.kind == LabelKind::Monument {
            monuments_placed += 1;
        }
        taken.push((at, tw, th));
        canvas.rect(
            at[0] - tw * 0.5 - 3.0 * ss as f64,
            at[1] - 6.0 * ss as f64,
            at[0] + tw * 0.5 + 3.0 * ss as f64,
            at[1] + 9.0 * ss as f64,
            PLATE,
            0.72,
        );
        let colour = if candidate.kind == LabelKind::Monument {
            LABEL_MONU
        } else {
            LABEL
        };
        canvas.text(
            at[0] - tw * 0.5,
            at[1] - 3.0 * ss as f64,
            &name,
            size,
            colour,
        );
    }
}

/// The tallest building in the city, as a caption.
///
/// PRD §7.3: "the tallest thing on the map is the biggest unreviewed pile". The
/// caption is how an operator checks the picture against the data, so the tie is
/// broken on the path and the answer is a function of the layout alone.
fn tallest_caption(city: &City) -> String {
    city.layout
        .buildings
        .values()
        .max_by(|a, b| {
            a.height
                .total_cmp(&b.height)
                .then_with(|| b.path.cmp(&a.path))
        })
        .map_or_else(
            || "NONE".to_owned(),
            |b| format!("{} H={:.1}", b.path.as_str(), b.height),
        )
}

/// Draw a line of text that is guaranteed to end before `right`.
///
/// The footer used to be drawn at a fixed size with a comment asking future
/// authors to keep every line under 130 characters. That is not a mechanism, and
/// it failed the moment a repository had a long path in it: two consecutive
/// reviews could only read `TALLEST DJANGO/__MAIN__.PY H=` because the value was
/// off the edge of the canvas. A caption that names the single most important
/// building on the map is not a place to lose the last characters, so the size
/// is now solved for rather than hoped for, and only if the text still will not
/// fit does it lose its tail — with an ellipsis, so the loss is visible.
fn fitted_text(canvas: &mut Canvas, x: f64, y: f64, right: f64, text: &str, size: f64, ink: Rgb) {
    let room = (right - x).max(0.0);
    if room <= 0.0 || text.is_empty() {
        return;
    }
    let want = Canvas::text_width(text, size);
    if want <= room {
        canvas.text(x, y, text, size, ink);
        return;
    }
    // Shrink, but only so far: below two thirds the bitmap font stops being a
    // font. Past that, cut characters and say so.
    let shrunk = (size * room / want).max(size * 0.66);
    if Canvas::text_width(text, shrunk) <= room {
        canvas.text(x, y, text, shrunk, ink);
        return;
    }
    let fits = (room / (6.0 * shrunk)).floor().max(0.0) as usize;
    let keep = fits.saturating_sub(3);
    let cut: String = text.chars().take(keep).collect();
    canvas.text(x, y, &format!("{cut}..."), shrunk, ink);
}

/// The legend and the metrics footer.
///
/// Everything here is laid out against the canvas's right edge rather than
/// against a constant, so nothing is clipped at any canvas size.
fn draw_footer(
    canvas: &mut Canvas,
    city: &City,
    structure: &Structure,
    title: &str,
    w: usize,
    height: f64,
) {
    // Every measurement below is in *footer units*: a fixed grid 128 tall,
    // scaled to whatever strip the caller reserved. The footer used to be laid
    // out in supersample units while its height came from a fraction of the
    // canvas, so the two agreed only at the one canvas size it was tuned on, and
    // the text ran off the bottom of every smaller one.
    let u = height / 128.0;
    let y0 = w as f64 - height;
    canvas.rect(0.0, y0, w as f64, w as f64, PLATE, 0.96);
    canvas.rect(0.0, y0, w as f64, y0 + 2.0 * u, [46, 50, 58], 1.0);
    let s = u;
    let fs = 2.0 * u;
    let pad = 14.0 * u;
    let right = w as f64 - pad;
    fitted_text(canvas, pad, y0 + 6.0 * u, right, title, fs * 1.45, TITLE);

    // The height key comes first and is drawn as a ramp rather than as two
    // swatches, because two swatches is what the last review could not tell
    // apart: it is a strip of the actual roof ramp with the actual cast shadow
    // under its tall end, so the legend teaches the encoding instead of naming
    // it.
    let ly = y0 + 32.0 * u;
    let key_h = 11.0 * s;
    let key_w = 62.0 * s;
    let mut x = pad;
    canvas.text(x, ly + 2.0 * s, "HEIGHT", fs * 0.95, FOOTER_TEXT);
    x += Canvas::text_width("HEIGHT ", fs * 0.95);
    canvas.text(x, ly + 2.0 * s, "LOW", fs * 0.95, FOOTER_TEXT);
    x += Canvas::text_width("LOW ", fs * 0.95);
    let ramp_x = x;
    for step in 0..33 {
        let t = f64::from(step) / 32.0;
        let cx = ramp_x + key_w * t;
        canvas.rect(
            cx,
            ly,
            cx + key_w / 32.0 + 1.0,
            ly + key_h,
            mix(ROOF_LO, ROOF_HI, t),
            1.0,
        );
    }
    // The cast shadow under the ramp is the *same* encoding the map draws, so
    // the key shows both channels at once: the strip gets lighter and its
    // shadow gets longer.
    canvas.rect(
        ramp_x,
        ly + key_h,
        ramp_x + key_w * 0.12,
        ly + key_h + 1.5 * s,
        SHADOW,
        1.0,
    );
    canvas.rect(
        ramp_x + key_w * 0.60,
        ly + key_h,
        ramp_x + key_w,
        ly + key_h + 4.5 * s,
        SHADOW,
        1.0,
    );
    x = ramp_x + key_w + 4.0 * s;
    canvas.text(x, ly + 2.0 * s, "TALL", fs * 0.95, FOOTER_TEXT);
    x += Canvas::text_width("TALL", fs * 0.95) + 16.0 * s;

    let swatches = [
        ("TERRAIN", SEA_DEEP),
        ("COAST", SHELF),
        ("DISTRICT", ground_tint(0.58, 0.0)),
        ("LOT", mix(ground_tint(0.58, 0.0), LOT_LIFT, 0.35)),
        ("MONUMENT", MONU),
        ("INDUSTRIAL", INDUS_HATCH),
        ("ARTERIAL", ROAD_HI),
        ("ALLEY", ROAD_LO),
        ("BORDER", BORDER),
    ];
    for (name, colour) in swatches {
        let entry = 15.0f64.mul_add(s, Canvas::text_width(name, fs * 0.95)) + 14.0 * s;
        if x + entry > right {
            break;
        }
        canvas.rect(x, ly, x + 11.0 * s, ly + 11.0 * s, colour, 1.0);
        canvas.text(x + 15.0 * s, ly + 2.0 * s, name, fs * 0.95, FOOTER_TEXT);
        x += entry;
    }

    let r = &city.report;
    let line1 = format!(
        "ROADS V={} E={} COMP={} CYCLES={} CROSSINGS={} DEG4+={} DANGLING={} STROKE={}% OF DIAMETER",
        r.road_nodes,
        r.road_segments,
        r.components,
        r.cycles,
        r.crossings,
        r.complex_junctions,
        r.dangling,
        (structure.longest_stroke * 100.0).round() as i64
    );
    let line2 = format!(
        "BLOCKS={} SLIVERS={} P95/P05={}X AGE={}X LOTS={} BUILT={}/{} COVER={}% ON-ROAD={} OFF-LOT={} DISTRICTS={} SPLIT={}",
        r.blocks,
        r.slivers,
        (structure.block_hierarchy * 10.0).round() as i64 / 10,
        (structure.age_gradient * 10.0).round() as i64 / 10,
        r.lots,
        r.buildings,
        r.files - r.massed,
        (structure.coverage * 100.0).round() as i64,
        structure.buildings_on_road,
        structure.buildings_outside_lot,
        r.districts,
        r.fragmented_districts
    );
    let line3 = format!(
        "PRD 10.3 BASE MAP <= CHANNEL {} (L* 20) / 3 CLOUDS {}-{} / 4 AGENTS {}-{} / 5 ATTENTION {}-{} RESERVED",
        BASE_MAP_CEILING,
        CLOUD_BAND.0,
        CLOUD_BAND.1,
        AGENT_BAND.0,
        AGENT_BAND.1,
        ATTENTION_BAND.0,
        ATTENTION_BAND.1,
    );
    // The tallest building gets a line of its own. It is PRD §7.3's headline
    // fact about the repository and it was the caption being cut in half.
    let line4 = format!("TALLEST {}", tallest_caption(city));
    for (i, line) in [line1, line2, line3, line4].iter().enumerate() {
        fitted_text(
            canvas,
            pad,
            16.0f64.mul_add(i as f64, 52.0).mul_add(u, y0),
            right,
            line,
            fs * 0.88,
            if i == 3 { LABEL_MONU } else { FOOTER_TEXT },
        );
    }
}

/// Render the road graph alone, junctions coloured by degree.
///
/// The "is it a tree?" test, and the render the design bake-off was most
/// confident about: a network with cycles and no dangling ends is immediately,
/// unarguably not a tree.
///
/// This one is a **diagnostic**, not the map, so it is deliberately outside PRD
/// §10.3's budget: its whole job is to make one structural property unmissable,
/// and nothing will ever be composited on top of it.
#[must_use]
pub fn render_junctions(
    city: &City,
    structure: &Structure,
    title: &str,
    pixels: usize,
    supersample: usize,
) -> Canvas {
    let ss = supersample.max(1);
    let w = pixels * ss;
    let mut canvas = Canvas::new(w, w, [10, 11, 14]);
    let nodes = &city.layout.roads.nodes;
    if nodes.is_empty() {
        return canvas.downsample(ss);
    }
    let mut lo = [f64::INFINITY, f64::INFINITY];
    let mut hi = [f64::NEG_INFINITY, f64::NEG_INFINITY];
    for n in nodes {
        lo[0] = lo[0].min(f64::from(n.position.x));
        lo[1] = lo[1].min(f64::from(n.position.y));
        hi[0] = hi[0].max(f64::from(n.position.x));
        hi[1] = hi[1].max(f64::from(n.position.y));
    }
    let footer = w as f64 * 0.075;
    let view = View::fit(lo, hi, w, w - footer as usize, w as f64 * 0.025);

    let mut degree = vec![0usize; nodes.len()];
    let mut lengths: Vec<f64> = Vec::with_capacity(city.layout.roads.segments.len());
    for s in &city.layout.roads.segments {
        if let (Some(a), Some(b)) = (nodes.get(s.from.0 as usize), nodes.get(s.to.0 as usize)) {
            degree[s.from.0 as usize] += 1;
            degree[s.to.0 as usize] += 1;
            lengths.push(f64::from(a.position.distance(b.position)) * view.scale());
        }
    }
    lengths.sort_by(f64::total_cmp);
    let unit = if lengths.is_empty() {
        8.0
    } else {
        lengths[lengths.len() / 2]
    };

    for s in &city.layout.roads.segments {
        if let (Some(a), Some(b)) = (nodes.get(s.from.0 as usize), nodes.get(s.to.0 as usize)) {
            canvas.polyline(
                &[view.at(a.position), view.at(b.position)],
                (unit * 0.055).max(1.0),
                [124, 134, 152],
                0.95,
            );
        }
    }
    let palette = |d: usize| -> (Rgb, f64) {
        match d.min(5) {
            0 | 1 => ([255, 70, 70], unit * 0.20),
            2 => ([70, 100, 190], unit * 0.065),
            3 => ([70, 200, 170], unit * 0.105),
            4 => ([255, 205, 60], unit * 0.155),
            _ => ([255, 110, 235], unit * 0.195),
        }
    };
    // Low degrees first, so the rare high-degree junctions sit on top.
    let mut order: Vec<(usize, usize)> = degree.iter().copied().zip(0..nodes.len()).collect();
    order.sort_unstable();
    for (d, i) in order {
        let (colour, radius) = palette(d);
        canvas.disc(view.at(nodes[i].position), radius.max(1.1), colour, 1.0);
    }

    let y0 = w as f64 - footer;
    canvas.rect(0.0, y0, w as f64, w as f64, [7, 8, 10], 0.96);
    canvas.rect(0.0, y0, w as f64, y0 + 2.0 * ss as f64, [70, 74, 84], 1.0);
    let fs = 2.0 * ss as f64;
    let pad = 14.0 * ss as f64;
    canvas.text(pad, y0 + 9.0 * ss as f64, title, fs * 1.5, [240, 240, 240]);
    let mut histogram = BTreeMap::new();
    for d in &degree {
        *histogram.entry((*d).min(5)).or_insert(0usize) += 1;
    }
    let mut x = pad;
    let ly = y0 + 32.0 * ss as f64;
    for d in 1..=5usize {
        let count = histogram.get(&d).copied().unwrap_or(0);
        let label = match d {
            1 => format!("DEG 1 (DANGLING) : {count}"),
            2 => format!("DEG 2 (BEND) : {count}"),
            5 => format!("DEG 5+ : {count}"),
            _ => format!("DEG {d} : {count}"),
        };
        let (colour, _) = palette(d);
        canvas.disc(
            [x + 6.0 * ss as f64, ly + 6.0 * ss as f64],
            6.0 * ss as f64,
            colour,
            1.0,
        );
        canvas.text(
            x + 17.0 * ss as f64,
            ly + 2.0 * ss as f64,
            &label,
            fs * 0.95,
            [205, 208, 214],
        );
        x += 17.0 * ss as f64 + Canvas::text_width(&label, fs * 0.95) + 20.0 * ss as f64;
    }
    let r = &city.report;
    let junctions = r.junctions.max(1);
    let line = format!(
        "V={} E={} COMP={} CYCLES(E-V+C)={} CROSSINGS WITHOUT A NODE={} DEG4+ SHARE={}% STROKE={}% (A TREE HAS CYCLES=0)",
        r.road_nodes,
        r.road_segments,
        r.components,
        r.cycles,
        r.crossings,
        (100.0 * r.complex_junctions as f64 / junctions as f64).round() as i64,
        (structure.longest_stroke * 100.0).round() as i64
    );
    canvas.text(
        pad,
        y0 + 54.0 * ss as f64,
        &line,
        fs * 0.95,
        [168, 174, 186],
    );
    canvas.downsample(ss)
}

// ---------------------------------------------------------------------------
// The band validation render (PRD §10.3, settled with an image)
// ---------------------------------------------------------------------------

/// Cloud iso-band tones (PRD §10.4, layer 3), low to high, inside
/// [`CLOUD_BAND`].
const CLOUD_TONES: [Rgb; 3] = [[54, 62, 74], [70, 80, 92], [86, 92, 96]];
/// A worker whose last operation is still pending (layer 4, [`AGENT_BAND`]).
const AGENT_PENDING: Rgb = [132, 140, 156];
/// A worker whose last operation succeeded.
const AGENT_DONE: Rgb = [96, 168, 150];
/// A worker whose last operation failed.
const AGENT_FAILED: Rgb = [168, 98, 92];
/// The tether tying a worker to its thread.
const AGENT_TETHER: Rgb = [100, 110, 130];
/// A thread's trail (PRD §12).
const AGENT_TRAIL: Rgb = [110, 122, 144];
/// Needs-decision (PRD §11.2a). Layer 5 owns the top of the range.
const ATTN_DECISION: Rgb = [255, 188, 62];
/// Done, unverified (PRD §11.2b).
const ATTN_DONE: Rgb = [118, 236, 206];
/// Contention (PRD §11.2c) — a relation, drawn as a link.
const ATTN_CONTENTION: Rgb = [255, 92, 78];

/// Which of PRD §10.1's glyphs a mark uses.
#[derive(Debug, Clone, Copy)]
enum Glyph {
    /// Hollow circle.
    Read,
    /// Circle with a bar through.
    Edit,
    /// Filled square.
    Write,
    /// Filled triangle.
    Run,
    /// Concentric circles.
    Verify,
}

/// A 16-gon on the unit circle, from a constant table: no trigonometry reaches
/// the image (PRD §7.4).
const CIRCLE: [[f64; 2]; 16] = [
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

/// Blend `target` over the pixel at `(x, y)`, then hold the result at `floor`.
///
/// The floor is what makes a *band* a band. A translucent wash over a dark base
/// map lands wherever the base map happens to be, which is exactly how a layer
/// that is meant to own channels 49–96 ends up drawing at 40. Clamping up to the
/// band's low end keeps the layer inside its allocation while leaving the
/// texture above the floor intact, so the city still reads through the cloud.
fn wash_pixel(canvas: &mut Canvas, x: usize, y: usize, target: Rgb, alpha: f64, floor: u8) {
    if x >= canvas.width || y >= canvas.height {
        return;
    }
    let i = (y * canvas.width + x) * 3;
    let mut lifted = false;
    for (channel, src) in canvas.pixels[i..i + 3].iter_mut().zip(target.iter()) {
        let dst = f64::from(*channel);
        let v = (dst + (f64::from(*src) - dst) * alpha)
            .round()
            .clamp(0.0, 255.0) as u8;
        *channel = v;
        lifted |= v >= floor;
    }
    if !lifted {
        // Lift the whole pixel rather than one channel, so the band keeps its
        // hue and the texture underneath keeps its shape.
        let head = canvas.pixels[i..i + 3].iter().copied().max().unwrap_or(0);
        let gain = f64::from(floor) - f64::from(head);
        for channel in &mut canvas.pixels[i..i + 3] {
            *channel = (f64::from(*channel) + gain).clamp(0.0, 255.0) as u8;
        }
    }
}

/// A quartic kernel with compact support: `(1 - r²)²` inside the radius.
///
/// PRD §10.4 splats Gaussians; a Gaussian needs `exp`, and nothing in this file
/// reaches for a transcendental (PRD §7.4). The quartic has the same bell shape,
/// has *finite* support — which makes the splat cheaper, not dearer — and is a
/// polynomial, so the same bytes come out on every libm.
fn kernel(r2: f64) -> f64 {
    if r2 >= 1.0 {
        return 0.0;
    }
    let k = 1.0 - r2;
    k * k
}

/// Stroke a circle of `radius` about `at`.
fn stroke_circle(canvas: &mut Canvas, at: Px, radius: f64, width: f64, colour: Rgb) {
    let pts: Vec<Px> = CIRCLE
        .iter()
        .map(|c| [c[0].mul_add(radius, at[0]), c[1].mul_add(radius, at[1])])
        .collect();
    canvas.stroke_polygon(&pts, width, colour, 1.0);
}

/// Draw one of PRD §10.1's operation glyphs.
///
/// Shape encodes *what*, colour encodes *how it went*, and the two are never
/// conflated (PRD §10).
fn draw_glyph(canvas: &mut Canvas, at: Px, r: f64, glyph: Glyph, colour: Rgb) {
    match glyph {
        Glyph::Read => stroke_circle(canvas, at, r, r * 0.42, colour),
        Glyph::Edit => {
            stroke_circle(canvas, at, r, r * 0.42, colour);
            canvas.segment(
                [at[0] - r, at[1]],
                [at[0] + r, at[1]],
                r * 0.42,
                colour,
                1.0,
            );
        }
        Glyph::Write => canvas.rect(at[0] - r, at[1] - r, at[0] + r, at[1] + r, colour, 1.0),
        Glyph::Run => canvas.fill_polygon(
            &[
                [at[0], at[1] - r],
                [at[0] + r, at[1] + r * 0.8],
                [at[0] - r, at[1] + r * 0.8],
            ],
            colour,
            1.0,
        ),
        Glyph::Verify => {
            stroke_circle(canvas, at, r, r * 0.34, colour);
            stroke_circle(canvas, at, r * 0.5, r * 0.34, colour);
        }
    }
}

/// The M1 base map with **simulated** M4 clouds and M5 marks on top of it.
///
/// This render exists to settle one argument with an image instead of a
/// paragraph. The fresh-eyes review reads the base map as too dark to be a map;
/// PRD §10.3 holds layers 1–2 in the bottom fifth of the range precisely so the
/// layers above have somewhere to go. Both claims are about the same picture and
/// only one of them can be right — so here is the picture: real M1 geometry,
/// real M1 tone, and stand-in clouds, agents and attention marks each drawn
/// inside the band §10.3 reserves for it.
///
/// Nothing here is live data and none of it is M4 or M5 code. The territories
/// are the three heaviest packages, the workers sit on their buildings, and the
/// attention marks sit on the territory centres — all chosen from the layout by
/// a total order, so this image is exactly as reproducible as the map under it.
#[must_use]
pub fn render_band_validation(
    city: &City,
    structure: &Structure,
    title: &str,
    pixels: usize,
    supersample: usize,
) -> Canvas {
    let ss = supersample.max(1);
    let w = pixels * ss;
    let footer = w as f64 * 0.088;
    let mut canvas = Canvas::new(w, w, SEA_DEEP);
    let view = draw_map(&mut canvas, city, w, w - footer as usize, false, ss);
    let unit = map_unit(city, &view);
    let map_h = w as f64 - footer;

    // --- layer 3: clouds -------------------------------------------------
    //
    // Three territories, each the sum of one package's kernels, thresholded into
    // the 2–3 discrete bands PRD §10.4 asks for. The thresholds are absolute —
    // "how many kernels overlap here" — and never normalised against the
    // observed maximum, which is ADR-0020's finding applied to this render.
    let mut packages: BTreeMap<&str, (f64, Vec<Px>)> = BTreeMap::new();
    for building in city.layout.buildings.values() {
        let top = building.path.components().next().unwrap_or("");
        let entry = packages.entry(top).or_insert_with(|| (0.0, Vec::new()));
        entry.0 += f64::from(building.height);
        entry.1.push(view.at(building.footprint.centroid()));
    }
    let mut ranked: Vec<(&str, f64, Vec<Px>)> = packages
        .into_iter()
        .map(|(k, (mass, pts))| (k, mass, pts))
        .collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    ranked.truncate(3);

    let grid = 420usize;
    let mut field = vec![0.0f64; grid * grid];
    let radius = (unit * 2.6).max(w as f64 * 0.022);
    for (i, (_, _, pts)) in ranked.iter().enumerate() {
        // A deterministic thinning, so a large package does not simply out-mass
        // a small one by sheer count.
        let step = (pts.len() / 150).max(1);
        // Each territory gets its own bandwidth, so the three clouds are not
        // three copies of one shape (PRD §6.4).
        let reach = radius * 0.26f64.mul_add(i as f64, 0.74);
        for p in pts.iter().step_by(step) {
            let gx0 = (((p[0] - reach) / w as f64) * grid as f64).floor().max(0.0) as usize;
            let gx1 = ((((p[0] + reach) / w as f64) * grid as f64).ceil() as usize).min(grid - 1);
            let gy0 = (((p[1] - reach) / map_h) * grid as f64).floor().max(0.0) as usize;
            let gy1 = ((((p[1] + reach) / map_h) * grid as f64).ceil() as usize).min(grid - 1);
            for gy in gy0..=gy1 {
                let sy = (gy as f64 + 0.5) / grid as f64 * map_h;
                for gx in gx0..=gx1 {
                    let sx = (gx as f64 + 0.5) / grid as f64 * w as f64;
                    let dx = (sx - p[0]) / reach;
                    let dy = (sy - p[1]) / reach;
                    field[gy * grid + gx] += kernel(dx.mul_add(dx, dy * dy));
                }
            }
        }
    }
    let iso = [0.9f64, 2.6, 5.4];
    let alpha = [0.42f64, 0.52, 0.62];
    for y in 0..(map_h as usize).min(canvas.height) {
        let gy = ((y as f64 + 0.5) / map_h * grid as f64)
            .floor()
            .clamp(0.0, (grid - 1) as f64) as usize;
        for x in 0..canvas.width {
            let gx = ((x as f64 + 0.5) / w as f64 * grid as f64)
                .floor()
                .clamp(0.0, (grid - 1) as f64) as usize;
            let v = field[gy * grid + gx];
            let band = if v >= iso[2] {
                2
            } else if v >= iso[1] {
                1
            } else if v >= iso[0] {
                0
            } else {
                continue;
            };
            wash_pixel(
                &mut canvas,
                x,
                y,
                CLOUD_TONES[band],
                alpha[band],
                CLOUD_BAND.0,
            );
        }
    }

    // PRD §10.3: clouds are drawn *beneath* the district outlines so the map
    // stays readable through them, so the package skeleton goes back on top of
    // the wash.
    {
        let (_, package_edges) = borders(city);
        let mut ink = MapInk {
            canvas: &mut canvas,
        };
        for (i, segment) in city.layout.roads.segments.iter().enumerate() {
            if !package_edges.contains(&i) {
                continue;
            }
            let (Some(a), Some(b)) = (
                city.layout.roads.nodes.get(segment.from.0 as usize),
                city.layout.roads.nodes.get(segment.to.0 as usize),
            ) else {
                continue;
            };
            ink.polyline(
                &[view.at(a.position), view.at(b.position)],
                (unit * 0.030).max(ss as f64 * 1.4),
                BORDER,
                0.95,
            );
        }
    }

    draw_labels(&mut canvas, city, &view, unit, ss, pixels);

    // --- layer 4: agents -------------------------------------------------
    let glyphs = [
        Glyph::Edit,
        Glyph::Read,
        Glyph::Write,
        Glyph::Run,
        Glyph::Verify,
    ];
    let r = (unit * 0.30).max(3.0 * ss as f64);
    let mut anchors: Vec<Px> = Vec::new();
    for (i, (_, _, pts)) in ranked.iter().enumerate() {
        if pts.is_empty() {
            continue;
        }
        // The thread's anchor is the middle of its territory; its workers are
        // spread through the package by a fixed stride, which is as close as a
        // static image gets to "a thread is in several places at once".
        let anchor = ring_centre(pts);
        anchors.push(anchor);
        let step = (pts.len() / 6).max(1);
        let mut trail: Vec<Px> = Vec::new();
        for (k, p) in pts.iter().step_by(step).take(5).enumerate() {
            canvas.segment(anchor, *p, (r * 0.16).max(1.0), AGENT_TETHER, 0.75);
            trail.push(*p);
            let colour = match (i + k) % 3 {
                0 => AGENT_PENDING,
                1 => AGENT_DONE,
                _ => AGENT_FAILED,
            };
            draw_glyph(&mut canvas, *p, r, glyphs[(i + k) % glyphs.len()], colour);
        }
        if trail.len() >= 2 {
            canvas.polyline(&trail, (r * 0.22).max(1.0), AGENT_TRAIL, 0.55);
        }
    }

    // --- layer 5: attention ----------------------------------------------
    //
    // PRD §11.2's three states, owning the top of the range: a standing pin over
    // the thread that is waiting on a human, a decaying ring over one that
    // finished unverified, and a **link** — not a badge — joining the two
    // threads that are about to collide.
    if let Some(at) = anchors.first().copied() {
        let h = r * 3.4;
        canvas.segment(
            at,
            [at[0], at[1] - h],
            (r * 0.22).max(1.2),
            ATTN_DECISION,
            0.95,
        );
        canvas.fill_polygon(
            &[
                [at[0], at[1] - h - r * 1.15],
                [at[0] + r * 0.80, at[1] - h],
                [at[0], at[1] - h + r * 1.15],
                [at[0] - r * 0.80, at[1] - h],
            ],
            ATTN_DECISION,
            1.0,
        );
    }
    if let Some(at) = anchors.get(1).copied() {
        draw_glyph(&mut canvas, at, r * 1.7, Glyph::Verify, ATTN_DONE);
    }
    if anchors.len() >= 2 {
        let a = anchors[0];
        let b = anchors[anchors.len() - 1];
        canvas.polyline(
            &[
                a,
                [
                    f64::midpoint(a[0], b[0]),
                    (r * 4.0f64).mul_add(-1.0, f64::midpoint(a[1], b[1])),
                ],
                b,
            ],
            (r * 0.34).max(1.6),
            ATTN_CONTENTION,
            0.92,
        );
        for end in [a, b] {
            canvas.disc(end, r * 0.9, ATTN_CONTENTION, 1.0);
        }
    }

    draw_footer(&mut canvas, city, structure, title, w, footer);
    // One extra key line: the allocation is this render's whole subject.
    let u = footer / 128.0;
    let y0 = w as f64 - footer;
    let mut x = 14.0 * u;
    let key = [
        ("BASE MAP 0-48 (M1, REAL)", ROOF_HI),
        ("CLOUDS 49-96 (M4, SIMULATED)", CLOUD_TONES[1]),
        ("AGENTS 97-168 (M4, SIMULATED)", AGENT_PENDING),
        ("ATTENTION 169-255 (M5, SIMULATED)", ATTN_DECISION),
    ];
    for (name, colour) in key {
        let size = 2.0 * u * 0.72;
        let entry = 13.0f64.mul_add(u, Canvas::text_width(name, size)) + 13.0 * u;
        if x + entry > w as f64 - 14.0 * u {
            break;
        }
        canvas.rect(x, y0 + 114.0 * u, x + 9.0 * u, y0 + 123.0 * u, colour, 1.0);
        // The label is in the footer's own ink, never in the band's: a swatch
        // at channel 48 on a plate at channel 8 is a legible *swatch* and an
        // illegible *word*.
        canvas.text(x + 13.0 * u, y0 + 114.5 * u, name, size, FOOTER_TEXT);
        x += entry;
    }
    canvas.downsample(ss)
}

#[cfg(test)]
mod tests {
    use super::*;
    use polis_layout::city;
    use polis_repo::synthetic;

    fn small_city() -> City {
        city::generate_city(&synthetic::repository(240, 0x51))
    }

    /// Rec. 709 luma of a pixel, the same measure the palette is written in.
    fn pixel_luma(p: [u8; 3]) -> f64 {
        luma([f64::from(p[0]), f64::from(p[1]), f64::from(p[2])])
    }

    /// The settlement footprint, as a pixel mask.
    ///
    /// Derived from the *geometry* — the traced coastline — rather than from a
    /// brightness threshold, because a mask cut by brightness moves when the
    /// palette moves, and a palette change that flatters its own mask proves
    /// nothing. Even-odd across every ring, so interior voids are excluded the
    /// way the coastline draws them.
    fn footprint_mask(city: &City, pixels: usize) -> Vec<bool> {
        let (lo, hi) = world_bounds(city);
        let view = View::fit(lo, hi, pixels, pixels, pixels as f64 * 0.022);
        let rings: Vec<Vec<Px>> = settlement_outline(city)
            .iter()
            .map(|r| view.ring(r))
            .collect();
        let mut mask = vec![false; pixels * pixels];
        let mut crossings: Vec<f64> = Vec::new();
        for y in 0..pixels {
            let sy = y as f64 + 0.5;
            crossings.clear();
            for ring in &rings {
                for i in 0..ring.len() {
                    let a = ring[i];
                    let b = ring[(i + 1) % ring.len()];
                    let (p, q) = if a[1] < b[1] { (a, b) } else { (b, a) };
                    if sy >= p[1] && sy < q[1] {
                        crossings.push(p[0] + (q[0] - p[0]) * (sy - p[1]) / (q[1] - p[1]));
                    }
                }
            }
            crossings.sort_by(f64::total_cmp);
            for pair in crossings.as_chunks::<2>().0 {
                let x0 = pair[0].ceil().max(0.0) as usize;
                let x1 = (pair[1].floor() as i64).clamp(0, pixels as i64 - 1) as usize;
                for x in x0..=x1.max(x0) {
                    if x < pixels {
                        mask[y * pixels + x] = true;
                    }
                }
            }
        }
        mask
    }

    fn percentile(sorted: &[f64], q: f64) -> f64 {
        if sorted.is_empty() {
            return 0.0;
        }
        let i = ((sorted.len() - 1) as f64 * q).round() as usize;
        sorted[i]
    }

    /// PRD §10.3 says the base map is faint. It does not say the base map may
    /// waste the range it is given, and the first render used 28 of its 48
    /// levels — a smudge where the spec asks for a coastline.
    ///
    /// Three separate claims, because "contrast" alone is a number a dark render
    /// can win by being darker: the *fabric* spans a real range, there is ink at
    /// the top of the band, and the sea is at the bottom of it.
    #[test]
    fn the_base_map_spends_the_band_it_is_given() {
        let c = small_city();
        let pixels = 500;
        let canvas = render_base_map(&c, pixels, 1, false);
        let mask = footprint_mask(&c, pixels);
        let mut land: Vec<f64> = canvas
            .pixels
            .as_chunks::<3>()
            .0
            .iter()
            .zip(mask.iter())
            .filter(|(_, inside)| **inside)
            .map(|(p, _)| pixel_luma([p[0], p[1], p[2]]))
            .collect();
        land.sort_by(f64::total_cmp);
        assert!(!land.is_empty(), "nothing was drawn");
        let contrast = percentile(&land, 0.95) - percentile(&land, 0.05);
        assert!(
            contrast >= 34.0,
            "base-map ink contrast is {contrast:.1} of the {BASE_MAP_CEILING} levels available"
        );
        // Ink at the top of the band. This is the half the first render lost:
        // roads, the coastline and the tall roofs all have to *reach* 48, or the
        // ceiling is a limit nothing ever touches.
        let bright = canvas
            .pixels
            .as_chunks::<3>()
            .0
            .iter()
            .filter(|p| p.iter().copied().max().unwrap_or(0) >= 44)
            .count();
        let share = 1_000 * bright / (canvas.width * canvas.height);
        assert!(
            share >= 10,
            "only {}% of the map reaches the top of its own band",
            share as f64 / 10.0
        );
        // And the sea really is at the bottom, which is what gives the land a
        // silhouette at thumbnail size.
        let sea: Vec<f64> = canvas
            .pixels
            .as_chunks::<3>()
            .0
            .iter()
            .map(|p| pixel_luma([p[0], p[1], p[2]]))
            .filter(|y| *y <= 12.0)
            .collect();
        let mean = sea.iter().sum::<f64>() / sea.len().max(1) as f64;
        assert!(mean < 8.0, "the sea sits at {mean:.1}, not near zero");
    }

    /// The ladder the palette is built on, asserted rather than commented.
    #[test]
    fn ground_stays_under_the_built_mass_and_roads_reach_the_top() {
        let c = small_city();
        let roof_floor = pixel_luma(ROOF_LO);
        for (path, colour) in district_colours(&c) {
            let head = colour.iter().copied().max().unwrap_or(0);
            assert!(
                head <= GROUND_CEILING,
                "{path} is {colour:?}: ground may not enter the buildings' half of the band"
            );
            assert!(
                pixel_luma(colour) < roof_floor,
                "{path} is {colour:?}, which is not darker than the shortest roof {ROOF_LO:?}"
            );
            let lot = mix(colour, LOT_LIFT, 0.28);
            assert!(pixel_luma(lot) < roof_floor, "{path}'s lot is {lot:?}");
        }
        // Roads are the base map's brightest reliable ink: they exist on every
        // map, where a tall building does not.
        assert_eq!(ROAD_HI.iter().copied().max(), Some(BASE_MAP_CEILING));
        assert!(pixel_luma(ROAD_HI) > pixel_luma(ROAD_MID));
        assert!(pixel_luma(ROAD_MID) > pixel_luma(ROAD_LO));
        assert!(pixel_luma(LIMIT) > 40.0, "the coastline is not crisp ink");
    }

    /// Hue, not lightness, is what separates two packages.
    #[test]
    fn district_tints_differ_in_hue_and_not_in_lightness() {
        let mut lumas: Vec<f64> = Vec::new();
        let mut chromas: Vec<f64> = Vec::new();
        for step in 0..24 {
            let t = ground_tint(f64::from(step) / 24.0, 0.0);
            lumas.push(pixel_luma(t));
            chromas.push(f64::from(
                t.iter().copied().max().unwrap_or(0) - t.iter().copied().min().unwrap_or(0),
            ));
        }
        let spread = lumas.iter().copied().fold(f64::MIN, f64::max)
            - lumas.iter().copied().fold(f64::MAX, f64::min);
        assert!(
            spread <= GROUND_LUMA.1 - GROUND_LUMA.0 + 1.0,
            "the hue circle spans {spread:.1} levels of lightness, which is a second encoding"
        );
        let weakest = chromas.iter().copied().fold(f64::MAX, f64::min);
        let strongest = chromas.iter().copied().fold(f64::MIN, f64::max);
        assert!(
            weakest >= 10.0,
            "the faintest hue carries only {weakest} levels"
        );
        assert!(
            strongest <= GROUND_CHROMA + 1.0,
            "one hue shouts at {strongest} levels while another whispers at {weakest}"
        );
    }

    /// The whole point of the two-register ramp: on a **clean** checkout, where
    /// every height comes from file size alone, roofs must still span a range
    /// the eye can rank. A single linear ramp to the work register's reference
    /// squeezed that case into a sixth of the tone range, and that is why the
    /// census found one roof grey.
    #[test]
    fn a_clean_checkout_still_spends_most_of_the_roof_ramp() {
        use polis_layout::buildings::{BASE_HEIGHT, SETTLED_CEILING};
        let low = mix(ROOF_LO, ROOF_HI, height_ramp(BASE_HEIGHT));
        let top = mix(ROOF_LO, ROOF_HI, height_ramp(SETTLED_CEILING));
        let span = pixel_luma(top) - pixel_luma(low);
        let whole = pixel_luma(ROOF_HI) - pixel_luma(ROOF_LO);
        assert!(
            span >= whole * 0.6,
            "the settled register gets {span:.1} of {whole:.1} levels"
        );
        // …and the work register still owns the very top, so PRD §7.3 holds.
        assert!(height_ramp(SETTLED_CEILING) < height_ramp(HEIGHT_REFERENCE));
        assert_eq!(height_ramp(HEIGHT_REFERENCE), 1.0);
    }

    /// The roof tones a real city puts on the canvas, counted.
    ///
    /// This is the census the review had to run by hand, run in CI instead: a
    /// render whose buildings are all one tone passes every structural gate in
    /// the suite and fails the one thing the map is for.
    #[test]
    fn roof_tones_span_a_visible_range_in_the_pixels() {
        let mut c = small_city();
        // Give the city a spread of heights, since a fixture checkout has no
        // uncommitted work and would test only half the ramp.
        let keys: Vec<LogicalPath> = c.layout.buildings.keys().cloned().collect();
        for (i, k) in keys.iter().enumerate() {
            if let Some(b) = c.layout.buildings.get_mut(k) {
                b.height = 1.0 + (i % 12) as f32 * 2.0;
            }
        }
        let canvas = render_base_map(&c, 500, 2, false);
        let mut seen = [0usize; 256];
        for p in canvas.pixels.as_chunks::<3>().0 {
            seen[p[0] as usize] += 1;
        }
        // Count the levels inside the roof ramp that carry real area.
        let floor = usize::from(ROOF_LO[0]);
        let ceiling = usize::from(ROOF_HI[0]);
        let busy = (floor..=ceiling).filter(|v| seen[*v] > 40).count();
        assert!(
            busy >= 14,
            "only {busy} of the {} roof levels carry ink: the ramp is not reaching the canvas",
            ceiling - floor + 1
        );
    }

    /// The bullseye is gone, and it must not come back.
    ///
    /// A tall building used to be drawn as nested concentric set-backs, which
    /// reads as a target rather than a tower and forces the eye to count rings.
    /// The replacement is one lit side and one shaded side, so a horizontal cut
    /// across a tall roof crosses at most one light/dark boundary — where a
    /// bullseye gives at least three.
    #[test]
    fn a_tall_building_is_massed_and_not_ringed() {
        let square = [[6.0, 6.0], [58.0, 6.0], [58.0, 58.0], [6.0, 58.0]];
        let mut canvas = Canvas::new(64, 64, [0, 0, 0]);
        {
            let mut ink = MapInk {
                canvas: &mut canvas,
            };
            let roof = mix(ROOF_LO, ROOF_HI, 1.0);
            ink.fill_polygon(&square, roof, 1.0);
            draw_massing(
                &mut ink,
                &square,
                ring_centre(&square),
                roof,
                1.0,
                40.0,
                1.0,
            );
        }
        let row = 32usize;
        let mut alternations = 0;
        let mut last: Option<bool> = None;
        for x in 8..56 {
            let i = (row * 64 + x) * 3;
            let bright = canvas.pixels[i] >= 42;
            if last.is_some_and(|l| l != bright) {
                alternations += 1;
            }
            last = Some(bright);
        }
        assert!(
            alternations <= 2,
            "a scanline across the roof changes tone {alternations} times: that is a bullseye"
        );
    }

    /// The shore is a *line*, not a glow.
    ///
    /// The first version spread sixteen strokes over three block widths, which
    /// wrapped the settlement in a wide midtone halo and blurred the one edge
    /// the squint test reads. Measured here as: a few pixels out to sea, the
    /// tone must already be sea.
    #[test]
    fn the_coastline_is_crisp_and_not_a_shelf() {
        let c = small_city();
        let canvas = render_base_map(&c, 300, 1, false);
        let w = canvas.width;
        let at = |x: usize, y: usize| {
            let i = (y * w + x) * 3;
            pixel_luma([canvas.pixels[i], canvas.pixels[i + 1], canvas.pixels[i + 2]])
        };
        // Walk in from the left on every row; the first land pixel is the shore.
        let mut halo = 0usize;
        let mut shores = 0usize;
        for y in 0..canvas.height {
            let Some(x) = (0..w).find(|x| at(*x, y) > 16.0) else {
                continue;
            };
            if x < 10 {
                continue;
            }
            shores += 1;
            // Eight pixels out from the shore must already be open sea.
            if at(x - 8, y) > 11.0 {
                halo += 1;
            }
        }
        assert!(shores > 40, "only {shores} shore crossings were found");
        let share = 100 * halo / shores;
        assert!(
            share <= 25,
            "{share}% of the shore still has a midtone shelf eight pixels out"
        );
    }

    /// PRD §8's industrial zone is a dull **mass**, not a hole.
    #[test]
    fn an_industrial_mass_is_occupied_ground() {
        let ring = [[4.0, 4.0], [92.0, 4.0], [92.0, 92.0], [4.0, 92.0]];
        let mut canvas = Canvas::new(96, 96, [0, 0, 0]);
        {
            let mut ink = MapInk {
                canvas: &mut canvas,
            };
            ink.fill_polygon(&ring, INDUS, 1.0);
            hatch(&mut ink, &ring, 9.0, INDUS_HATCH, 0.60);
        }
        let mut tones: BTreeMap<u8, usize> = BTreeMap::new();
        for y in 12..84 {
            for x in 12..84 {
                *tones.entry(canvas.pixels[(y * 96 + x) * 3]).or_insert(0) += 1;
            }
        }
        assert!(
            tones.len() >= 2,
            "the industrial mass is one flat tone, which is what makes it read as a hole"
        );
        let textured: usize = tones
            .iter()
            .filter(|(v, _)| **v > INDUS[0])
            .map(|(_, n)| *n)
            .sum();
        let share = 100 * textured / (72 * 72);
        assert!(
            (15..=60).contains(&share),
            "the hatch covers {share}% of the mass: it is either invisible or a second city"
        );
        // Dull is the point: the zone must not be brighter than a roof.
        for v in tones.keys() {
            assert!(*v < ROOF_HI[0], "the industrial mass reaches {v}");
        }
    }

    /// Nothing in the footer may be cut off, at any canvas size.
    ///
    /// Two consecutive reviews could only read half of the caption naming the
    /// tallest building on the map, because the footer was drawn at a fixed size
    /// against a canvas whose width was not.
    #[test]
    fn the_footer_is_never_clipped_at_any_canvas_size() {
        let c = small_city();
        let s = city::measure(&c);
        for (pixels, ss) in [(240usize, 1usize), (400, 1), (800, 2), (1600, 2)] {
            let canvas = render_plan(
                &c,
                &s,
                "POLIS / A REPOSITORY WITH A VERY LONG NAME",
                pixels,
                ss,
                false,
            );
            let footer_top = canvas.height - (canvas.height as f64 * 0.088) as usize;
            let pad = ((canvas.width as f64 * 0.088) / 128.0 * 14.0) as usize;
            // The right margin and the bottom margin must contain nothing but
            // the footer plate.
            for y in footer_top + 3..canvas.height {
                for x in canvas.width - pad / 2..canvas.width {
                    let i = (y * canvas.width + x) * 3;
                    let p = [canvas.pixels[i], canvas.pixels[i + 1], canvas.pixels[i + 2]];
                    assert!(
                        pixel_luma(p) <= pixel_luma(PLATE) + 6.0,
                        "ink at ({x}, {y}) is in the right margin at {pixels}px x{ss}: {p:?}"
                    );
                }
            }
            let last = canvas.height - 1;
            for x in 0..canvas.width {
                let i = (last * canvas.width + x) * 3;
                let p = [canvas.pixels[i], canvas.pixels[i + 1], canvas.pixels[i + 2]];
                assert!(
                    pixel_luma(p) <= pixel_luma(PLATE) + 6.0,
                    "ink is touching the bottom edge at x={x}, {pixels}px x{ss}: {p:?}"
                );
            }
        }
        // The validation render carries an extra key row, which is the row
        // closest to the edge and therefore the one most likely to be cut.
        for (pixels, ss) in [(300usize, 1usize), (800, 2)] {
            let canvas = render_band_validation(&c, &s, "POLIS / A LONG TITLE", pixels, ss);
            let last = canvas.height - 1;
            for x in 0..canvas.width {
                let i = (last * canvas.width + x) * 3;
                let p = [canvas.pixels[i], canvas.pixels[i + 1], canvas.pixels[i + 2]];
                assert!(
                    pixel_luma(p) <= pixel_luma(PLATE) + 6.0,
                    "the band key is cut at x={x}, {pixels}px x{ss}: {p:?}"
                );
            }
        }
    }

    /// The evidence render puts ink in every reserved band, and leaves the base
    /// map legible underneath.
    #[test]
    fn the_band_validation_uses_every_reserved_band() {
        let c = small_city();
        let s = city::measure(&c);
        let canvas = render_band_validation(&c, &s, "TEST", 400, 1);
        let map_rows = (canvas.height as f64 * (1.0 - 0.088)) as usize;
        let mut in_band = [0usize; 4];
        let mut base = 0usize;
        for y in 0..map_rows {
            for x in 0..canvas.width {
                let i = (y * canvas.width + x) * 3;
                let head = canvas.pixels[i..i + 3].iter().copied().max().unwrap_or(0);
                if head <= BASE_MAP_CEILING {
                    base += 1;
                } else if head <= CLOUD_BAND.1 {
                    in_band[1] += 1;
                } else if head <= AGENT_BAND.1 {
                    in_band[2] += 1;
                } else {
                    in_band[3] += 1;
                }
            }
        }
        in_band[0] = base;
        for (i, name) in ["base map", "clouds", "agents", "attention"]
            .iter()
            .enumerate()
        {
            assert!(in_band[i] > 0, "nothing was drawn in the {name} band");
        }
        // The point of the exercise: the base map is still most of the picture.
        let total: usize = in_band.iter().sum();
        assert!(
            base * 2 > total,
            "the overlays cover the map: only {base} of {total} pixels are still base map"
        );
    }

    /// Every render is a pure function of the layout, including the new one.
    #[test]
    fn the_band_validation_is_byte_identical_across_runs() {
        let c = small_city();
        let s = city::measure(&c);
        let a = render_band_validation(&c, &s, "TEST", 220, 1).encode_png();
        let b = render_band_validation(&c, &s, "TEST", 220, 1).encode_png();
        assert_eq!(a, b, "the validation render moved between two runs");
    }

    #[test]
    fn a_plan_is_drawn_and_is_not_blank() {
        let c = small_city();
        let s = city::measure(&c);
        let canvas = render_plan(&c, &s, "TEST", 320, 1, true);
        assert_eq!(canvas.width, 320);
        let distinct: BTreeSet<[u8; 3]> = canvas
            .pixels
            .as_chunks::<3>()
            .0
            .iter()
            .map(|p| [p[0], p[1], p[2]])
            .collect();
        assert!(
            distinct.len() > 40,
            "only {} distinct colours: the plan is blank",
            distinct.len()
        );
    }

    #[test]
    fn the_render_is_byte_identical_across_runs() {
        let c = small_city();
        let s = city::measure(&c);
        let a = render_plan(&c, &s, "TEST", 240, 1, true).encode_png();
        let b = render_plan(&c, &s, "TEST", 240, 1, true).encode_png();
        assert_eq!(a, b, "the PNG moved between two runs");
        let j1 = render_junctions(&c, &s, "TEST", 240, 1).encode_png();
        let j2 = render_junctions(&c, &s, "TEST", 240, 1).encode_png();
        assert_eq!(j1, j2);
    }

    /// PRD §10.3, enforced on pixels rather than on the palette.
    #[test]
    fn the_base_map_stays_inside_the_bottom_fifth_of_the_contrast_range() {
        let c = small_city();
        for supersample in [1, 2] {
            let canvas = render_base_map(&c, 200, supersample, true);
            for (i, chunk) in canvas.pixels.as_chunks::<3>().0.iter().enumerate() {
                let max = chunk.iter().copied().max().unwrap_or(0);
                assert!(
                    max <= BASE_MAP_CEILING,
                    "pixel {i} is {chunk:?}: channel {max} is above the layer 1-2 ceiling {BASE_MAP_CEILING}"
                );
            }
        }
    }

    /// The ceiling is only worth anything if it really bounds `L*`.
    #[test]
    fn the_channel_ceiling_bounds_lightness_at_a_fifth_of_the_range() {
        // `L*` of the grey at the ceiling, computed here rather than asserted
        // from a constant, so a change to the ceiling is caught by this test.
        let v = f64::from(BASE_MAP_CEILING) / 255.0;
        let lin = ((v + 0.055) / 1.055).powf(2.4);
        let l = 116.0 * lin.cbrt() - 16.0;
        assert!(
            l <= 20.0,
            "the ceiling admits L* {l}, which is not the bottom fifth"
        );
        assert!(l > 15.0, "the ceiling is so low the base map cannot read");
        // Every reserved band starts above the ceiling and they tile upward.
        assert!(CLOUD_BAND.0 > BASE_MAP_CEILING);
        assert!(AGENT_BAND.0 > CLOUD_BAND.1);
        assert!(ATTENTION_BAND.0 > AGENT_BAND.1);
        assert_eq!(ATTENTION_BAND.1, 255, "attention must own the top");
    }

    /// PRD §7.3's primary encoded quantity has to be visible.
    #[test]
    fn a_taller_building_is_drawn_lighter_and_casts_a_longer_shadow() {
        assert_eq!(height_ramp(1.0), 0.0);
        assert_eq!(height_ramp(HEIGHT_REFERENCE), 1.0);
        assert_eq!(height_ramp(1_000.0), 1.0, "the ramp is clamped, not scaled");
        assert!(height_ramp(4.0) > height_ramp(2.0));
        let low = mix(ROOF_LO, ROOF_HI, height_ramp(1.0));
        let high = mix(ROOF_LO, ROOF_HI, height_ramp(8.0));
        assert!(
            high[0] > low[0] + 8,
            "the roof ramp is too flat to see: {low:?} -> {high:?}"
        );
        assert!(high.iter().all(|c| *c <= BASE_MAP_CEILING));
    }

    /// Two cities differing only in one building's height must differ in pixels.
    /// This is the regression that the pixel census caught: every building at
    /// one RGB value, and the encoded quantity invisible.
    #[test]
    fn height_reaches_the_pixels() {
        let base = small_city();
        let mut taller = base.clone();
        let key = taller
            .layout
            .buildings
            .keys()
            .next()
            .expect("a city has buildings")
            .clone();
        let flat = render_base_map(&base, 300, 1, false);
        taller
            .layout
            .buildings
            .get_mut(&key)
            .expect("the key came from this map")
            .height = 9.0;
        let risen = render_base_map(&taller, 300, 1, false);
        let changed = flat
            .pixels
            .iter()
            .zip(risen.pixels.iter())
            .filter(|(a, b)| a != b)
            .count();
        assert!(
            changed > 0,
            "raising a building changed nothing in the image"
        );
    }

    #[test]
    fn a_package_is_one_hue_family() {
        let c = small_city();
        let colours = district_colours(&c);
        let industrial: BTreeSet<&LogicalPath> = c.industrial.iter().map(|m| &m.host).collect();
        let mut by_top: BTreeMap<&str, Vec<Rgb>> = BTreeMap::new();
        for (path, colour) in &colours {
            if industrial.contains(path) {
                continue;
            }
            by_top
                .entry(path.components().next().unwrap_or(""))
                .or_default()
                .push(*colour);
        }
        // Two districts of the same package must be closer in hue than the
        // spacing between packages.
        let families = by_top.len().max(2) as f64;
        for (top, family) in &by_top {
            if family.len() < 2 {
                continue;
            }
            let hues: Vec<f64> = family.iter().map(|c| hue_of(*c)).collect();
            let span = hues.iter().copied().fold(f64::MIN, f64::max)
                - hues.iter().copied().fold(f64::MAX, f64::min);
            assert!(
                span < 1.0 / families,
                "{top} spans {span} of the hue circle with {families} packages"
            );
        }
    }

    /// The ground is a tint, not a statement: this is what stops the base map
    /// eating the range the attention layer needs.
    #[test]
    fn the_district_ground_is_low_chroma() {
        let c = small_city();
        for (path, colour) in district_colours(&c) {
            let max = colour.iter().copied().max().unwrap_or(0);
            let min = colour.iter().copied().min().unwrap_or(0);
            assert!(
                u32::from(max - min) * 100 <= 255 * 12,
                "{path} is {colour:?}: chroma {} of 100 is a choropleth, not a tint",
                u32::from(max - min) * 100 / 255
            );
            assert!(max <= BASE_MAP_CEILING, "{path} is {colour:?}");
        }
    }

    fn hue_of(c: Rgb) -> f64 {
        let r = f64::from(c[0]) / 255.0;
        let g = f64::from(c[1]) / 255.0;
        let b = f64::from(c[2]) / 255.0;
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let d = max - min;
        if d < 1e-9 {
            return 0.0;
        }
        let h = if max == r {
            ((g - b) / d).rem_euclid(6.0)
        } else if max == g {
            (b - r) / d + 2.0
        } else {
            (r - g) / d + 4.0
        };
        h / 6.0
    }

    /// The silhouette is the settlement's own boundary, not its convex hull.
    #[test]
    fn the_outline_traces_the_settlement_and_not_its_hull() {
        let c = small_city();
        let rings = settlement_outline(&c);
        assert!(!rings.is_empty(), "no coastline was traced");
        // Every outline edge belongs to exactly one block: that is what makes
        // it the settlement's boundary rather than a shape drawn around it.
        let mut multiplicity: BTreeMap<Edge, u32> = BTreeMap::new();
        for block in &c.layout.blocks {
            let r = &block.boundary.vertices;
            for i in 0..r.len() {
                let a = grid_key(r[i]);
                let b = grid_key(r[(i + 1) % r.len()]);
                if a != b {
                    *multiplicity.entry(edge(a, b)).or_insert(0) += 1;
                }
            }
        }
        let mut traced = 0usize;
        for ring in &rings {
            for i in 0..ring.len() {
                let a = grid_key(ring[i]);
                let b = grid_key(ring[(i + 1) % ring.len()]);
                if a == b {
                    continue;
                }
                assert_eq!(
                    multiplicity.get(&edge(a, b)).copied(),
                    Some(1),
                    "the outline used an interior edge"
                );
                traced += 1;
            }
        }
        let boundary = multiplicity.values().filter(|c| **c == 1).count();
        assert_eq!(traced, boundary, "the coastline is incomplete");
        assert!(
            boundary > 3,
            "a settlement with {boundary} boundary edges is a triangle"
        );
    }

    /// The outline must be able to describe a concavity, which a hull cannot.
    #[test]
    fn the_outline_is_tighter_than_the_convex_hull() {
        let c = small_city();
        let rings = settlement_outline(&c);
        let outline_area: f64 = rings
            .iter()
            .map(|r| {
                let px: Vec<Px> = r.iter().map(|p| [f64::from(p.x), f64::from(p.y)]).collect();
                ring_area(&px)
            })
            .fold(f64::MIN, f64::max);
        let mut pts: Vec<Px> = c
            .layout
            .blocks
            .iter()
            .flat_map(|b| b.boundary.vertices.iter())
            .map(|p| [f64::from(p.x), f64::from(p.y)])
            .collect();
        pts.sort_by(|a, b| a[0].total_cmp(&b[0]).then_with(|| a[1].total_cmp(&b[1])));
        pts.dedup();
        let turn =
            |o: Px, a: Px, b: Px| (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0]);
        let chain = |order: &[Px]| -> Vec<Px> {
            let mut half: Vec<Px> = Vec::new();
            for &p in order {
                while half.len() >= 2 && turn(half[half.len() - 2], half[half.len() - 1], p) <= 0.0
                {
                    half.pop();
                }
                half.push(p);
            }
            half.pop();
            half
        };
        let reversed: Vec<Px> = pts.iter().rev().copied().collect();
        let mut hull = chain(&pts);
        hull.extend(chain(&reversed));
        let hull_area = ring_area(&hull);
        assert!(
            outline_area <= hull_area + 1e-6,
            "the traced outline {outline_area} is larger than the hull {hull_area}"
        );
    }

    /// PRD §8: monuments are labelled, and the skeleton survives decluttering.
    #[test]
    fn monuments_and_packages_win_the_label_budget() {
        let mut order = [LabelKind::Quarter, LabelKind::Monument, LabelKind::Package];
        order.sort_unstable();
        assert_eq!(
            order,
            [LabelKind::Monument, LabelKind::Package, LabelKind::Quarter]
        );
    }

    /// A roof form must actually put a second tone on the roof.
    #[test]
    fn roof_form_adds_tone() {
        let square = [[4.0, 4.0], [20.0, 4.0], [20.0, 20.0], [4.0, 20.0]];
        for form in RoofForm::ALL {
            let mut canvas = Canvas::new(24, 24, [0, 0, 0]);
            {
                let mut ink = MapInk {
                    canvas: &mut canvas,
                };
                ink.fill_polygon(&square, ROOF_LO, 1.0);
                draw_roof_form(&mut ink, &square, ring_centre(&square), ROOF_LO, form);
            }
            let tones: BTreeSet<[u8; 3]> = canvas
                .pixels
                .as_chunks::<3>()
                .0
                .iter()
                .map(|p| [p[0], p[1], p[2]])
                .filter(|p| *p != [0, 0, 0])
                .collect();
            let expected = usize::from(form != RoofForm::Flat) + 1;
            assert!(
                tones.len() >= expected,
                "{form:?} produced {} tones, wanted {expected}",
                tones.len()
            );
            for t in tones {
                assert!(t.iter().all(|c| *c <= BASE_MAP_CEILING), "{t:?}");
            }
        }
    }
}
