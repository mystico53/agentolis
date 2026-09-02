//! Cloud rendering — the territory density field (PRD §10.4).
//!
//! > Discrete iso-contour bands, **2–3 levels, never a continuous blur.**
//! > Continuous gradients turn to mush and you lose the ability to say "that file
//! > is in the core of this thread's work" versus "it's at the fringe."
//!
//! > Implementation: splat Gaussian kernels into an offscreen R16F density
//! > texture (512²), then threshold in a fragment shader to produce bands.
//! > Metaballs, essentially. A hundred kernels into a 512² target is free.
//!
//! # The additive field is unbounded — this is the one that bites
//!
//! Additive blending means the field sums. Measured on real hardware: a maximum
//! of **2.5371** with 1 750 texels above 1.0, because one full-weight Gaussian
//! peaks at 0.98889 and N overlapping kernels sum to roughly N.
//!
//! Thresholds must therefore be uniforms applied against a **fixed per-kernel
//! reference** — one full-weight kernel equals 1.0 — and explicitly **not**
//! against the observed field maximum. Normalising by the observed maximum was
//! rendered both ways: it collapsed every ordinary territory into a single fringe
//! band (body 1 725 px, core 422 px), which is exactly the flat mush §10.4
//! forbids. The fixed reference gave a healthy fringe 17 892 / body 11 063 /
//! core 2 296 spread (ADR-0020).
//!
//! Uniforms rather than WGSL constants, so retuning does not force a pipeline
//! rebuild — and the thresholds *will* need retuning: 0.08 / 0.30 / 0.75 and the
//! −4.5 falloff constant are probe values from a synthetic ten-kernel scene.
//!
//! # A band is a set of marks, not a fill — settled on the CPU path first
//!
//! Thresholding the field into bands and then *filling* each band is the obvious
//! reading of §10.4 and it is the wrong one. `plan`'s first cloud layer did
//! exactly that, and measured on the shipped image it inked two thirds of its
//! own footprint, lifted 40 % of the city by more than six levels, and moved the
//! base median underneath it from `L 22` to `L 43` — the map fogging into pale
//! grey precisely where the activity was.
//!
//! What `plan::render_band_validation` draws instead, and what this pass must
//! reproduce: for each of the 2–3 levels, a **contour stroke** on the level's
//! boundary (widest on the outermost, because that silhouette is what survives
//! being seen from across the room) plus a **hatch** whose spacing tightens and
//! whose stroke widens toward the core. Both opaque, both sparse. The test of a
//! correct implementation is not a screenshot: it is that the median luminance
//! of base-map pixels under a cloud equals the median outside one. On the CPU
//! path it does, to a tenth of a level.
//!
//! # Format
//!
//! `R16Float` with `RENDER_ATTACHMENT | TEXTURE_BINDING`, verified filterable and
//! blendable with `Features::empty()` and `Limits::downlevel_defaults()`. The
//! `Rgba16Float` fallback path exists for older integrated parts and is
//! **untested**, because this adapter never triggers it — and taking it requires
//! flipping the splat fragment entry point's return type from `f32` to
//! `vec4<f32>`, since naga validates the return type against the colour target's
//! component count.

use eframe::wgpu;

/// One Gaussian kernel, as uploaded.
///
/// Instanced: six vertices from `@builtin(vertex_index)`, one instance per splat,
/// four floats each. A hundred kernels is 1.6 KB and one `draw` call.
#[derive(Debug, Clone, Copy)]
pub struct SplatInstance {
    /// Centre in city space.
    pub centre: [f32; 2],
    /// Radius, from the territory's bandwidth.
    pub radius: f32,
    /// Kernel weight after decay.
    pub weight: f32,
}

/// Iso-band thresholds, as a uniform.
#[derive(Debug, Clone, Copy)]
pub struct IsoParams {
    /// Normalisation. **1.0 means "one full-weight kernel == 1.0"** — a fixed
    /// reference. Never set this from the observed field maximum.
    pub inv_scale: f32,
    /// Fringe threshold.
    pub t0: f32,
    /// Body threshold.
    pub t1: f32,
    /// Core threshold.
    pub t2: f32,
}

impl Default for IsoParams {
    /// Probe values, tuned against a synthetic ten-kernel scene rather than real
    /// KDE output. They are uniforms precisely so retuning is cheap.
    fn default() -> Self {
        Self {
            inv_scale: 1.0,
            t0: 0.08,
            t1: 0.30,
            t2: 0.75,
        }
    }
}

/// The offscreen density target and its two pipelines.
#[derive(Debug)]
pub struct DensityField {
    _private: (),
}

impl DensityField {
    /// Creates the 512² target, probing the format at runtime.
    pub fn new(device: &wgpu::Device, adapter: &wgpu::Adapter) -> Self {
        let _ = (device, adapter);
        todo!("PRD §10.4 — R16Float with an Rgba16Float fallback")
    }

    /// Encodes the additive splat pass into the caller's encoder.
    ///
    /// Blend state is `One`/`One`/`Add` on both colour and alpha. That additive
    /// blend is the whole trick, and it is what makes the field unbounded.
    pub fn encode(&self, encoder: &mut wgpu::CommandEncoder, splats: &[SplatInstance]) {
        let _ = (encoder, splats);
        todo!("PRD §10.4 — additive splat into the offscreen target")
    }

    /// Draws the thresholded bands into the caller's pass.
    pub fn draw_bands(&self, pass: &mut wgpu::RenderPass<'static>, params: IsoParams) {
        let _ = (pass, params);
        todo!("PRD §10.4 — 2-3 discrete bands, never a smoothstep")
    }
}

/// Picks the density texture format for an adapter.
///
/// Returns the fallback and a reason when `R16Float` is not blendable, so the
/// status bar can say *why* the clouds look different rather than leaving the
/// operator to guess.
pub fn pick_format(adapter: &wgpu::Adapter) -> (wgpu::TextureFormat, Option<String>) {
    let _ = adapter;
    todo!("docs/verified/gpu-stack.md — get_texture_format_features, check BLENDABLE")
}
