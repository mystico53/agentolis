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
use polis_render::live::{self, CloudField, CloudKernel, CloudTween, CLOUD_CROWD, CLOUD_TONES};
use polis_render::raster::Canvas;
use polis_world::snapshot::WorldSnapshot;
use polis_world::territory::{self, Territory};

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
    /// How many kernels went in, for the status bar.
    pub kernels: usize,
    /// How many territories got a cloud.
    pub shown: usize,
    /// Texels two or more territories both claim — PRD §6.4's contention signal,
    /// visible before a write collides.
    pub contested: usize,
    /// How long the last repaint took.
    pub build_ms: f64,
}

impl std::fmt::Debug for Clouds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Clouds")
            .field("kernels", &self.kernels)
            .field("shown", &self.shown)
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
            kernels: 0,
            shown: 0,
            contested: 0,
            build_ms: 0.0,
        }
    }
}

impl Clouds {
    /// Advances the field and returns what to draw.
    ///
    /// `cap` is PRD §10.4's cloud cap: *"Forty threads means forty systems and
    /// the map vanishes under haze."* Which territories survive it is
    /// [`polis_world::territory::visible_clouds`]'s decision, not this module's.
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
            self.target =
                Self::target_field(base, snapshot, cap, &mut self.kernels, &mut self.shown);
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

    /// The visible territories' kernels, in texel coordinates.
    fn target_field(
        base: &BaseMap,
        snapshot: &WorldSnapshot,
        cap: usize,
        kernels_out: &mut usize,
        shown_out: &mut usize,
    ) -> Option<CloudField> {
        let pairs: Vec<(&polis_world::Thread, &Territory)> = snapshot
            .threads
            .iter()
            .map(|thread| (thread, &thread.territory))
            .collect();
        let visible = territory::visible_clouds(&pairs, cap);
        *shown_out = visible.len();

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
        }
        *kernels_out = kernels.len();
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

        let bands = field.bands();
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
                if CLOUD_TONES.contains(p) {
                    Color32::from_rgb(p[0], p[1], p[2])
                } else {
                    Color32::TRANSPARENT
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
