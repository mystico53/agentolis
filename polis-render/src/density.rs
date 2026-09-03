//! Cloud rendering on the GPU — the territory density field (PRD §10.4).
//!
//! > Discrete iso-contour bands, **2–3 levels, never a continuous blur.**
//! > Continuous gradients turn to mush and you lose the ability to say "that file
//! > is in the core of this thread's work" versus "it's at the fringe."
//!
//! > Implementation: splat Gaussian kernels into an offscreen R16F density
//! > texture (512²), then threshold in a fragment shader to produce bands.
//! > Metaballs, essentially. A hundred kernels into a 512² target is free.
//!
//! This is [`crate::live`]'s cloud layer as a wgpu pipeline. It computes the
//! same field with the same kernel against the same thresholds and draws the
//! same marks, and it imports every constant from `live` rather than keeping a
//! second copy — a visual language with two definitions has none.
//!
//! # Three passes, not two, and the third is the one PRD §6.4 asks for
//!
//! The obvious pipeline is splat-then-threshold. It cannot answer §6.4's actual
//! question. *"Overlap is field addition — two territories overlapping is just a
//! denser region, **which is exactly the contention signal**"*: the sum makes
//! the region denser, but a sum cannot tell **one** thread working hard from
//! **two** threads standing on the same files, and only the second is news.
//!
//! Additive blending cannot separate them either, because the test is per
//! territory and applied *after* that territory's own kernels have summed. So:
//!
//! 1. **splat** one territory's kernels into a scratch target, additively;
//! 2. **accumulate** that scratch into `(density, crowd)` — adding the field and
//!    adding one wherever this territory alone already reaches fringe level;
//! 3. repeat 1–2 per territory, then **threshold** once.
//!
//! Two passes per territory, and PRD §10.4 caps the visible clouds, so it is a
//! dozen 512² passes in the worst case.
//!
//! # The additive field is unbounded — this is the one that bites
//!
//! Additive blending means the field sums. Measured on real hardware: a maximum
//! of **2.5371** with 1 750 texels above 1.0, because one full-weight kernel
//! peaks near 1.0 and N overlapping kernels sum to roughly N.
//!
//! Thresholds must therefore be uniforms applied against a **fixed per-kernel
//! reference** — one full-weight kernel equals 1.0 — and explicitly **not**
//! against the observed field maximum. Normalising by the observed maximum was
//! rendered both ways: it collapsed every ordinary territory into a single fringe
//! band, which is exactly the flat mush §10.4 forbids (ADR-0020).
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
//! So [`DensityField::draw_bands`] draws what [`crate::live::paint_cloud_bands`]
//! draws: a **contour stroke** on each level's boundary plus a **hatch** whose
//! spacing tightens toward the core, crossed where territories overlap. Both
//! opaque, both sparse, and both `discard` everywhere else so the city shows
//! through untouched. The contour width is passed in per band rather than
//! hardcoded, because a fixed three-pixel ring around a small cloud is 60 % of
//! it — see [`crate::live::CLOUD_CONTOUR_FRACTION`].
//!
//! # The kernel is `live`'s quartic, not the probe's Gaussian
//!
//! `docs/verified/gpu-stack.md` splats `exp(-4.5·d²) − exp(-4.5)`. This module
//! splats `(1 − d²)²`, which is `live`'s own `kernel`. Both are bells with
//! compact support; using the CPU's one is what lets a test assert that the GPU
//! field and the CPU field agree to four decimals, which is the only way to tell
//! "additive blending is broken" from "your kernels do not overlap".
//!
//! # Format
//!
//! The scratch target is `R16Float` where the adapter allows it as a blendable,
//! filterable render attachment, and `Rgba16Float` otherwise — the fallback is
//! guaranteed by core WebGPU. Taking it flips the splat fragment entry point
//! from `f32` to `vec4<f32>`, because naga validates the return type against the
//! colour target's component count. The accumulator is always `Rgba16Float`: it
//! needs two channels, and two-channel formats are not worth a second probe.

// The shader constants and the pixel arithmetic behind them are `f32`; the
// notation they come from is `f64`. Every cast here is a narrowing of a value
// already inside `[0, 255]` or a small pixel count.
// `new` and `encode` are long because a wgpu pipeline is a long literal, and
// splitting a descriptor across helpers hides which field belongs to which pass
// — which is exactly the thing an implementer needs to read here.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]

use std::borrow::Cow;

use eframe::wgpu;

use crate::live::{
    CLOUD_CROWD, CLOUD_HATCH, CLOUD_HATCH_SPACING, CLOUD_HATCH_WIDTH, CLOUD_ISO,
    CLOUD_OVERLAP_SPACING, CLOUD_TONES,
};

/// The offscreen field's edge, in texels (PRD §10.4: *"a 512² target"*).
pub const GRID: u32 = 512;

/// One Gaussian kernel, as uploaded.
///
/// Instanced: six vertices from `@builtin(vertex_index)`, one instance per
/// splat, four floats each. A hundred kernels is 1.6 KB and one `draw` call.
#[derive(Debug, Clone, Copy)]
pub struct SplatInstance {
    /// Centre in city space.
    pub centre: [f32; 2],
    /// Radius, from the territory's bandwidth (PRD §6.4).
    pub radius: f32,
    /// Kernel weight after decay.
    pub weight: f32,
    /// Which territory dropped it.
    ///
    /// Never uploaded — it groups the splats into per-territory passes on the
    /// CPU, which is what produces the `crowd` channel. See the module docs.
    pub thread: u16,
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
    /// Contour stroke width per band, in output pixels. Comes from
    /// [`crate::live::contour_steps`] on the CPU path; a caller with no band map
    /// to measure can pass [`crate::live::CLOUD_CONTOUR_WIDTH`].
    pub contour: [f32; 3],
    /// The viewport this pass draws into, in pixels. The contour and the hatch
    /// are screen-space measurements, so the shader needs it.
    pub viewport: [f32; 2],
}

impl Default for IsoParams {
    /// [`crate::live`]'s thresholds, which are the ones the notation was tuned
    /// against on real sessions.
    fn default() -> Self {
        Self {
            inv_scale: 1.0,
            t0: CLOUD_ISO[0] as f32,
            t1: CLOUD_ISO[1] as f32,
            t2: CLOUD_ISO[2] as f32,
            contour: [1.0, 1.0, 1.0],
            viewport: [1.0, 1.0],
        }
    }
}

/// The uniform the iso pass reads. `repr(C)`, 16-byte aligned throughout.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct IsoUniform {
    /// `inv_scale`, `t0`, `t1`, `t2`.
    thresholds: [f32; 4],
    /// Contour width per band, then the crowd threshold.
    contour: [f32; 4],
    /// Hatch direction, the overlap spacing multiplier, and a pad.
    hatch: [f32; 4],
    /// Hatch spacing per band, and a pad.
    spacing: [f32; 4],
    /// Hatch stroke width per band, and a pad.
    widths: [f32; 4],
    /// Viewport in pixels, and two pads.
    viewport: [f32; 4],
    /// The three iso tones, premultiplied into the target's colour space.
    tones: [[f32; 4]; 3],
}

/// Whether a colour written by a fragment shader is encoded by the hardware.
///
/// `docs/verified/gpu-stack.md` §3: eframe's surface is `Rgba8Unorm` on Vulkan
/// and `Bgra8Unorm` on DX12 — **non**-sRGB in both cases — while an offscreen
/// `Rgba8UnormSrgb` target encodes on write. Identical shader output therefore
/// looks visibly darker in the window than headless, and [`crate::live`]'s tones
/// are sRGB bytes. So the conversion happens here, once, from the runtime
/// format, and never from a constant.
fn tone_for(format: wgpu::TextureFormat, tone: [u8; 3]) -> [f32; 4] {
    let encoded = format.is_srgb();
    let channel = |v: u8| -> f32 {
        let s = f32::from(v) / 255.0;
        if encoded {
            // sRGB EOTF: the hardware will apply its inverse on write.
            if s <= 0.040_45 {
                s / 12.92
            } else {
                ((s + 0.055) / 1.055).powf(2.4)
            }
        } else {
            s
        }
    };
    [channel(tone[0]), channel(tone[1]), channel(tone[2]), 1.0]
}

/// What [`pick_format`] decided and why.
#[derive(Debug, Clone)]
pub struct DensityFormatReport {
    /// The scratch target's format.
    pub chosen: wgpu::TextureFormat,
    /// Why the fallback was taken, if it was. `None` means `R16Float` was fine.
    ///
    /// A reason rather than a flag, so the status bar can say *why* the clouds
    /// look different instead of leaving the operator to guess.
    pub fallback: Option<String>,
}

/// Picks the density texture format for an adapter.
///
/// `R16Float` needs four properties for this pipeline — `RENDER_ATTACHMENT`,
/// `TEXTURE_BINDING`, `BLENDABLE` and `FILTERABLE` — and has all four on the
/// hardware `docs/verified/gpu-stack.md` was measured on, with
/// `Features::empty()` and `Limits::downlevel_defaults()`. It is not universal
/// on older integrated parts, and the check is six lines.
#[must_use]
pub fn pick_format(adapter: &wgpu::Adapter) -> DensityFormatReport {
    let f = adapter.get_texture_format_features(wgpu::TextureFormat::R16Float);
    let need_usage = wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING;
    let need_flags =
        wgpu::TextureFormatFeatureFlags::BLENDABLE | wgpu::TextureFormatFeatureFlags::FILTERABLE;
    if f.allowed_usages.contains(need_usage) && f.flags.contains(need_flags) {
        return DensityFormatReport {
            chosen: wgpu::TextureFormat::R16Float,
            fallback: None,
        };
    }
    DensityFormatReport {
        chosen: wgpu::TextureFormat::Rgba16Float,
        fallback: Some(format!(
            "R16Float is not a blendable, filterable render target here \
             (usages {:?}, flags {:?}); clouds use Rgba16Float, four times the memory \
             and the same picture",
            f.allowed_usages, f.flags
        )),
    }
}

/// The accumulator's format: two channels, always available, always blendable.
const ACCUM: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

/// The offscreen density field and its three pipelines.
pub struct DensityField {
    /// The scratch one territory splats into, cleared per territory.
    scratch: wgpu::TextureView,
    /// `(density, crowd)` summed over the territories.
    accum: wgpu::TextureView,
    /// The accumulator itself, so a test can read the field back and compare it
    /// with the CPU path — the one check that tells a broken additive blend
    /// from a scene with no overlap in it.
    accum_texture: wgpu::Texture,
    report: DensityFormatReport,
    /// The format the iso pass draws into. Decides whether the tones are
    /// written sRGB-encoded or linear; never a constant.
    iso_target: wgpu::TextureFormat,
    camera: wgpu::Buffer,
    camera_group: wgpu::BindGroup,
    instances: wgpu::Buffer,
    capacity: usize,
    iso_buffer: wgpu::Buffer,
    scratch_group: wgpu::BindGroup,
    accum_group: wgpu::BindGroup,
    splat: wgpu::RenderPipeline,
    accumulate: wgpu::RenderPipeline,
    iso: wgpu::RenderPipeline,
    /// Splat ranges, one per territory, filled by [`DensityField::encode`].
    runs: Vec<(u32, u32)>,
}

impl std::fmt::Debug for DensityField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DensityField")
            .field("format", &self.report.chosen)
            .field("fallback", &self.report.fallback)
            .field("capacity", &self.capacity)
            .field("territories", &self.runs.len())
            .finish_non_exhaustive()
    }
}

/// How many splats the instance buffer starts with. It grows on demand; PRD
/// §10.4 caps the visible clouds, so it does not grow far.
const INITIAL_SPLATS: usize = 256;

impl DensityField {
    /// Builds the targets and the three pipelines.
    ///
    /// `target_format` must come from `render_state.target_format` at runtime,
    /// never from a constant: it decides both the iso pass's colour target and
    /// whether the tones need converting out of sRGB.
    #[must_use]
    pub fn new(
        device: &wgpu::Device,
        adapter: &wgpu::Adapter,
        target_format: wgpu::TextureFormat,
    ) -> Self {
        let report = pick_format(adapter);
        let make = |label: &str, format: wgpu::TextureFormat| -> wgpu::Texture {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: GRID,
                    height: GRID,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            })
        };
        let scratch = make("polis-density-scratch", report.chosen)
            .create_view(&wgpu::TextureViewDescriptor::default());
        let accum_texture = make("polis-density-accum", ACCUM);
        let accum = accum_texture.create_view(&wgpu::TextureViewDescriptor::default());

        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("polis-density"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(SHADER)),
        });

        // --- group 0 of the splat pipeline: the camera ----------------------
        let camera = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("polis-density-camera"),
            size: 64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let camera_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("polis-density-camera"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let camera_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("polis-density-camera"),
            layout: &camera_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: camera.as_entire_binding(),
            }],
        });

        // --- group 0 of the accumulate and iso pipelines ---------------------
        let iso_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("polis-density-iso"),
            size: std::mem::size_of::<IsoUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("polis-density"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let sample_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("polis-density-sample"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let bind_sample = |label: &str, view: &wgpu::TextureView| -> wgpu::BindGroup {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(label),
                layout: &sample_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: iso_buffer.as_entire_binding(),
                    },
                ],
            })
        };
        let scratch_group = bind_sample("polis-density-scratch", &scratch);
        let accum_group = bind_sample("polis-density-accum", &accum);

        // --- pipelines -------------------------------------------------------
        let splat_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("polis-splat"),
            bind_group_layouts: &[Some(&camera_layout)],
            immediate_size: 0,
        });
        let full_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("polis-fullscreen"),
            bind_group_layouts: &[Some(&sample_layout)],
            immediate_size: 0,
        });
        // `One`/`One`/`Add` on both channels: the additive blend is the whole
        // trick, and it is what makes the field unbounded.
        let additive = Some(wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::One,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::One,
                operation: wgpu::BlendOperation::Add,
            },
        });

        let splat = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("polis-splat"),
            layout: Some(&splat_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_splat"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: 16,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &[
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x2,
                            offset: 0,
                            shader_location: 0,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32,
                            offset: 8,
                            shader_location: 1,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32,
                            offset: 12,
                            shader_location: 2,
                        },
                    ],
                })],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                // naga validates the fragment return type against the colour
                // target's component count, so the fallback needs its own entry.
                entry_point: Some(if report.chosen == wgpu::TextureFormat::R16Float {
                    "fs_splat_r"
                } else {
                    "fs_splat_rgba"
                }),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: report.chosen,
                    blend: additive,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        let accumulate = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("polis-accumulate"),
            layout: Some(&full_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_full"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_accumulate"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: ACCUM,
                    blend: additive,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        let iso = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("polis-iso"),
            layout: Some(&full_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_full"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_iso"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    // The marks are opaque, so this is `Replace` in everything
                    // but name — `live`'s rule that a cloud never blends into
                    // the map's own band, carried onto the GPU.
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        let instances = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("polis-density-splats"),
            size: (INITIAL_SPLATS * 16) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            scratch,
            accum,
            accum_texture,
            report,
            iso_target: target_format,
            camera,
            camera_group,
            instances,
            capacity: INITIAL_SPLATS,
            iso_buffer,
            scratch_group,
            accum_group,
            splat,
            accumulate,
            iso,
            runs: Vec::new(),
        }
    }

    /// The scratch format that was chosen, and why the fallback was taken.
    #[must_use]
    pub fn report(&self) -> &DensityFormatReport {
        &self.report
    }

    /// Encodes the field into the caller's encoder: splat, accumulate, per
    /// territory.
    ///
    /// This has to be called from `CallbackTrait::prepare`. `paint()` already
    /// runs inside egui's render pass and a render pass cannot nest, so the
    /// offscreen work has nowhere else to go — that ordering is the single most
    /// important structural fact about this crate.
    ///
    /// `camera` is a column-major `mat4x4` mapping city space to NDC. Column
    /// major means the translation lives in the **last row** of the Rust
    /// literal, not the last column.
    pub fn encode(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        camera: [[f32; 4]; 4],
        splats: &[SplatInstance],
        params: IsoParams,
    ) {
        queue.write_buffer(&self.camera, 0, bytemuck::cast_slice(&camera));
        queue.write_buffer(
            &self.iso_buffer,
            0,
            bytemuck::bytes_of(&self.uniform(params)),
        );

        // Grouped by territory, so each gets its own scratch pass. Sorting a
        // few hundred instances is nothing next to a 512² pass.
        let mut order: Vec<&SplatInstance> = splats
            .iter()
            .filter(|s| s.weight > 0.0 && s.radius > 0.0)
            .collect();
        order.sort_by_key(|s| s.thread);
        let flat: Vec<[f32; 4]> = order
            .iter()
            .map(|s| [s.centre[0], s.centre[1], s.radius, s.weight])
            .collect();
        if flat.len() > self.capacity {
            self.capacity = flat.len().next_power_of_two();
            self.instances = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("polis-density-splats"),
                size: (self.capacity * 16) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        if !flat.is_empty() {
            queue.write_buffer(&self.instances, 0, bytemuck::cast_slice(&flat));
        }
        self.runs.clear();
        let mut i = 0;
        while i < order.len() {
            let thread = order[i].thread;
            let mut j = i;
            while j < order.len() && order[j].thread == thread {
                j += 1;
            }
            self.runs.push((i as u32, j as u32));
            i = j;
        }

        // The accumulator is cleared once; the scratch is cleared per territory.
        let mut first = true;
        for (from, to) in self.runs.clone() {
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("polis-splat"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &self.scratch,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(&self.splat);
                pass.set_bind_group(0, &self.camera_group, &[]);
                pass.set_vertex_buffer(0, self.instances.slice(..));
                pass.draw(0..6, from..to);
            }
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("polis-accumulate"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &self.accum,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: if first {
                                wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT)
                            } else {
                                wgpu::LoadOp::Load
                            },
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(&self.accumulate);
                pass.set_bind_group(0, &self.scratch_group, &[]);
                pass.draw(0..3, 0..1);
            }
            first = false;
        }
        if first {
            // No territory at all: the accumulator still has to be cleared, or
            // the last frame's clouds would survive as ghosts.
            let _ = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("polis-accumulate-clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.accum,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        }
    }

    /// Draws the thresholded bands into the caller's pass.
    ///
    /// Layer 3: call this **before** the district outlines and the labels, so
    /// the map stays readable through the clouds (PRD §10.3).
    pub fn draw_bands(&self, pass: &mut wgpu::RenderPass<'static>) {
        pass.set_pipeline(&self.iso);
        pass.set_bind_group(0, &self.accum_group, &[]);
        pass.draw(0..3, 0..1);
    }

    /// The accumulator's view, for a caller that wants to read the field back.
    #[must_use]
    pub fn accumulator(&self) -> &wgpu::TextureView {
        &self.accum
    }

    /// The accumulator texture, for a numeric readback.
    #[must_use]
    pub fn accumulator_texture(&self) -> &wgpu::Texture {
        &self.accum_texture
    }

    fn uniform(&self, params: IsoParams) -> IsoUniform {
        IsoUniform {
            thresholds: [params.inv_scale, params.t0, params.t1, params.t2],
            contour: [
                params.contour[0],
                params.contour[1],
                params.contour[2],
                f32::from(CLOUD_CROWD),
            ],
            hatch: [
                CLOUD_HATCH[0] as f32,
                CLOUD_HATCH[1] as f32,
                CLOUD_OVERLAP_SPACING as f32,
                0.0,
            ],
            spacing: [
                CLOUD_HATCH_SPACING[0] as f32,
                CLOUD_HATCH_SPACING[1] as f32,
                CLOUD_HATCH_SPACING[2] as f32,
                0.0,
            ],
            widths: [
                CLOUD_HATCH_WIDTH[0] as f32,
                CLOUD_HATCH_WIDTH[1] as f32,
                CLOUD_HATCH_WIDTH[2] as f32,
                0.0,
            ],
            viewport: [
                params.viewport[0].max(1.0),
                params.viewport[1].max(1.0),
                0.0,
                0.0,
            ],
            tones: [
                tone_for(self.iso_target, CLOUD_TONES[0]),
                tone_for(self.iso_target, CLOUD_TONES[1]),
                tone_for(self.iso_target, CLOUD_TONES[2]),
            ],
        }
    }
}

/// The whole cloud layer, in one module.
///
/// Three entry-point pairs share one file because they share the uniform and
/// because a notation split across three shader files is a notation with three
/// definitions.
const SHADER: &str = r#"
struct Camera { mvp: mat4x4<f32> };
@group(0) @binding(0) var<uniform> camera: Camera;

struct SplatOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) weight: f32,
};

// `live::kernel`: a quartic with compact support. Same bell as a Gaussian, no
// transcendental, and exactly zero at its own edge so adjacent splats do not
// seam.
fn kernel(r2: f32) -> f32 {
    let k = max(1.0 - r2, 0.0);
    return k * k;
}

@vertex
fn vs_splat(
    @builtin(vertex_index) vi: u32,
    @location(0) centre: vec2<f32>,
    @location(1) radius: f32,
    @location(2) weight: f32,
) -> SplatOut {
    var corner = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(1.0, -1.0), vec2<f32>(-1.0, 1.0),
        vec2<f32>(-1.0,  1.0), vec2<f32>(1.0, -1.0), vec2<f32>( 1.0, 1.0),
    );
    let c = corner[vi];
    var o: SplatOut;
    o.pos = camera.mvp * vec4<f32>(centre + c * radius, 0.0, 1.0);
    o.local = c;
    o.weight = weight;
    return o;
}

@fragment
fn fs_splat_r(v: SplatOut) -> @location(0) f32 {
    return kernel(dot(v.local, v.local)) * v.weight;
}

@fragment
fn fs_splat_rgba(v: SplatOut) -> @location(0) vec4<f32> {
    return vec4<f32>(kernel(dot(v.local, v.local)) * v.weight, 0.0, 0.0, 0.0);
}

struct Iso {
    thresholds: vec4<f32>,
    contour: vec4<f32>,
    hatch: vec4<f32>,
    spacing: vec4<f32>,
    widths: vec4<f32>,
    viewport: vec4<f32>,
    tone0: vec4<f32>,
    tone1: vec4<f32>,
    tone2: vec4<f32>,
};
@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var smp: sampler;
@group(0) @binding(2) var<uniform> iso: Iso;

struct Full {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// One oversized triangle, no vertex buffer. NDC to UV flips Y, because wgpu's
// texture origin is top-left while NDC +Y is up.
@vertex
fn vs_full(@builtin(vertex_index) i: u32) -> Full {
    var p = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0),
    );
    var o: Full;
    o.pos = vec4<f32>(p[i], 0.0, 1.0);
    o.uv = vec2<f32>(p[i].x * 0.5 + 0.5, 0.5 - p[i].y * 0.5);
    return o;
}

// One territory's summed field, added into the accumulator: the density in R,
// and one in G wherever this territory *alone* already reaches fringe level.
// The blend is additive, so G ends up as the number of territories claiming the
// texel — PRD §6.4's contention signal, which the sum on its own cannot give.
@fragment
fn fs_accumulate(v: Full) -> @location(0) vec4<f32> {
    let d = textureSample(src, smp, v.uv).r;
    let present = select(0.0, 1.0, d >= iso.thresholds.y);
    return vec4<f32>(d, present, 0.0, 0.0);
}

fn band_at(uv: vec2<f32>) -> i32 {
    let d = textureSample(src, smp, uv).r * iso.thresholds.x;
    // Hard steps, NOT a smoothstep. PRD 10.4: "never a continuous blur".
    if (d >= iso.thresholds.w) { return 2; }
    if (d >= iso.thresholds.z) { return 1; }
    if (d >= iso.thresholds.y) { return 0; }
    return -1;
}

@fragment
fn fs_iso(v: Full) -> @location(0) vec4<f32> {
    let band = band_at(v.uv);
    if (band < 0) { discard; }

    let per_px = vec2<f32>(1.0, 1.0) / iso.viewport.xy;
    let s = iso.contour[band] * per_px;
    // The contour is a boundary in the field, found by comparing a texel's band
    // with its neighbours a stroke-width away. A closed curve of the right
    // thickness for free, and it cannot leak.
    let contour =
        band_at(v.uv + vec2<f32>(s.x, 0.0)) != band ||
        band_at(v.uv - vec2<f32>(s.x, 0.0)) != band ||
        band_at(v.uv + vec2<f32>(0.0, s.y)) != band ||
        band_at(v.uv - vec2<f32>(0.0, s.y)) != band;

    var ink = contour;
    if (!contour) {
        let p = v.pos.xy;
        let crowd = textureSample(src, smp, v.uv).g;
        let contested = crowd >= iso.contour.w - 0.5;
        var spacing = iso.spacing[band];
        let width = iso.widths[band];
        let a = iso.hatch.x * p.x + iso.hatch.y * p.y;
        if (contested) {
            // Contested ground crosses the hatch, at a spacing that opens up so
            // the pair lays down about as much ink as one direction did.
            spacing = spacing * iso.hatch.z;
            let b = iso.hatch.y * p.x - iso.hatch.x * p.y;
            ink = fract(a / spacing) * spacing < width || fract(b / spacing) * spacing < width;
        } else {
            ink = fract(a / spacing) * spacing < width;
        }
    }
    if (!ink) { discard; }
    if (band == 2) { return iso.tone2; }
    if (band == 1) { return iso.tone1; }
    return iso.tone0;
}
"#;
