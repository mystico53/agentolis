//! Territory clouds (PRD §10.4).
//!
//! > Discrete iso-contour bands, **2–3 levels, never a continuous blur.**
//! > Continuous gradients turn to mush and you lose the ability to say "that
//! > file is in the core of this thread's work" versus "it's at the fringe."
//!
//! > Implementation: splat Gaussian kernels into an offscreen R16F density
//! > texture (512²), then threshold in a fragment shader to produce bands.
//! > Metaballs, essentially.
//!
//! This is that algorithm on the CPU, into an `egui` texture rather than an
//! `R16Float` render target. `polis-render` owns the GPU version; the window
//! needs the layer to exist today, and the two agree about what is being
//! computed because both are the same three steps: **sum the kernels, threshold
//! at fixed levels, fill discrete bands.**
//!
//! # Three findings from `docs/verified/gpu-stack.md` §5 are honoured here
//!
//! 1. **The field is unbounded.** Additive blending means N overlapping kernels
//!    sum to ≈N, not to 1. The thresholds are therefore levels on an open scale,
//!    not fractions of a maximum.
//! 2. **Normalising by the observed maximum is a trap.** The probe measured it:
//!    one hot cluster consumed the whole range and every other territory
//!    collapsed into a single fringe band. The reference is **fixed** — one
//!    full-weight kernel is 1.0 — and hot spots clip into the core band.
//! 3. **Hard steps, never a `smoothstep`.** [`BANDS`] is an `if` chain over
//!    three levels and the assertion that keeps it that way counts distinct
//!    colours in the output.
//!
//! # Why the grid is anchored in map space, not on screen
//!
//! A screen-anchored field would have to be recomputed and re-uploaded on every
//! pan and every zoom, which is exactly the kind of per-frame work PRD §13.1's
//! idle budget rules out. Anchored to the kernels' own bounding box in base-map
//! pixels, it changes only when the world does, so panning around a still world
//! uploads nothing. Zooming in makes the cloud softer, which is the correct
//! answer for the layer PRD §12 calls *fuzzy above, exact below*.

// The field is a numeric grid: every cast here lands in a texel index that is
// clamped on purpose.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

use eframe::egui::{self, Color32, Pos2, Rect};
use polis_world::snapshot::WorldSnapshot;
use polis_world::territory::{self, Territory};

use crate::basemap::BaseMap;
use crate::palette;

/// The density grid's edge, in texels.
///
/// PRD §10.4 specifies 512² for the GPU target. This is the CPU version and it
/// only ever covers the territories' own bounding box rather than the whole
/// viewport, so a quarter of the linear resolution lands in the same place while
/// costing a sixteenth of the upload.
pub const GRID: usize = 256;

/// Iso levels, fringe to core, on the fixed reference where one full-weight
/// kernel peaks at 1.0.
pub const BANDS: [f32; 3] = [0.10, 0.34, 0.75];

/// How far past a kernel's radius the Gaussian is evaluated. Beyond this the
/// truncated kernel is zero, so the loop can stop.
const KERNEL_EXTENT: f32 = 1.0;

/// One kernel's peak, subtracted so the kernel reaches exactly zero at its own
/// edge and adjacent splats do not seam.
const TRUNCATION: f32 = 0.011_109; // exp(-4.5)

/// The rasterised cloud layer: one texture, recomputed only when the world
/// changes.
pub struct Clouds {
    texture: Option<egui::TextureHandle>,
    /// The base-map rectangle the texture covers.
    rect: Rect,
    /// The snapshot generation the field was built from.
    generation: u64,
    /// The cloud cap in force when it was built.
    cap: usize,
    /// How many kernels went in, for the status bar.
    pub kernels: usize,
    /// How many territories got a cloud, out of how many threads.
    pub shown: usize,
    /// How long the last rebuild took.
    pub build_ms: f64,
}

impl std::fmt::Debug for Clouds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Clouds")
            .field("kernels", &self.kernels)
            .field("shown", &self.shown)
            .field("generation", &self.generation)
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
            cap: 0,
            kernels: 0,
            shown: 0,
            build_ms: 0.0,
        }
    }
}

impl Clouds {
    /// Rebuilds the field if the world moved, and returns what to draw.
    ///
    /// `cap` is PRD §10.4's cloud cap: *"Forty threads means forty systems and
    /// the map vanishes under haze."* Which territories survive it is
    /// [`polis_world::territory::visible_clouds`]'s decision, not this module's.
    pub fn update(
        &mut self,
        ctx: &egui::Context,
        base: &BaseMap,
        snapshot: &WorldSnapshot,
        cap: usize,
    ) -> Option<(&egui::TextureHandle, Rect)> {
        if self.generation != snapshot.generation || self.cap != cap {
            self.rebuild(ctx, base, snapshot, cap);
            self.generation = snapshot.generation;
            self.cap = cap;
        }
        let texture = self.texture.as_ref()?;
        Some((texture, self.rect))
    }

    fn rebuild(
        &mut self,
        ctx: &egui::Context,
        base: &BaseMap,
        snapshot: &WorldSnapshot,
        cap: usize,
    ) {
        let started = std::time::Instant::now();
        let pairs: Vec<(&polis_world::Thread, &Territory)> = snapshot
            .threads
            .iter()
            .map(|thread| (thread, &thread.territory))
            .collect();
        let visible = territory::visible_clouds(&pairs, cap);
        self.shown = visible.len();

        // Splats in base-map pixels: centre, radius, weight.
        let mut splats: Vec<(Pos2, f32, f32)> = Vec::new();
        let scale = base.map_px_per_world();
        for territory in &visible {
            for kernel in &territory.kernels {
                let radius = (kernel.radius * scale).max(2.0);
                splats.push((base.to_map(kernel.centre), radius, kernel.weight));
            }
        }
        self.kernels = splats.len();
        if splats.is_empty() {
            self.texture = None;
            self.rect = Rect::NOTHING;
            self.build_ms = started.elapsed().as_secs_f64() * 1_000.0;
            return;
        }

        let mut min = Pos2::new(f32::INFINITY, f32::INFINITY);
        let mut max = Pos2::new(f32::NEG_INFINITY, f32::NEG_INFINITY);
        for (centre, radius, _) in &splats {
            let r = radius * KERNEL_EXTENT;
            min.x = min.x.min(centre.x - r);
            min.y = min.y.min(centre.y - r);
            max.x = max.x.max(centre.x + r);
            max.y = max.y.max(centre.y + r);
        }
        // Square the box so texels are square and the field is not stretched.
        let side = (max.x - min.x).max(max.y - min.y).max(1.0);
        let centre = Pos2::new(f32::midpoint(min.x, max.x), f32::midpoint(min.y, max.y));
        let rect = Rect::from_center_size(centre, egui::Vec2::splat(side));

        let mut field = vec![0.0f32; GRID * GRID];
        let per_texel = side / GRID as f32;
        for (centre, radius, weight) in &splats {
            splat(&mut field, rect, per_texel, *centre, *radius, *weight);
        }

        let image = threshold(&field);
        match &mut self.texture {
            Some(handle) => handle.set(image, egui::TextureOptions::LINEAR),
            None => {
                self.texture =
                    Some(ctx.load_texture("polis-clouds", image, egui::TextureOptions::LINEAR));
            }
        }
        self.rect = rect;
        self.build_ms = started.elapsed().as_secs_f64() * 1_000.0;
    }
}

/// Adds one truncated Gaussian to the field, over its own footprint only.
///
/// Restricting the loop to the kernel's own bounding box is what makes the
/// whole layer cost the sum of the kernels' areas rather than
/// `kernels × GRID²`.
fn splat(field: &mut [f32], rect: Rect, per_texel: f32, centre: Pos2, radius: f32, weight: f32) {
    let to_texel = |v: f32, lo: f32| (v - lo) / per_texel;
    let r = radius * KERNEL_EXTENT;
    let x0 = to_texel(centre.x - r, rect.min.x).floor().max(0.0) as usize;
    let x1 = (to_texel(centre.x + r, rect.min.x).ceil() as usize).min(GRID);
    let y0 = to_texel(centre.y - r, rect.min.y).floor().max(0.0) as usize;
    let y1 = (to_texel(centre.y + r, rect.min.y).ceil() as usize).min(GRID);
    for ty in y0..y1 {
        let y = rect.min.y + (ty as f32 + 0.5) * per_texel;
        for tx in x0..x1 {
            let x = rect.min.x + (tx as f32 + 0.5) * per_texel;
            let dx = (x - centre.x) / radius;
            let dy = (y - centre.y) / radius;
            let d2 = dx * dx + dy * dy;
            if d2 > 1.0 {
                continue;
            }
            // Exactly the probe's kernel: truncated at the quad edge so
            // adjacent splats do not seam.
            let g = (-4.5 * d2).exp() - TRUNCATION;
            if g > 0.0 {
                field[ty * GRID + tx] += g * weight;
            }
        }
    }
}

/// Thresholds the field into three discrete bands.
///
/// Hard steps and a transparent floor, never a `smoothstep`: PRD §10.4's
/// "never a continuous blur" is the whole reason this layer is bands at all.
fn threshold(field: &[f32]) -> egui::ColorImage {
    let bands = palette::cloud_bands();
    let colours = [
        bands[0].alpha(0.30),
        bands[1].alpha(0.42),
        bands[2].alpha(0.54),
    ];
    let pixels = field
        .iter()
        .map(|&d| {
            if d >= BANDS[2] {
                colours[2]
            } else if d >= BANDS[1] {
                colours[1]
            } else if d >= BANDS[0] {
                colours[0]
            } else {
                Color32::TRANSPARENT
            }
        })
        .collect();
    egui::ColorImage::new([GRID, GRID], pixels)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn field_with(splats: &[(Pos2, f32, f32)]) -> Vec<f32> {
        let rect = Rect::from_min_size(Pos2::ZERO, egui::Vec2::splat(100.0));
        let mut field = vec![0.0f32; GRID * GRID];
        for (c, r, w) in splats {
            splat(&mut field, rect, 100.0 / GRID as f32, *c, *r, *w);
        }
        field
    }

    /// `docs/verified/gpu-stack.md` §5's central finding, reproduced on the CPU:
    /// overlapping kernels **sum**, so the field is unbounded and the thresholds
    /// cannot be fractions of a maximum.
    #[test]
    fn overlapping_kernels_sum_past_one() {
        let one = field_with(&[(Pos2::new(50.0, 50.0), 20.0, 1.0)]);
        let peak_one = one.iter().copied().fold(0.0f32, f32::max);
        assert!(
            (peak_one - (1.0 - TRUNCATION)).abs() < 0.01,
            "one kernel peaks at ~0.989, got {peak_one}"
        );

        let three = field_with(&[
            (Pos2::new(50.0, 50.0), 20.0, 1.0),
            (Pos2::new(52.0, 50.0), 20.0, 1.0),
            (Pos2::new(51.0, 52.0), 20.0, 1.0),
        ]);
        let peak_three = three.iter().copied().fold(0.0f32, f32::max);
        assert!(
            peak_three > 2.5,
            "three overlapping kernels summed to {peak_three}"
        );
    }

    /// The kernel is truncated at its own edge, so two adjacent territories do
    /// not leave a seam and a kernel contributes nothing outside its radius.
    #[test]
    fn a_kernel_is_exactly_zero_outside_its_radius() {
        let field = field_with(&[(Pos2::new(50.0, 50.0), 10.0, 1.0)]);
        let per_texel = 100.0 / GRID as f32;
        let at = |x: f32, y: f32| field[(y / per_texel) as usize * GRID + (x / per_texel) as usize];
        assert!(at(50.0, 50.0) > 0.9);
        // Exactly zero, not approximately: a truncated kernel contributes
        // nothing at all outside its radius, which is what stops two adjacent
        // territories from seaming.
        #[allow(clippy::float_cmp)]
        {
            assert_eq!(at(50.0, 12.0), 0.0, "well outside the radius");
            assert_eq!(at(9.0, 9.0), 0.0, "the corner of the grid");
        }
    }

    /// PRD §10.4: 2–3 levels, hard steps. The regression this guards against is
    /// somebody replacing the `if` chain with a `smoothstep`, which would
    /// produce thousands of distinct colours instead of four.
    #[test]
    fn the_output_has_exactly_four_distinct_colours() {
        let field = field_with(&[
            (Pos2::new(40.0, 50.0), 25.0, 1.4),
            (Pos2::new(60.0, 50.0), 25.0, 1.4),
            (Pos2::new(50.0, 50.0), 12.0, 2.0),
        ]);
        let image = threshold(&field);
        let distinct: HashSet<[u8; 4]> = image
            .pixels
            .iter()
            .map(|c| [c.r(), c.g(), c.b(), c.a()])
            .collect();
        assert!(
            distinct.len() <= 4,
            "transparent plus three bands, got {}",
            distinct.len()
        );
        assert_eq!(distinct.len(), 4, "all three bands should be present");
    }

    /// Multi-lobed shapes come free from field addition (PRD §6.4): an agent
    /// working in `auth` with one worker in `tests` gets two lobes and a thin
    /// connecting band, not a bounding box over the empty space between.
    #[test]
    fn two_separated_kernels_make_two_lobes_and_not_one_blob() {
        let field = field_with(&[
            (Pos2::new(25.0, 50.0), 15.0, 1.0),
            (Pos2::new(75.0, 50.0), 15.0, 1.0),
        ]);
        let per_texel = 100.0 / GRID as f32;
        let row = (50.0 / per_texel) as usize;
        let at = |x: f32| field[row * GRID + (x / per_texel) as usize];
        assert!(at(25.0) > BANDS[2], "left lobe has a core");
        assert!(at(75.0) > BANDS[2], "right lobe has a core");
        assert!(
            at(50.0) < BANDS[0],
            "the empty space between them is not claimed: {}",
            at(50.0)
        );
    }

    /// The trap the probe caught by measuring rather than by looking: with the
    /// thresholds normalised by the observed maximum, one hot cluster flattens
    /// every other territory into the fringe band. A fixed reference does not.
    #[test]
    fn a_hot_cluster_does_not_flatten_the_rest_of_the_map() {
        let field = field_with(&[
            (Pos2::new(20.0, 20.0), 10.0, 1.0),
            (Pos2::new(70.0, 70.0), 10.0, 3.0),
            (Pos2::new(72.0, 70.0), 10.0, 3.0),
        ]);
        let per_texel = 100.0 / GRID as f32;
        let at = |x: f32, y: f32| field[(y / per_texel) as usize * GRID + (x / per_texel) as usize];
        let lonely = at(20.0, 20.0);
        let peak = field.iter().copied().fold(0.0f32, f32::max);
        assert!(peak > 5.0, "the cluster is hot: {peak}");
        assert!(
            lonely >= BANDS[2],
            "the lone territory keeps its own core band at the fixed reference: {lonely}"
        );
        assert!(
            lonely / peak < BANDS[1],
            "and would have been demoted to fringe had the field been normalised by {peak}"
        );
    }

    #[test]
    fn an_empty_world_draws_no_cloud_at_all() {
        let field = field_with(&[]);
        let image = threshold(&field);
        assert!(image.pixels.iter().all(|c| c.a() == 0));
    }
}
