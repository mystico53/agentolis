//! Territory clouds (PRD §10.4).
//!
//! > Discrete iso-contour bands, **2–3 levels, never a continuous blur.**
//! > Continuous gradients turn to mush and you lose the ability to say "that
//! > file is in the core of this thread's work" versus "it's at the fringe."
//!
//! This is the window's half of that layer, and it is deliberately **not** a
//! second implementation. The field, the thresholds, the contour, the hatch, the
//! cross-hatch on contested ground and the tween all come from
//! [`polis_render::live`]; this module maps kernels into a fixed texel frame,
//! hands them over, and uploads what comes back. A visual language with two
//! definitions has none, and the previous version proved it — see below.
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
//! What is drawn instead is what [`polis_render::live::paint_cloud_bands`]
//! draws: a contour stroke on each level's boundary, widest on the outermost
//! because that silhouette is what survives being seen from across the room,
//! plus a hatch whose spacing tightens toward the core, crossed where two
//! territories claim the same ground. Every mark is **opaque** and every other
//! texel is fully transparent, so the city underneath is not merely recoverable,
//! it is untouched — measured at **zero** disturbed pixels, and a median
//! luminance that moves by `0.000` levels, over 192 frames of six real sessions
//! replayed side by side (`polis-render/tests/cloud_measure.rs`).
//!
//! # Nearest, not linear, and that is not a detail
//!
//! A sparse mark stretched with a linear filter is a smear whose edges land at
//! every intermediate alpha, and a half-transparent cloud tone composites into
//! the base map's own contrast band — which is the fill this module just stopped
//! drawing, arriving by the back door. [`egui::TextureOptions::NEAREST`] keeps a
//! mark a mark at every zoom.
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

use eframe::egui::{self, Color32, Rect};
use polis_render::live::{self, CloudCensus, CloudField, CloudKernel, CloudTween, CLOUD_CROWD};
use polis_render::raster::Canvas;
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

/// Transparent, and distinguishable from every cloud tone.
const NOTHING: [u8; 3] = [0, 0, 0];

/// A field within this fraction of its target on every texel has arrived, and
/// the layer stops repainting until the world moves again (PRD §13.1's idle
/// budget: *"< 2 % of one core"*).
const SETTLED: f32 = 0.02;

/// The rasterised cloud layer: one texture, repainted while it is moving.
pub struct Clouds {
    texture: Option<egui::TextureHandle>,
    /// The base-map rectangle the texture covers.
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
            texture: None,
            rect: Rect::NOTHING,
            generation: u64::MAX,
            cap: usize::MAX,
            target: None,
            tween: CloudTween::default(),
            settled: false,
            census: CloudCensus::default(),
            widest_texels: 0.0,
            tints: Vec::new(),
            contested: 0,
            build_ms: 0.0,
        }
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
        dt: f32,
    ) -> Option<(&egui::TextureHandle, Rect)> {
        if self.generation != snapshot.generation || self.cap != cap {
            self.target = Self::target_field(
                base,
                snapshot,
                cap,
                &mut self.census,
                &mut self.widest_texels,
                &mut self.tints,
            );
            self.generation = snapshot.generation;
            self.cap = cap;
            self.settled = false;
        }
        if !self.settled {
            self.advance(ctx, f64::from(dt));
        }
        let texture = self.texture.as_ref()?;
        Some((texture, self.rect))
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

    /// The visible territories' kernels, in texel coordinates.
    fn target_field(
        base: &BaseMap,
        snapshot: &WorldSnapshot,
        cap: usize,
        census_out: &mut CloudCensus,
        widest_out: &mut f64,
        tints_out: &mut Vec<u8>,
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
        // Whose each cloud is. `select_clouds` returns territories and the hue
        // is the *thread's*, so each is matched back to the pair it came from —
        // the rank a kernel carries is a grouping index for the field sampler
        // and must stay one, or two threads sharing a hue (which past twelve
        // threads they will) would be summed as one territory and PRD §6.4's
        // crowd signal would go quiet.
        tints_out.clear();
        for territory in &visible {
            tints_out.push(
                pairs
                    .iter()
                    .find(|(_, t)| std::ptr::eq(*t, *territory))
                    .map_or(live::NO_TINT, |(thread, _)| thread.tint),
            );
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
                    thread: u16::try_from(rank).unwrap_or(u16::MAX),
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
                    thread: u16::try_from(rank).unwrap_or(u16::MAX),
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
            self.texture = None;
            self.rect = Rect::NOTHING;
            self.contested = 0;
            self.settled = self.target.is_none();
            self.build_ms = started.elapsed().as_secs_f64() * 1_000.0;
            return;
        };

        let bands = field.bands_tinted(&self.tints);
        self.contested = bands
            .crowd
            .iter()
            .zip(bands.cells.iter())
            .filter(|(c, b)| **c >= CLOUD_CROWD && **b != live::NO_BAND)
            .count();

        // The notation itself, rasterised by `polis_render::live` so the window
        // and the headless renderer cannot disagree about a single stroke.
        let mut canvas = Canvas::new(bands.width, bands.height, NOTHING);
        live::paint_cloud_bands_into(&mut canvas, &bands, [bands.x0, bands.y0]);

        // Opaque where a mark landed, fully transparent everywhere else. There
        // is no intermediate alpha anywhere in this image, which is what stops
        // the layer compositing into the base map's own contrast band.
        let pixels: Vec<Color32> = canvas
            .pixels
            .as_chunks::<3>()
            .0
            .iter()
            .map(|p| {
                // Opaque where a mark landed. Keyed on the canvas's own clear
                // value rather than on a list of tones: with a hue per thread
                // there is no fixed list any more, and a membership test would
                // have quietly dropped every tinted pixel — which is to say,
                // every cloud.
                if *p == NOTHING {
                    Color32::TRANSPARENT
                } else {
                    Color32::from_rgb(p[0], p[1], p[2])
                }
            })
            .collect();
        let image = egui::ColorImage::new([bands.width, bands.height], pixels);
        match &mut self.texture {
            Some(handle) => handle.set(image, egui::TextureOptions::NEAREST),
            None => {
                self.texture =
                    Some(ctx.load_texture("polis-clouds", image, egui::TextureOptions::NEAREST));
            }
        }

        let per_texel = BASE_MAP_PIXELS as f32 / TEXELS as f32;
        self.rect = Rect::from_min_max(
            egui::pos2(bands.x0 as f32 * per_texel, bands.y0 as f32 * per_texel),
            egui::pos2(
                (bands.x0 + bands.width) as f32 * per_texel,
                (bands.y0 + bands.height) as f32 * per_texel,
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

    fn paint(field: &CloudField) -> Canvas {
        let bands = field.bands();
        let mut canvas = Canvas::new(bands.width, bands.height, NOTHING);
        live::paint_cloud_bands_into(&mut canvas, &bands, [bands.x0, bands.y0]);
        canvas
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
        let inked = canvas
            .pixels
            .as_chunks::<3>()
            .0
            .iter()
            .filter(|p| **p != NOTHING)
            .count();
        let banded = f
            .bands()
            .cells
            .iter()
            .fold(0usize, |n, b| n + usize::from(*b != live::NO_BAND));
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
        assert!(
            solo.bands().crowd.iter().all(|c| *c < CLOUD_CROWD),
            "one territory was drawn as contested ground"
        );
        assert!(
            pair.bands().crowd.iter().any(|c| *c >= CLOUD_CROWD),
            "two territories on one place left no contested ground"
        );
        assert_ne!(
            paint(&solo).pixels,
            paint(&pair).pixels,
            "contested ground is drawn exactly like uncontested ground"
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
