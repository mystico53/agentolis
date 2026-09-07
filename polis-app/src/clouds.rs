//! Territory clouds (PRD §10.4).
//!
//! > Discrete iso-contour bands, **2–3 levels, never a continuous blur.**
//! > Continuous gradients turn to mush and you lose the ability to say "that
//! > file is in the core of this thread's work" versus "it's at the fringe."
//!
//! This is the window's half of that layer, and it is deliberately **not** a
//! second implementation. The field, the thresholds, the contour, the hatch, the
//! stacking and the tween all come from [`polis_render::live`]; this module maps
//! kernels into a fixed texel frame, hands them over, and uploads what comes
//! back. A visual language with two definitions has none, and the previous
//! version proved it — see below.
//!
//! # What was here before, and why it had to go
//!
//! The first version thresholded the field into three bands and **filled** each
//! one with a translucent colour: `bands[0].alpha(0.42)` through
//! `bands[2].alpha(0.70)`, one `Color32` per texel, no texel left unpainted.
//!
//! That is an area fill, and the same construction was measured on the
//! rasteriser's side of the house: it inked two thirds of its own footprint,
//! lifted 40 % of the city by more than six levels, and moved the median
//! luminance of the base map underneath it from `L 22` to `L 45` — the map
//! fogging into pale grey exactly where the activity was, which is the one place
//! it must not. PRD §10.3 puts clouds beneath the district outlines *so the map
//! stays readable*, and a wash makes that sentence false however low the alpha
//! goes.
//!
//! What is drawn instead is what [`polis_render::live::paint_cloud_stack`]
//! draws: a contour stroke on each level's boundary, widest on the outermost
//! because that silhouette is what survives being seen from across the room,
//! plus a hatch whose spacing tightens toward the core. Every mark is **opaque**
//! and every other texel was fully transparent, so the city underneath was not
//! merely recoverable, it was untouched — measured at **zero** disturbed pixels,
//! and a median luminance that moves by `0.000` levels, over 192 frames of six
//! real sessions replayed side by side (`polis-render/tests/cloud_measure.rs`).
//! That is still exactly what this module draws with the veil at zero; see the
//! third version below for what the operator may now trade it for.
//!
//! # And the second version, which absorbed one thread into another
//!
//! That notation was drawn **once for the whole sky**: every territory's kernels
//! summed into one field, thresholded once, and each texel given to whichever
//! thread was densest there. Two consequences, both visible on the operator's
//! own map and neither tunable:
//!
//! * a thread whose cloud crossed a busier one was not dimmed under it, it was
//!   **gone** — the argmax handed every shared texel to the winner, so the map
//!   could not say that two threads were in one place, which two, or how far
//!   each reached;
//! * and the band level came from the *sum*, so two fringes overlapping drew a
//!   core belonging to neither, breaking the one sentence §10.4 asks the bands
//!   to carry: *"that file is in the core of **this thread's** work"*.
//!
//! [`polis_render::live::CloudField::stack`] draws one banded, contoured and
//! hatched layer **per territory** instead, each in its own hue and on its own
//! hatch axis, painted over each other urgent-last. The marks are sparse, so two
//! weaves 30° apart interleave rather than one erasing the other, and each
//! layer's hatch opens up by how many territories share the pixel — so the
//! second reading is paid for out of the first one's ink budget and the city
//! underneath is as untouched as it was with one layer.
//!
//! Contention is still PRD §6.4's, and better told: it used to be a crossed
//! hatch in one thread's hue saying *somebody else is here too*, and it is now
//! two hues, two silhouettes and two stroke directions saying **who**.
//!
//! # And the third version, which gave the marks a ground to stand on
//!
//! Sparse opaque marks leave the city untouched, and that is also what was
//! wrong with them: *"the clouds need an even background, they don't
//! distinguish themselves."* A weave over a city block reads as texture **on**
//! the map rather than as a region of it, and a territory's silhouette was
//! carried by a contour one to three pixels wide.
//!
//! So each band now also lays down an even ground —
//! [`polis_render::live::CLOUD_BODY`], a tone *below* the base map's own ceiling,
//! at [`polis_render::live::CLOUD_BODY_ALPHA`] scaled by the operator's
//! [`crate::config::Look::cloud_veil`]. It levels rather than fills: a lit
//! district under a cloud comes down toward the floor tone, near-black ground
//! barely moves, and the region's variance collapses. That is what makes it read
//! as one place. How much of the city it may take with it is the operator's own
//! setting, because it is a trade between the map's legibility and the cloud's
//! and no measurement settles it (ADR-0107).
//!
//! At `cloud_veil = 0` this module draws exactly what it drew before, zero
//! disturbed pixels included.
//!
//! # Nearest for the marks, linear for the ground
//!
//! A sparse mark stretched with a linear filter is a smear whose edges land at
//! every intermediate alpha, and a half-transparent cloud tone composites into
//! the base map's own contrast band — which is the fill this module keeps
//! removing, arriving by the back door. [`egui::TextureOptions::NEAREST`] keeps
//! a mark a mark at every zoom.
//!
//! The body is the opposite case. It is an even field, and nearest sampling
//! magnifies it into a wall of blocks — the *"super pixelated"* the operator
//! reported, which is the texel grid becoming visible rather than anything about
//! the notation. Filtered linearly, a band step becomes a ramp one texel wide
//! and the silhouette is smooth at any zoom.
//!
//! One image cannot have two filters, so [`Clouds`] uploads two.
//!
//! # Why the texel frame is fixed to the whole base map
//!
//! The grid used to be anchored to the territories' own bounding box, which
//! moves whenever a territory grows a lobe. PRD §13 asks for the cloud density
//! to be **tweened between updates**, and a tween across a moving coordinate
//! frame either resamples every frame or jumps. Anchoring the texel grid to the
//! base map instead makes the frame constant for the life of the city: the
//! *image* is still only the cloud's own rectangle, so nothing large is
//! uploaded, but the field the tween interpolates never has to be re-registered.

// The field is a numeric grid: every cast here lands in a texel index that is
// clamped on purpose.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

use eframe::egui::{self, Color32, Pos2, Rect};
use polis_events::ThreadId;
use polis_render::live::{self, CloudCensus, CloudField, CloudKernel, CloudTween};
use polis_world::snapshot::WorldSnapshot;
use polis_world::territory::{self, CloudPolicy, Territory};

use crate::basemap::{BaseMap, BASE_MAP_PIXELS};

/// The cloud field's edge, in texels, across the **whole base map**.
///
/// PRD §10.4 specifies 512² for the GPU target. This is a thousand and
/// twenty-four because it is a CPU rasterisation the operator zooms into: at
/// 1 600 base-map pixels that is 1.56 px per texel, so a contour stroke is still
/// a stroke at the district tier rather than a staircase.
///
/// It is not what the layer costs. The field is only ever sampled, thresholded
/// and painted over the territories' own bounding rectangle — a few hundred
/// texels on a side in practice — and the lattice underneath it is capped at
/// 256² by `polis_render::live` whatever the rectangle's size.
pub const TEXELS: usize = 1024;

/// A field within this fraction of its target on every texel has arrived, and
/// the layer stops repainting until the world moves again (PRD §13.1's idle
/// budget: *"< 2 % of one core"*).
const SETTLED: f32 = 0.02;

/// How much brighter the cloud of the thread the operator is asking about is
/// drawn: a multiplier on every tone that layer emits.
///
/// # Why brightness, and why nothing new is drawn
///
/// The card in the rail and the cloud on the map already carry one hue
/// (PRD §11.4), so the question a hover asks — *which shape is this row?* — is
/// answered by lighting that hue up. The map used to answer it with **lines**:
/// pointing at a card revealed the thread's trail and its workers' tethers, and
/// the operator's verdict on a map full of them was *"still lines everywhere!!"*
/// — then, on the quieter version, *"instead of these lines when hovering over a
/// thread card, highlight the cloud"*. A cloud that brightens says the same
/// thing with no ink the map did not already have, and it says it about the
/// region rather than about a path across the city.
///
/// `1.8` lifts the marks from the 56–84 of `polis_render::live::CLOUD_TONES` to
/// 100–151: clear of the band the rest of the sky is drawn in, and still under
/// the 168 of `crate::palette::thread`, so a worker glyph stays the brightest
/// thing in its own hue.
const LIT_GAIN: f32 = 1.8;

/// One drawn cloud, in **base-map pixels**: whose it is, where it peaks, and
/// the rectangle its silhouette cannot leave.
///
/// The window used to have no way to say which thread a cloud belonged to
/// except its hue, because `select_clouds` returns territories and this module
/// kept only the tint it needed to paint them. So the map could draw five
/// clouds and name none of them, and the operator's only route from a shape on
/// the map to the thread that made it was to match a colour against the rail.
/// `crate::mapview::draw_thread_connectors` is the other half of the fix — the
/// line from the thread's rail card to its own cloud — and this is the half that
/// knows where the cloud is.
#[derive(Debug, Clone)]
pub struct CloudMark {
    /// Whose cloud it is.
    pub thread: ThreadId,
    /// The field layer it is drawn on — `polis_world::Thread::layer`, unique
    /// across threads and stable across frames, and the identity `ink_stack`
    /// matches when the operator asks about one thread.
    ///
    /// Not [`Self::tint`]: twelve hue slots and then they repeat, so matching on
    /// the hue would light two threads' clouds on a busy world and the highlight
    /// would stop meaning *this row*.
    pub layer: u16,
    /// Its identity hue slot, so the connector is drawn in the colour the cloud
    /// and the rail card already share.
    pub tint: u8,
    /// [`Territory::anchor`] — the kernel centre the field is highest at, which
    /// is where a leader should leave from. Not the middle of [`Self::bounds`]:
    /// a territory with lobes of unequal weight peaks over the heavy one and
    /// the gap between them is where a caption reads worst.
    pub at: Pos2,
    /// The kernels' bounding box, which contains the drawn cloud **exactly**.
    /// `polis_render::live`'s kernel has compact support and is zero at its own
    /// radius, so no contour can reach outside this.
    pub bounds: Rect,
    /// How wide the silhouette is **at [`Self::at`]'s own height**, which is
    /// where a leader has to attach.
    ///
    /// Not [`Self::bounds`], and the difference is the whole reason this field
    /// exists. A territory with lobes has a bounding box far wider than the
    /// cloud is at any one height, so a leader starting at the box's edge starts
    /// in open map — a dot floating beside the cloud with a gap behind it.
    /// Every kernel is a disc, so this is the union of their chords at that
    /// height, and the leader leaves ink.
    pub span: (f32, f32),
}

/// The rasterised cloud layer: two textures, repainted while it is moving.
///
/// # Why two, when it used to be one
///
/// The layer draws two kinds of pixel now and they want opposite filters.
///
/// * The **body** — [`polis_render::live::CLOUD_BODY_ALPHA`] — is an even
///   translucent ground, and it is what makes a cloud read as a region rather
///   than as texture lying on the city. Magnified with
///   [`egui::TextureOptions::NEAREST`] it is a wall of blocks, which is exactly
///   the *"super pixelated"* the operator reported; magnified with `LINEAR` it
///   is a smooth ramp a texel wide at the band steps, which is what a cloud
///   should look like at any zoom.
/// * The **marks** — contour and hatch — are sparse and opaque, and linear
///   filtering turns each one into a smear whose edges land at every
///   intermediate alpha. That is the fill this layer spent two rewrites getting
///   rid of, arriving by the back door. They stay `NEAREST`.
///
/// One image cannot have both filters, so there are two, drawn body-then-marks
/// over the same rectangle.
pub struct Clouds {
    /// The even ground, `LINEAR`.
    body: Option<egui::TextureHandle>,
    /// The contour and hatch, `NEAREST`, over the body.
    ink: Option<egui::TextureHandle>,
    /// How strong the body is, `0` for marks only. The operator's own setting —
    /// see [`polis_render::live::CLOUD_VEIL`] — held here so a change to it
    /// invalidates the texture the same way a change to the world does.
    veil: f64,
    /// The one thread the operator is asking about, whose cloud is drawn at
    /// [`LIT_GAIN`]. Held for the same reason as [`Self::veil`]: it changes the
    /// ink over an unmoved field, so it has to invalidate the texture.
    lit: Option<ThreadId>,
    /// The base-map rectangle the textures cover.
    rect: Rect,
    /// The world the target was built from.
    generation: u64,
    /// The cloud cap in force when the target was built.
    cap: usize,
    /// Where the world says the field should be.
    target: Option<CloudField>,
    /// Where it is (PRD §13's tween).
    tween: CloudTween,
    /// Whether the tween has arrived and the texture can be left alone.
    settled: bool,
    /// What [`polis_world::territory::select_clouds`] decided, less the one
    /// field this module cannot fill — see [`Clouds::census`].
    ///
    /// The window used to call `visible_clouds`, a wrapper that returns the
    /// chosen territories and drops `unplaced`, `dormant` and `capped` on the
    /// floor. So the status bar could say `0 clouds (0 kernels)` and had no way
    /// to say *why*, which is how the operator arrived at a map with no clouds
    /// and no thread to pull. The headless renderer has carried these counts
    /// since `CloudCensus` was written; this is the window's half.
    census: CloudCensus,
    /// The widest kernel in the target field, in **texels**.
    ///
    /// [`CloudCensus::widest_px`] wants output pixels, because
    /// [`CloudCensus::sub_pixel`] compares it against a minimum stroke width in
    /// output pixels. This module builds kernels in the fixed texel frame and
    /// has never seen the camera, so the conversion is deferred to
    /// [`Clouds::census`], where the caller has one. Converting here would make
    /// the *"ZOOM IN"* hint a statement about a texture the operator cannot
    /// zoom.
    widest_texels: f64,
    /// `polis_world::Thread::tint` per visible territory, in the order the
    /// kernels' `thread` field indexes — so a cloud is drawn in its own
    /// thread's hue and matches that thread's swatch in the rail.
    tints: Vec<u8>,
    /// Where each visible cloud is and whose it is, for `crate::callout`.
    /// Parallel to [`Self::tints`], and rebuilt with the target field.
    marks: Vec<CloudMark>,
    /// Texels two or more territories both claim — PRD §6.4's contention signal,
    /// visible before a write collides.
    pub contested: usize,
    /// How long the last repaint took.
    pub build_ms: f64,
}

impl std::fmt::Debug for Clouds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Clouds")
            .field("census", &self.census)
            .field("contested", &self.contested)
            .field("generation", &self.generation)
            .field("settled", &self.settled)
            .field("build_ms", &self.build_ms)
            .finish_non_exhaustive()
    }
}

impl Default for Clouds {
    fn default() -> Self {
        Self {
            body: None,
            ink: None,
            veil: live::CLOUD_VEIL,
            lit: None,
            rect: Rect::NOTHING,
            generation: u64::MAX,
            cap: usize::MAX,
            target: None,
            tween: CloudTween::default(),
            settled: false,
            census: CloudCensus::default(),
            widest_texels: 0.0,
            tints: Vec::new(),
            marks: Vec::new(),
            contested: 0,
            build_ms: 0.0,
        }
    }
}

/// Where one selected territory is, in base-map pixels, so `crate::callout` can
/// put a caption on the end of a leader from it.
///
/// `None` for a territory with no anchor and no live kernel: it has a layer and
/// a hue, because the field sampler was handed one, but there is nothing on the
/// map for a leader to point at.
fn mark_of(
    base: &BaseMap,
    thread: &polis_world::Thread,
    territory: &Territory,
) -> Option<CloudMark> {
    let at = base.to_map(territory.anchor().or(territory.centre_of_mass)?);
    let px = base.map_px_per_world();
    let mut bounds = Rect::NOTHING;
    let mut span = (f32::INFINITY, f32::NEG_INFINITY);
    for kernel in territory.kernels.iter().filter(|k| k.weight > 0.0) {
        let radius = (kernel.radius * px).max(1.0);
        let centre = base.to_map(kernel.centre);
        bounds = bounds.union(Rect::from_center_size(
            centre,
            egui::Vec2::splat(radius * 2.0),
        ));
        // The kernel's support is a disc, so its width at the anchor's height is
        // a chord and this is exact. A kernel that does not reach that height
        // contributes nothing, which is the whole point: a lobe sitting above or
        // below the anchor must not drag the attachment out to its own edge.
        let chord = radius.mul_add(radius, -((centre.y - at.y) * (centre.y - at.y)));
        if chord > 0.0 {
            let half = chord.sqrt();
            span.0 = span.0.min(centre.x - half);
            span.1 = span.1.max(centre.x + half);
        }
    }
    if !bounds.is_positive() {
        return None;
    }
    Some(CloudMark {
        thread: thread.id.clone(),
        layer: thread.layer,
        tint: thread.tint,
        at,
        bounds,
        // An anchor is a kernel centre, so a chord through it always exists —
        // but `centre_of_mass` is a mean and can fall in the gap between two
        // lobes, where no kernel reaches. Then the box is the honest answer.
        span: if span.0 <= span.1 {
            span
        } else {
            (bounds.min.x, bounds.max.x)
        },
    })
}

/// Uploads an image into an existing handle, or makes one.
///
/// `handle.set` rather than a fresh `load_texture` per frame: the tween repaints
/// this layer every frame it is moving, and allocating a texture id per frame
/// leaks one per frame.
fn upload(
    ctx: &egui::Context,
    handle: &mut Option<egui::TextureHandle>,
    name: &str,
    image: egui::ColorImage,
    options: egui::TextureOptions,
) {
    match handle {
        Some(handle) => handle.set(image, options),
        None => *handle = Some(ctx.load_texture(name, image, options)),
    }
}

/// The reach of a bridge kernel, in world units, before the base map's scale.
///
/// Narrow on purpose. A bridge is a band, not a claim: it should reach the
/// outermost iso level and stop, so the connection reads without the ground
/// between two lobes being coloured as territory. PRD §6.4 is explicit that the
/// honest drawing is *"two lobes and a thin connecting band"* and **not** a
/// shape that "would falsely claim the empty space between".
const BRIDGE_RADIUS: f32 = 3.0;

/// Weight of one bridge kernel.
///
/// [`polis_render::live::CLOUD_ISO`] is `[0.55, 1.60, 3.20]`, so a chain of
/// kernels at this weight sums into the first band and nowhere near the second.
/// The band is therefore always the faintest level the notation has, whatever
/// the lobes either side of it are doing.
const BRIDGE_WEIGHT: f32 = 0.30;

/// How far apart two kernels have to be to count as separate lobes, as a
/// multiple of their own reach.
const LOBE_SEPARATION: f32 = 2.5;

/// Joins one thread's separated lobes with a thin band of low-weight kernels.
///
/// PRD §6.4 promises that a thread working in two places is drawn as *"two lobes
/// and a thin connecting band"*. The field gives the lobes for free — they are
/// just kernels — but it does **not** give the band: a Gaussian falls off fast,
/// so two lobes further apart than a few bandwidths sum to nothing in between
/// and the cloud silently becomes two islands. On a map where every thread is a
/// colour that is indistinguishable from two unrelated agents, which inverts the
/// one thing the operator asked for: *"even if the line gets thinner in the
/// middle, it should be clear that this is one entity working on both of these
/// things."*
///
/// So the band is made explicit. Kernels are clustered by proximity; every
/// cluster after the first is joined to the heaviest one by a chain of
/// [`BRIDGE_WEIGHT`] kernels along the straight line between their centroids.
/// They go through the same field, the same thresholds and the same hatch as
/// real evidence, so the band cannot introduce a tone, a level or an alpha of
/// its own — it can only ever be the outermost band, which is what "thinner in
/// the middle" means in this notation.
fn bridge_lobes(territory: &Territory) -> Vec<(polis_layout::Point, f32)> {
    let kernels = &territory.kernels;
    if kernels.len() < 2 {
        return Vec::new();
    }
    // Greedy single-pass clustering, in the kernels' own order, so the same
    // field yields the same band on every run (PRD §7.4).
    let mut centres: Vec<(polis_layout::Point, f32)> = Vec::new();
    for k in kernels {
        if k.weight <= 0.0 {
            continue;
        }
        let reach = (k.radius * LOBE_SEPARATION).max(f32::EPSILON);
        let found = centres.iter_mut().find(|(c, _)| {
            let dx = c.x - k.centre.x;
            let dy = c.y - k.centre.y;
            dx.mul_add(dx, dy * dy) <= reach * reach
        });
        match found {
            Some((c, w)) => {
                // Weighted running centroid, so a cluster's centre is where its
                // mass is rather than where its first kernel happened to land.
                let total = *w + k.weight;
                c.x = (c.x * *w + k.centre.x * k.weight) / total;
                c.y = (c.y * *w + k.centre.y * k.weight) / total;
                *w = total;
            }
            None => centres.push((k.centre, k.weight)),
        }
    }
    if centres.len() < 2 {
        return Vec::new();
    }
    let Some((anchor, _)) = centres
        .iter()
        .copied()
        .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
    else {
        return Vec::new();
    };
    let mut band = Vec::new();
    for (centre, _) in &centres {
        let dx = centre.x - anchor.x;
        let dy = centre.y - anchor.y;
        let span = dx.hypot(dy);
        if span <= f32::EPSILON {
            continue;
        }
        // One kernel per bandwidth keeps the chain continuous without paying for
        // a kernel per texel.
        let steps = (span / BRIDGE_RADIUS).ceil().min(64.0) as usize;
        for i in 1..steps {
            let f = i as f32 / steps as f32;
            band.push((
                polis_layout::Point::new(dx.mul_add(f, anchor.x), dy.mul_add(f, anchor.y)),
                BRIDGE_WEIGHT,
            ));
        }
    }
    band
}

/// [`live::cloud_ink_stack`], with the asked-about thread's layer lifted.
///
/// The stack is emitted layer by layer here rather than in one call because
/// emphasis is a property of **one** territory and `cloud_ink_stack` knows only
/// the sky. Every tone that layer emits — the contour, the hatch, and the ground
/// under them — is multiplied by [`LIT_GAIN`], so the cloud brightens whole and
/// keeps its own ladder: the same gain on all three bands leaves *"core versus
/// fringe"* reading inside a lit cloud exactly as it does outside one.
///
/// Window-only, which is why it is here and not in `polis_render::live`: a
/// recorded frame has no pointer, so it has nothing to light. The mark is still
/// that module's — this scales what it emits and adds no stroke of its own.
fn ink_stack(
    stack: &live::CloudStack,
    origin: [usize; 2],
    veil: f64,
    lit: Option<u16>,
    emit: &mut impl FnMut(usize, usize, [u8; 3], f64),
) {
    for layer in &stack.layers {
        let asked = lit.is_some_and(|id| id == layer.thread);
        live::cloud_ink(layer, origin, veil, &mut |x, y, tone, alpha| {
            emit(x, y, if asked { brighten(tone) } else { tone }, alpha);
        });
    }
}

/// One tone at [`LIT_GAIN`]. A hue is a ratio between channels and a scalar
/// keeps it; the cloud tones are low enough that nothing clips.
fn brighten(tone: [u8; 3]) -> [u8; 3] {
    tone.map(|c| (f32::from(c) * LIT_GAIN).round().min(255.0) as u8)
}

impl Clouds {
    /// Advances the field and returns what to draw.
    ///
    /// `cap` is PRD §10.4's cloud cap: *"Forty threads means forty systems and
    /// the map vanishes under haze."* Which territories survive it is
    /// [`polis_world::territory::select_clouds`]'s decision, not this module's;
    /// what it decided about the ones it withheld is [`Clouds::census`].
    ///
    /// `dt` is **presentation** seconds. The tween is an animation, so it runs
    /// on the clock the viewer experiences and not on the world's — the same
    /// split `polis_render::frame` makes, and for the same reason: a cloud
    /// should take the same third of a second to arrive whether a replay is at
    /// 1× or 64×.
    pub fn update(
        &mut self,
        ctx: &egui::Context,
        base: &BaseMap,
        snapshot: &WorldSnapshot,
        cap: usize,
        veil: f64,
        lit: Option<&ThreadId>,
        dt: f32,
    ) -> Option<(&egui::TextureHandle, &egui::TextureHandle, Rect)> {
        // A veil the operator has just moved is a repaint, not a new world: the
        // field is unchanged and only the ink over it differs, so the tween is
        // left alone and the texture is rebuilt on the next step.
        if (self.veil - veil).abs() > f64::EPSILON {
            self.veil = veil;
            self.settled = false;
        }
        // Asking about a thread is a repaint for exactly the same reason: the
        // field is where it was and only one layer's ink differs. One rebuild
        // when the pointer arrives on a card and one when it leaves, not one
        // per frame while it rests there.
        if self.lit.as_ref() != lit {
            self.lit = lit.cloned();
            self.settled = false;
        }
        if self.generation != snapshot.generation || self.cap != cap {
            self.target = Self::target_field(
                base,
                snapshot,
                cap,
                &mut self.census,
                &mut self.widest_texels,
                &mut self.tints,
                &mut self.marks,
            );
            self.generation = snapshot.generation;
            self.cap = cap;
            self.settled = false;
        }
        if !self.settled {
            self.advance(ctx, f64::from(dt));
        }
        Some((self.body.as_ref()?, self.ink.as_ref()?, self.rect))
    }

    /// Whether the layer still owes the window a frame.
    ///
    /// The window repaints on its own while agents move; this is what keeps a
    /// cloud arriving smoothly in a world that is otherwise still.
    #[must_use]
    pub fn animating(&self) -> bool {
        !self.settled
    }

    /// Drops the tween. Called when the replay is scrubbed, because easing
    /// across a cut would draw a cloud sliding through the city.
    pub fn reset(&mut self) {
        self.tween.reset();
        self.generation = u64::MAX;
        self.settled = false;
    }

    /// What the layer decided and why, for the status bar.
    ///
    /// `screen_px_per_map_px` is [`crate::camera::Camera::scale`]. It is asked
    /// for rather than remembered because [`CloudCensus::widest_px`] is in
    /// **output** pixels — [`CloudCensus::sub_pixel`] tests it against a minimum
    /// stroke width, and `shown: 6, widest: 0.8 px` is the whole diagnosis of an
    /// empty sky — while this module's own frame is the fixed texel grid that
    /// exists precisely so the tween never has to be re-registered when the
    /// camera moves. One texel is 1.5625 base-map pixels for the life of the
    /// city, and a base-map pixel is a fraction of a screen pixel or several of
    /// them, depending entirely on the scroll wheel.
    #[must_use]
    pub fn census(&self, screen_px_per_map_px: f32) -> CloudCensus {
        let map_px_per_texel = BASE_MAP_PIXELS as f64 / TEXELS as f64;
        CloudCensus {
            widest_px: self.widest_texels * map_px_per_texel * f64::from(screen_px_per_map_px),
            ..self.census
        }
    }

    /// Which clouds are on the map, whose they are, and where.
    ///
    /// The **target** selection, not the tween's current state: a cloud that has
    /// just entered the selection is fading in and is already worth naming, and
    /// one that has just left stops being named on the frame it stops being
    /// true rather than a third of a second later.
    #[must_use]
    pub fn marks(&self) -> &[CloudMark] {
        &self.marks
    }

    /// The visible territories' kernels, in texel coordinates.
    fn target_field(
        base: &BaseMap,
        snapshot: &WorldSnapshot,
        cap: usize,
        census_out: &mut CloudCensus,
        widest_out: &mut f64,
        tints_out: &mut Vec<u8>,
        marks_out: &mut Vec<CloudMark>,
    ) -> Option<CloudField> {
        let pairs: Vec<(&polis_world::Thread, &Territory)> = snapshot
            .threads
            .iter()
            .map(|thread| (thread, &thread.territory))
            .collect();
        // `select_clouds` rather than `visible_clouds`: the same decision by the
        // same function — `visible_clouds` is a wrapper around it — but the
        // wrapper throws away the three counts that say why a thread got no
        // cloud, and throwing them away is what left the window unable to
        // explain an empty sky. The attention list goes with it, which the
        // wrapper also dropped: PRD §10.4 ranks *"threads with an active
        // attention state"* first, so without it the window's cap and the
        // headless renderer's could pick a different five out of the same world.
        let selection = territory::select_clouds(
            &pairs,
            &snapshot.attention,
            CloudPolicy::default().with_cap(cap),
        );
        let visible = selection.visible;
        // Whose each cloud is. `select_clouds` returns territories, and both the
        // layer a kernel carries and the hue it is painted in are the
        // *thread's*, so each is matched back to the pair it came from.
        //
        // The layer must stay one per thread, or two territories are summed as
        // one and PRD §6.4's crowd signal goes quiet — which is why this is
        // `Thread::layer` and not `Thread::tint` (twelve slots, then it
        // repeats). And it must stay the *same* one from frame to frame, which
        // is why it is not this territory's index in `visible`: that list is
        // sorted by `last_activity`, so two agents working at once swap places
        // on every tool call, and the tween would then ease each cloud's shape
        // toward the other's and repaint both in the other's hue.
        let mut orphan = u16::MAX;
        let mut layers: Vec<u16> = Vec::with_capacity(visible.len());
        tints_out.clear();
        for territory in &visible {
            // An unmatched territory cannot happen — `visible` is borrowed from
            // `pairs` — but if it did it gets a layer nothing else is on rather
            // than sharing layer 0 with a real thread.
            let found = pairs.iter().find(|(_, t)| std::ptr::eq(*t, *territory));
            let (layer, tint) = if let Some((thread, _)) = found {
                (thread.layer, thread.tint)
            } else {
                let layer = orphan;
                orphan = orphan.saturating_sub(1);
                (layer, live::NO_TINT)
            };
            layers.push(layer);
            // Indexed by layer, because `CloudField::stack` looks a layer's tint
            // up by the id the layer carries.
            if tints_out.len() <= layer as usize {
                tints_out.resize(layer as usize + 1, live::NO_TINT);
            }
            tints_out[layer as usize] = tint;
        }

        // Where each cloud is and whose it is, for `crate::callout`. This loop
        // rather than the one above because a mark needs the thread's id as
        // well as its hue, and because a territory with no anchor and no live
        // kernel has a layer and a tint but nothing to point a leader at.
        marks_out.clear();
        for territory in &visible {
            let Some((thread, _)) = pairs.iter().find(|(_, t)| std::ptr::eq(*t, *territory)) else {
                continue;
            };
            marks_out.extend(mark_of(base, thread, territory));
        }
        // Where one thread's field has separated into lobes, bridge them.
        // See `bridge_lobes`.
        let mut bridges: Vec<Vec<(polis_layout::Point, f32)>> = Vec::with_capacity(visible.len());
        for territory in &visible {
            bridges.push(bridge_lobes(territory));
        }

        // Base-map pixels to texels: one fixed factor for the life of the city,
        // which is what lets the tween interpolate without re-registering.
        let per_texel = TEXELS as f32 / BASE_MAP_PIXELS as f32;
        let scale = base.map_px_per_world() * per_texel;
        let mut kernels: Vec<CloudKernel> = Vec::new();
        for (rank, territory) in visible.iter().enumerate() {
            for kernel in &territory.kernels {
                if kernel.weight <= 0.0 {
                    continue;
                }
                let at = base.to_map(kernel.centre);
                kernels.push(CloudKernel {
                    at: [f64::from(at.x * per_texel), f64::from(at.y * per_texel)],
                    // PRD §6.4's bandwidth, already `base / sqrt(effective_n)`
                    // and already clamped by `polis_world`. The renderer must not
                    // second-guess it: the width of the cloud *is* the width of
                    // the claim, and it is the only thing that makes a thinly
                    // evidenced territory look thinly evidenced.
                    radius: f64::from((kernel.radius * scale).max(2.0)),
                    weight: f64::from(kernel.weight),
                    thread: layers[rank],
                });
            }
            // The band. Bridge kernels are added at the same scale as the real
            // ones, so the field sums them exactly as it sums evidence — the
            // connection is drawn by the same machinery that draws the lobes,
            // which is why it cannot lift the base map or invent a band level
            // of its own.
            for (centre, weight) in &bridges[rank] {
                let at = base.to_map(*centre);
                kernels.push(CloudKernel {
                    at: [f64::from(at.x * per_texel), f64::from(at.y * per_texel)],
                    radius: f64::from((BRIDGE_RADIUS * scale).max(2.0)),
                    weight: f64::from(*weight),
                    thread: layers[rank],
                });
            }
        }
        *widest_out = kernels.iter().map(|k| k.radius).fold(0.0f64, f64::max);
        *census_out = CloudCensus {
            shown: visible.len(),
            kernels: kernels.len(),
            // Filled in by `Clouds::census`, where the camera is known.
            widest_px: 0.0,
            unplaced: selection.unplaced,
            dormant: selection.dormant,
            capped: selection.capped,
        };
        CloudField::sample(&kernels, TEXELS, TEXELS)
    }

    /// One tween step, and the repaint it implies.
    fn advance(&mut self, ctx: &egui::Context, dt: f64) {
        let started = std::time::Instant::now();
        let Some(field) = self
            .tween
            .advance(self.target.clone(), dt, live::CLOUD_TWEEN_RATE)
        else {
            self.body = None;
            self.ink = None;
            self.rect = Rect::NOTHING;
            self.contested = 0;
            self.settled = self.target.is_none();
            self.build_ms = started.elapsed().as_secs_f64() * 1_000.0;
            return;
        };

        // One banded map per territory rather than one for the sky. See
        // `polis_render::live::CloudField::stack`: a summed map labels each
        // pixel with whoever is densest there, which draws a thread sharing
        // ground with a busier one as though it were not there at all.
        let stack = field.stack(&self.tints);
        let Some((x0, y0, width, height)) = stack.rect() else {
            self.body = None;
            self.ink = None;
            self.rect = Rect::NOTHING;
            self.contested = 0;
            self.settled = self.target.is_none();
            self.build_ms = started.elapsed().as_secs_f64() * 1_000.0;
            return;
        };
        self.contested = stack.contested();

        // The notation itself, emitted by `polis_render::live` so the window and
        // the headless renderer cannot disagree about a single stroke. One pass
        // over the stack fills both images: an opaque emission is a mark and a
        // translucent one is the body under it, which is the whole of the split.
        //
        // Premultiplied, because that is what `Color32` is and what a linear
        // filter needs to be correct at the silhouette — interpolating straight
        // alpha against transparent black darkens every edge it touches.
        let mut body = vec![[0.0f32; 4]; width * height];
        let mut ink = vec![Color32::TRANSPARENT; width * height];
        // Which layer, if any, is the operator's own question. By layer and
        // not by hue: `CloudMark::layer` says why.
        let lit = self
            .lit
            .as_ref()
            .and_then(|id| self.marks.iter().find(|m| &m.thread == id))
            .map(|m| m.layer);
        ink_stack(
            &stack,
            [x0, y0],
            self.veil,
            lit,
            &mut |x, y, tone, alpha| {
                if x >= width || y >= height {
                    return;
                }
                let i = y * width + x;
                if alpha >= 1.0 {
                    ink[i] = Color32::from_rgb(tone[0], tone[1], tone[2]);
                    return;
                }
                // Source-over in premultiplied form, so two territories sharing
                // ground read as two veils and not as whichever was painted
                // last.
                let a = alpha as f32;
                let dst = &mut body[i];
                for (c, src) in dst.iter_mut().zip(tone) {
                    *c = (f32::from(src) / 255.0).mul_add(a, *c * (1.0 - a));
                }
                dst[3] = a.mul_add(1.0 - dst[3], dst[3]);
            },
        );
        let body: Vec<Color32> = body
            .iter()
            .map(|p| {
                Color32::from_rgba_premultiplied(
                    (p[0] * 255.0).round() as u8,
                    (p[1] * 255.0).round() as u8,
                    (p[2] * 255.0).round() as u8,
                    (p[3] * 255.0).round() as u8,
                )
            })
            .collect();
        upload(
            ctx,
            &mut self.body,
            "polis-cloud-body",
            egui::ColorImage::new([width, height], body),
            egui::TextureOptions::LINEAR,
        );
        upload(
            ctx,
            &mut self.ink,
            "polis-cloud-marks",
            egui::ColorImage::new([width, height], ink),
            egui::TextureOptions::NEAREST,
        );

        let per_texel = BASE_MAP_PIXELS as f32 / TEXELS as f32;
        self.rect = Rect::from_min_max(
            egui::pos2(x0 as f32 * per_texel, y0 as f32 * per_texel),
            egui::pos2(
                (x0 + width) as f32 * per_texel,
                (y0 + height) as f32 * per_texel,
            ),
        );
        self.settled = match &self.target {
            Some(t) => t.aligned_with(field) && Self::close(&t.density, &field.density),
            None => false,
        };
        self.build_ms = started.elapsed().as_secs_f64() * 1_000.0;
    }

    /// Whether the tween has arrived, to within a hundredth of the target's own
    /// peak. Relative rather than absolute, so a faint territory is allowed to
    /// settle as readily as a dense one.
    fn close(target: &[f32], now: &[f32]) -> bool {
        let peak = target.iter().copied().fold(0.0f32, f32::max).max(1e-6);
        target
            .iter()
            .zip(now.iter())
            .all(|(a, b)| (a - b).abs() <= peak * SETTLED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polis_render::raster::Canvas;

    /// The measurement canvas's clear value: transparent, and distinguishable
    /// from every cloud tone. The window itself no longer needs one — it
    /// composites into an alpha channel rather than keying on a colour.
    const NOTHING: [u8; 3] = [0, 0, 0];
    use polis_render::live::CLOUD_ISO;
    use std::collections::HashSet;

    fn field(splats: &[([f64; 2], f64, f64, u16)]) -> CloudField {
        let kernels: Vec<CloudKernel> = splats
            .iter()
            .map(|(at, radius, weight, thread)| CloudKernel {
                at: *at,
                radius: *radius,
                weight: *weight,
                thread: *thread,
            })
            .collect();
        CloudField::sample(&kernels, 400, 400).expect("a field")
    }

    /// The shipped path: a stack, one layer per territory, on the tints given.
    ///
    /// The canvas is the whole field rectangle rather than the stack's own, so
    /// two paintings of two different worlds can be compared pixel for pixel.
    fn paint_with(field: &CloudField, tints: &[u8]) -> Canvas {
        let stack = field.stack(tints);
        let mut canvas = Canvas::new(field.width, field.height, NOTHING);
        // Marks only. Every assertion below counts inked texels against the
        // notation's own sparseness rule, and a veil inks every banded texel by
        // definition — measuring the body here would measure the setting rather
        // than the marks. The body's own cost is asserted in
        // `polis-render/tests/cloud_measure.rs`, which is where the disturbance
        // budget lives.
        live::paint_cloud_stack_into(&mut canvas, &stack, [field.x0, field.y0], 0.0);
        canvas
    }

    fn paint(field: &CloudField) -> Canvas {
        paint_with(field, &[])
    }

    fn inked(canvas: &Canvas) -> usize {
        canvas
            .pixels
            .as_chunks::<3>()
            .0
            .iter()
            .filter(|p| **p != NOTHING)
            .count()
    }

    /// `docs/verified/gpu-stack.md` §5's central finding, on the CPU:
    /// overlapping kernels **sum**, so the field is unbounded and the thresholds
    /// cannot be fractions of a maximum.
    #[test]
    fn overlapping_kernels_sum_past_one() {
        let one = field(&[([200.0, 200.0], 60.0, 1.0, 0)]);
        assert!(
            (one.peak() - 1.0).abs() < 0.02,
            "one full-weight kernel peaks at ~1, got {}",
            one.peak()
        );
        let three = field(&[
            ([200.0, 200.0], 60.0, 1.0, 0),
            ([206.0, 200.0], 60.0, 1.0, 0),
            ([203.0, 206.0], 60.0, 1.0, 0),
        ]);
        assert!(
            three.peak() > 2.5,
            "three overlapping kernels summed to {}",
            three.peak()
        );
    }

    /// The trap the probe caught by measuring rather than by looking: with the
    /// thresholds normalised by the observed maximum, one hot cluster flattens
    /// every other territory into the fringe band. A fixed reference does not.
    #[test]
    fn a_hot_cluster_does_not_flatten_the_rest_of_the_map() {
        let f = field(&[
            ([80.0, 80.0], 40.0, 1.0, 0),
            ([300.0, 300.0], 40.0, 3.0, 1),
            ([308.0, 300.0], 40.0, 3.0, 1),
        ]);
        let lonely = f.at(80.0, 80.0);
        let peak = f.peak();
        assert!(peak > 5.0, "the cluster is hot: {peak}");
        assert!(
            f64::from(lonely) >= CLOUD_ISO[0],
            "the lone territory lost its band at the fixed reference: {lonely}"
        );
        assert!(
            f64::from(lonely / peak) < CLOUD_ISO[0],
            "and would have vanished had the field been normalised by {peak}"
        );
    }

    /// PRD §10.4: 2–3 levels, hard steps — and PRD §10.3: a cloud is a set of
    /// marks the city shows through.
    ///
    /// The regression this guards against is somebody replacing the `if` chain
    /// with a `smoothstep`, or the band with a fill. A fill leaves no
    /// transparent texel inside its own rectangle; this one leaves most of them.
    #[test]
    fn the_layer_draws_opaque_marks_and_leaves_the_rest_transparent() {
        let f = field(&[
            ([160.0, 200.0], 90.0, 1.4, 0),
            ([240.0, 200.0], 90.0, 1.4, 0),
            ([200.0, 200.0], 50.0, 2.0, 0),
        ]);
        let canvas = paint(&f);
        let distinct: HashSet<[u8; 3]> = canvas.pixels.as_chunks::<3>().0.iter().copied().collect();
        assert!(
            distinct.len() <= 4,
            "transparent plus at most three bands, got {}",
            distinct.len()
        );
        let inked = inked(&canvas);
        let banded = f.stack(&[]).flattened().map_or(0, |b| {
            b.cells
                .iter()
                .fold(0usize, |n, b| n + usize::from(*b != live::NO_BAND))
        });
        assert!(inked > 200, "the cloud drew almost nothing: {inked} px");
        assert!(
            inked * 100 / banded.max(1) <= 35,
            "the cloud inked {}% of its own banded region: that is a fill",
            inked * 100 / banded.max(1)
        );
    }

    /// Multi-lobed shapes come free from field addition (PRD §6.4): an agent
    /// working in `auth` with one worker in `tests` gets two lobes and a thin
    /// connecting band, not a bounding box over the empty space between.
    #[test]
    fn two_separated_kernels_make_two_lobes_and_not_one_blob() {
        let f = field(&[
            ([100.0, 200.0], 45.0, 1.0, 0),
            ([300.0, 200.0], 45.0, 1.0, 0),
        ]);
        assert!(f64::from(f.at(100.0, 200.0)) >= CLOUD_ISO[0], "left lobe");
        assert!(f64::from(f.at(300.0, 200.0)) >= CLOUD_ISO[0], "right lobe");
        assert!(
            f64::from(f.at(200.0, 200.0)) < CLOUD_ISO[0],
            "the empty space between them is claimed: {}",
            f.at(200.0, 200.0)
        );
    }

    /// PRD §6.4: two territories overlapping is a denser region, *"which is
    /// exactly the contention signal"* — and the layer says which, not just how
    /// dense.
    #[test]
    fn two_territories_on_one_place_are_marked_as_contested() {
        let solo = field(&[
            ([190.0, 200.0], 70.0, 1.0, 0),
            ([210.0, 200.0], 70.0, 1.0, 0),
        ]);
        let pair = field(&[
            ([190.0, 200.0], 70.0, 1.0, 0),
            ([210.0, 200.0], 70.0, 1.0, 1),
        ]);
        assert_eq!(solo.stack(&[]).contested(), 0, "one territory, no contest");
        assert!(
            pair.stack(&[]).contested() > 0,
            "two territories on one place left no contested ground"
        );
        assert_ne!(
            paint(&solo).pixels,
            paint(&pair).pixels,
            "contested ground is drawn exactly like uncontested ground"
        );
    }

    /// The complaint this whole layer was rebuilt for: *"patterns absorb each
    /// other"*.
    ///
    /// Two threads on the same ground, drawn from the same field. The old
    /// notation summed them into one band map and gave every pixel to whoever
    /// was denser, so the quieter territory's hue did not appear anywhere its
    /// neighbour reached — over the overlap it was not dimmed, it was gone. The
    /// stack draws both.
    #[test]
    fn a_territory_is_still_drawn_where_a_denser_one_overlaps_it() {
        // Slot 0 is rose and slot 6 is teal — opposite ends of the hue ring, so
        // the assertion is about presence and not about a near miss.
        let tints = [0u8, 6u8];
        // The second thread is much the denser of the two, which is exactly the
        // case the argmax used to erase.
        let f = field(&[
            ([200.0, 200.0], 80.0, 1.0, 0),
            ([215.0, 200.0], 80.0, 3.0, 1),
            ([225.0, 205.0], 80.0, 3.0, 1),
        ]);
        let canvas = paint_with(&f, &tints);

        // Whose ink is whose: a band tone put through each thread's hue.
        let mine: HashSet<[u8; 3]> = live::CLOUD_TONES
            .iter()
            .map(|t| live::thread_ink(tints[0], *t))
            .collect();
        let theirs: HashSet<[u8; 3]> = live::CLOUD_TONES
            .iter()
            .map(|t| live::thread_ink(tints[1], *t))
            .collect();
        assert!(
            mine.is_disjoint(&theirs),
            "the two hues are the same ink; this test cannot see anything"
        );

        // Only the ground the *denser* thread bands, so "still drawn" means
        // drawn underneath it and not merely drawn somewhere else on the map.
        let stack = f.stack(&tints);
        let over = stack
            .layers
            .iter()
            .find(|l| l.tint.first() == Some(&tints[1]))
            .expect("the denser thread has a layer");
        let (mut quiet, mut loud) = (0usize, 0usize);
        for y in 0..over.height {
            for x in 0..over.width {
                if over.cells[y * over.width + x] == live::NO_BAND {
                    continue;
                }
                let (cx, cy) = (x + over.x0 - f.x0, y + over.y0 - f.y0);
                let p = canvas.pixels.as_chunks::<3>().0[cy * canvas.width + cx];
                quiet += usize::from(mine.contains(&p));
                loud += usize::from(theirs.contains(&p));
            }
        }
        assert!(loud > 0, "the denser thread drew nothing on its own ground");
        assert!(
            quiet > 100,
            "the quieter thread put {quiet} px inside the denser one's territory: \
             it has been absorbed again"
        );
    }

    /// And the absorption is not paid for in fog: PRD §10.3's budget survives
    /// the second layer, because each layer's hatch opens up by how many
    /// territories share the pixel (`CLOUD_SHARE_SPACING`).
    #[test]
    fn two_layers_over_one_place_cost_about_one_layer_of_ink() {
        let solo = field(&[
            ([200.0, 200.0], 80.0, 1.0, 0),
            ([215.0, 200.0], 80.0, 1.0, 0),
        ]);
        let pair = field(&[
            ([200.0, 200.0], 80.0, 1.0, 0),
            ([215.0, 200.0], 80.0, 1.0, 1),
        ]);
        let one = inked(&paint(&solo));
        let two = inked(&paint(&pair));
        assert!(one > 200 && two > 200, "{one} and {two} px is not a cloud");
        // Two contours where there was one, and two weaves where there was one,
        // so it is not the same number — but it is the same order, not double.
        assert!(
            two * 100 / one <= 175,
            "splitting one territory into two inked {}% of what one did",
            two * 100 / one
        );
    }

    /// The operator's own rule for *"which shape on the map is this row?"*:
    /// hovering a card lights that thread's cloud instead of drawing a line to
    /// it — see [`LIT_GAIN`].
    ///
    /// Two claims, and the second is the one that makes it a highlight rather
    /// than a brightness knob: the asked-about layer comes out at exactly the
    /// gain, and every other layer comes out **texel for texel identical**. The
    /// halves are separated in x so a texel's own coordinate says whose it is.
    #[test]
    fn asking_about_one_thread_lights_that_cloud_and_leaves_the_sky_alone() {
        let f = field(&[
            ([100.0, 200.0], 45.0, 1.5, 0),
            ([300.0, 200.0], 45.0, 1.5, 1),
        ]);
        let stack = f.stack(&[3, 7]);
        assert_eq!(stack.layers.len(), 2, "one layer per territory");
        // Layer, not hue: the two would be the same number here, and are not on
        // a world with more than twelve threads. See `CloudMark::layer`.

        // Tone times coverage, which is the light a layer actually puts on the
        // map: a mark counts fully and a body texel counts its own veil.
        let light = |lit: Option<u16>| {
            let (mut left, mut right) = (0.0f64, 0.0f64);
            ink_stack(
                &stack,
                [f.x0, f.y0],
                live::CLOUD_VEIL,
                lit,
                &mut |x, _y, tone, alpha| {
                    let lit = tone.iter().map(|c| f64::from(*c)).sum::<f64>() * alpha;
                    if x + f.x0 < 200 {
                        left += lit;
                    } else {
                        right += lit;
                    }
                },
            );
            (left, right)
        };

        let (ambient_left, ambient_right) = light(None);
        assert!(
            ambient_left > 0.0 && ambient_right > 0.0,
            "the fixture drew no cloud: {ambient_left} and {ambient_right}"
        );

        let (left, right) = light(Some(0));
        assert!(
            (left / ambient_left - f64::from(LIT_GAIN)).abs() < 0.02,
            "the asked-about cloud is drawn at {:.3}, not LIT_GAIN",
            left / ambient_left
        );
        assert!(
            (right - ambient_right).abs() < 1e-6,
            "lighting one thread moved another one's cloud: {right} against {ambient_right}"
        );

        // And the same asked of the other thread, because "only this one" is a
        // claim about whichever one is asked about.
        let (left, right) = light(Some(1));
        assert!(
            (right / ambient_right - f64::from(LIT_GAIN)).abs() < 0.02,
            "the second thread's cloud is drawn at {:.3}, not LIT_GAIN",
            right / ambient_right
        );
        assert!(
            (left - ambient_left).abs() < 1e-6,
            "lighting the second thread moved the first one's cloud"
        );

        // A hue slot no cloud on screen is using is an ambient sky. The rail can
        // hold a row whose territory was withheld by the cloud cap, and pointing
        // at it must light nothing rather than the nearest thing.
        assert_eq!(
            light(Some(live::NO_LAYER)),
            (ambient_left, ambient_right),
            "a thread with no cloud lit one anyway"
        );
    }

    /// Each layer weaves on its own axis, so two overlapping territories are two
    /// textures rather than one (`CLOUD_WEAVES`).
    #[test]
    fn two_threads_do_not_share_a_hatch_axis() {
        let f = field(&[
            ([200.0, 200.0], 80.0, 1.5, 0),
            ([215.0, 200.0], 80.0, 1.5, 1),
        ]);
        let stack = f.stack(&[0, 1]);
        assert_eq!(stack.layers.len(), 2, "one layer per territory");
        assert_ne!(
            stack.layers[0].weave, stack.layers[1].weave,
            "both territories hatch along the same axis: they read as one texture"
        );
    }

    /// The stack is painted urgent-last, so the thread `select_clouds` ranked
    /// first is the one on top where two marks land on the same pixel.
    #[test]
    fn the_first_ranked_thread_is_painted_last() {
        let f = field(&[
            ([200.0, 200.0], 80.0, 1.5, 0),
            ([215.0, 200.0], 80.0, 1.5, 1),
        ]);
        let stack = f.stack(&[0, 1]);
        assert_eq!(
            stack.layers.last().map(|l| l.tint[0]),
            Some(0),
            "rank 0 is not the last layer painted"
        );
    }

    #[test]
    fn an_empty_world_draws_no_cloud_at_all() {
        assert!(CloudField::sample(&[], 400, 400).is_none());
    }
}

#[cfg(test)]
mod bridge_tests {
    use super::{bridge_lobes, BRIDGE_WEIGHT};
    use polis_layout::Point;
    use polis_world::territory::{Kernel, Territory};
    use std::time::Instant;

    fn territory_with(centres: &[(f32, f32)]) -> Territory {
        let now = Instant::now();
        let mut t = Territory::default();
        t.kernels = centres
            .iter()
            .map(|(x, y)| Kernel {
                centre: Point::new(*x, *y),
                radius: 4.0,
                weight: 1.0,
                at: now,
            })
            .collect();
        t
    }

    /// A focused thread is one lobe, and one lobe needs no band. Drawing one
    /// would be inventing a connection where there is nothing to connect.
    #[test]
    fn one_lobe_gets_no_band() {
        let t = territory_with(&[(0.0, 0.0), (2.0, 1.0), (1.0, 2.0)]);
        assert!(bridge_lobes(&t).is_empty());
    }

    /// The case the operator asked for: one agent working in two places stays
    /// visibly one entity.
    #[test]
    fn two_lobes_are_joined_and_the_band_lies_between_them() {
        let t = territory_with(&[(0.0, 0.0), (1.0, 0.0), (100.0, 0.0), (101.0, 0.0)]);
        let band = bridge_lobes(&t);
        assert!(!band.is_empty(), "two separated lobes must be joined");
        for (p, w) in &band {
            assert!(
                (*w - BRIDGE_WEIGHT).abs() < f32::EPSILON,
                "the band is always the faintest level the notation has"
            );
            assert!(
                p.x > -1.0 && p.x < 102.0 && p.y.abs() < 1.0,
                "the band runs between the lobes, not past them: {p:?}"
            );
        }
        // Every band kernel is lighter than any real one, so the connection can
        // never read as heavily as the places being connected.
        assert!(band.iter().all(|(_, w)| *w < 1.0));
    }

    /// Three lobes join to the heaviest, not in a chain — a chain would route
    /// the band through a lobe that happened to be in the middle and imply an
    /// order the evidence does not have.
    #[test]
    fn three_lobes_all_reach_the_heaviest() {
        let mut t = territory_with(&[(0.0, 0.0), (100.0, 0.0), (0.0, 100.0)]);
        t.kernels[0].weight = 10.0;
        let band = bridge_lobes(&t);
        let to_east = band.iter().any(|(p, _)| p.x > 40.0 && p.y.abs() < 1.0);
        let to_south = band.iter().any(|(p, _)| p.y > 40.0 && p.x.abs() < 1.0);
        assert!(to_east && to_south, "both outliers reach the anchor");
    }

    /// Determinism (PRD §7.4): the same field yields the same band.
    #[test]
    fn the_band_is_deterministic() {
        let t = territory_with(&[(0.0, 0.0), (100.0, 0.0), (0.0, 100.0), (50.0, 50.0)]);
        let a = bridge_lobes(&t);
        let b = bridge_lobes(&t);
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(b.iter()) {
            assert!((x.0.x - y.0.x).abs() < f32::EPSILON && (x.0.y - y.0.y).abs() < f32::EPSILON);
        }
    }
}
