# Polis rendering stack — verified

**Status: GREEN.** All four required proofs compiled and executed on real hardware.
Nothing in this document is from memory; every claim is either compiler-enforced
(the probe compiles) or printed by the probe at runtime.

Probe: `C:/Users/konka/AppData/Local/Temp/claude/C--coding-agentolis/6f51089f-1ec0-4e78-9bc9-8ff6864e0100/scratchpad/gpu-probe`
Two modes: `gpu-probe.exe window` (eframe window) and `gpu-probe.exe headless`
(offscreen → PNG + numeric assertions). Both exit 0.

| # | Claim (PRD) | Proof | Result |
|---|---|---|---|
| a | eframe window on the **WGPU** renderer, not glow | `window` mode printed the adapter from `cc.wgpu_render_state` | PASS |
| b | `egui_wgpu::CallbackTrait` custom pass, lyon polygons, one vertex buffer, one draw call, ortho uniform | both modes; `draw_indexed(0..39, 0, 0..1)` | PASS |
| c | §10.4 cloud renderer: additive Gaussian splats → offscreen **R16Float** → thresholded into discrete iso-bands | both modes + numeric readback assertions | PASS |
| d | egui text composited **on top of** the custom wgpu layer | `polis-window.png` | PASS |

Evidence images (regenerated on every run):
- `…/scratchpad/gpu-probe/polis-probe.png` — headless, 512², Rgba8UnormSrgb
- `…/scratchpad/gpu-probe/polis-window.png` — windowed screenshot, 1575×1225

---

## 1. The version set (read from `Cargo.lock`, not guessed)

**eframe 0.36.1 pins wgpu 30.0.1 and winit 0.30.13.** wgpu 30.0.1 happens to be
the current crates.io release, so there is no downgrade to absorb. winit is
**0.30.13, not the 0.31.0-beta.2** on crates.io — eframe has not moved to 0.31,
so polis must not either.

| Crate | Resolved | Note |
|---|---|---|
| eframe | 0.36.1 | direct |
| egui / epaint / ecolor / emath | 0.36.1 | via eframe |
| egui-wgpu / egui-winit | 0.36.1 | via eframe |
| **wgpu / wgpu-core / wgpu-hal / wgpu-types / naga** | **30.0.1** | via egui-wgpu |
| **winit** | **0.30.13** | via eframe + egui-winit |
| lyon | 1.0.19 (`lyon_tessellation` 1.0.21) | direct |
| bytemuck | 1.25.2 | direct (`1.24` requested) |
| image | 0.25.10 | direct, `png` only |
| raw-window-handle | 0.6.2 | shared by eframe + wgpu |
| ash | 0.38.0+1.3.281 | Vulkan backend |
| windows | 0.62.2 | DX12 backend |

305 packages total. Clean `--locked` release build from an empty `target/`: see §9.

### Do not add a direct `wgpu` dependency

`eframe` re-exports everything needed:

```rust
// eframe 0.36.1 src/lib.rs:162
pub use {egui_wgpu, egui_wgpu::SurfaceConfig, egui_wgpu::WgpuConfiguration, egui_wgpu::wgpu};
```

The probe was **converted to this form and recompiled** to prove it works,
including `wgpu::util::DeviceExt`. A direct `wgpu = "30.0.1"` in `polis-render`
would silently resolve to a *different* wgpu the day eframe bumps, and the
resulting `Device` type mismatch is a famously opaque error. Use:

```rust
use eframe::{egui, egui_wgpu, wgpu};
```

### `glow` in the tree is a false alarm

`cargo tree` shows `glow 0.17.0`. This is **not** eframe's glow renderer — it is
`wgpu-hal`'s GLES backend, pulled in unconditionally by wgpu-hal:

```
glow v0.17.0 └── wgpu-hal v30.0.1 └── wgpu v30.0.1 └── egui-wgpu v0.36.1 └── eframe
```

Seeing it does not mean you are on the GL path. `eframe::Renderer::Wgpu` plus
`cc.wgpu_render_state.is_some()` is the actual check, and the probe asserts it.

---

## 2. MSRV — this contradicts the environment brief

**The egui 0.36 family requires rustc ≥ 1.95.** The machine's `stable` is
**1.94.0**, which cannot build it. Verified by running the build, not by reading
the manifest:

```
$ cargo +stable build --release
error: rustc 1.94.0 is not supported by the following packages:
  ecolor@0.36.1 requires rustc 1.95
  eframe@0.36.1 requires rustc 1.95
  egui@0.36.1 requires rustc 1.95
  egui-wgpu@0.36.1 requires rustc 1.95
  egui-winit@0.36.1 requires rustc 1.95
  emath@0.36.1 requires rustc 1.95
  epaint@0.36.1 requires rustc 1.95
  epaint_default_fonts@0.36.1 requires rustc 1.95
```

wgpu 30.0.1 itself only needs 1.87.0 and winit 0.30.13 needs 1.70.0 — the floor
comes entirely from egui.

The probe carries a `rust-toolchain.toml` pinning **1.98.0** (installed on this
box), which is what makes it build. `polis` needs the same file at the workspace
root, or CI will fail on the default toolchain.

---

## 3. Adapter and backend actually selected

Run on **AMD Radeon RX 9070 XT**, DiscreteGpu. Both backends were exercised
end-to-end (`WGPU_BACKEND=dx12` / default) in both modes:

| Backend | Driver string | Surface format chosen by eframe |
|---|---|---|
| **Vulkan** (default pick) | `AMD proprietary driver 26.8.1 (LLPC)` | `Rgba8Unorm` |
| Dx12 | `32.0.31041.1004` | `Bgra8Unorm` |

Two things follow, both of which will bite an implementer:

1. **wgpu 30 prefers Vulkan over DX12 on Windows.** If you want DX12 you must ask
   for it. `wgpu::InstanceDescriptor::new_without_display_handle_from_env()`
   honours `WGPU_BACKEND`, which is how the probe A/B-tested them.
2. **The surface format differs by backend and is non-sRGB in both cases.** Never
   hardcode a target format; always take `render_state.target_format` and thread
   it into every `ColorTargetState` that draws to the screen. The probe's
   `PolisRenderer::new` takes it as a parameter for exactly this reason.

Because the surface is `*Unorm` (linear) and not `*UnormSrgb`, colours written by
a custom callback are **not** sRGB-encoded by the hardware. The identical shader
output looks visibly darker in the window than in the headless
`Rgba8UnormSrgb` render. egui compensates internally for its own meshes; your
callback must do its own conversion if you want the two to agree.

---

## 4. R16Float — supported here, with a fallback path anyway

The PRD specifies R16F. On this adapter it is fully supported and **no fallback
was needed**. Probe output, verbatim:

```
R16Float allowed_usages : TextureUsages(COPY_SRC | COPY_DST | TEXTURE_BINDING
                          | STORAGE_BINDING | RENDER_ATTACHMENT | TRANSIENT_ATTACHMENT)
R16Float flags          : TextureFormatFeatureFlags(FILTERABLE | MULTISAMPLE_X2
                          | MULTISAMPLE_X4 | MULTISAMPLE_X8 | MULTISAMPLE_RESOLVE
                          | STORAGE_READ_ONLY | STORAGE_WRITE_ONLY
                          | STORAGE_READ_WRITE | BLENDABLE)
chosen                  : R16Float
fell back               : false
```

`RENDER_ATTACHMENT` ✓, `TEXTURE_BINDING` ✓, `BLENDABLE` ✓, `FILTERABLE` ✓ — the
four properties the cloud renderer needs. Keep the runtime check anyway; it is
six lines and R16F blendability is not universal on older integrated parts:

```rust
pub fn pick_density_format(adapter: &wgpu::Adapter) -> DensityFormatReport {
    let f = adapter.get_texture_format_features(wgpu::TextureFormat::R16Float);
    let need_usage = wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING;
    let need_flags = wgpu::TextureFormatFeatureFlags::BLENDABLE
        | wgpu::TextureFormatFeatureFlags::FILTERABLE;
    let ok = f.allowed_usages.contains(need_usage) && f.flags.contains(need_flags);
    // Rgba16Float is the fallback: 4x the memory, same semantics.
    DensityFormatReport { chosen: if ok { R16Float } else { Rgba16Float }, /* … */ }
}
```

**Fallback is `Rgba16Float`** (guaranteed blendable + filterable in core WebGPU).
If you take it, the splat fragment shader's return type must change from `f32` to
`vec4<f32>` — see §6.

`R16Float` is filterable in **core** WebGPU. Only 32-bit float formats need the
`FLOAT32_FILTERABLE` feature, so the density sampler can be
`SamplerBindingType::Filtering` with `TextureSampleType::Float { filterable: true }`
and no feature request. The probe requests `Features::empty()` and
`Limits::downlevel_defaults()` and still works — the cloud renderer needs no
optional capability at all.

---

## 5. The finding that actually matters: the density field is unbounded

This is the part a reasonable implementation gets wrong, and the probe caught it
by asserting on a texture readback rather than by looking at the picture.

Additive blending into a float target means **N overlapping kernels sum to ≈N,
not to 1.** A single full-weight Gaussian in this probe peaks at
`exp(0) − exp(−4.5) = 0.98889`. With a cluster of three near-coincident kernels
(a hot file being edited by three threads), the measured field is:

```
== DENSITY FIELD ==
  max value          : 2.5371
  texels above 1.0   : 1750
```

So the iso thresholds **cannot** be absolute constants in the shader. Two
consequences:

**(1) Thresholds must be uniforms, not WGSL `const`.** Otherwise retuning them
costs a pipeline rebuild. The probe moved them into an `IsoParams` uniform at
`@group(0) @binding(2)`.

**(2) Normalising by the observed field maximum is a trap.** The probe rendered
both ways. With `inv_scale = 1/2.54` (normalise by the max), the single hot
cluster consumed the entire range and *every other territory collapsed into one
fringe band* — the map went flat, which is precisely the failure §10.4 is trying
to avoid. The correct default is a **fixed reference**: `inv_scale = 1.0`, i.e.
"one full-weight kernel == 1.0", and let hot spots clip into the core band. Band
pixel counts under the two settings:

| | fringe | body | core |
|---|---|---|---|
| normalise by max (`1/2.7`) | 17892 | 1725 | 422 |
| fixed reference (`1.0`) | 17892 | 11063 | 2296 |

The fixed reference is what the probe ships with.

### Verifying the blend is genuinely additive

Before the hot cluster existed, the GPU field maxed at `0.9888` — suspiciously
equal to one lone splat's peak. Rather than assume, the same field was modelled
in Python on the CPU: it also produced `0.9888` at `(−0.448, −0.299)`. GPU and
CPU agreeing to four decimals proves the blend sums correctly and that nothing
clamps; the scene simply had no real overlap yet. Keep that CPU cross-check in
`polis-render`'s test suite — it is the only way to distinguish "additive
blending is broken" from "your kernels do not overlap."

### The iso-bands are discrete — measured, not eyeballed

The headless run histograms every RGB triple in the frame:

```
== READBACK ==
  pixels: 262144
  distinct RGB values: 7
```

Exactly 7 = background + 3 iso-bands + 3 polygon fills. A continuous gradient
would produce thousands. The probe asserts `hist.len() <= 24`, which is a
regression test for "somebody replaced the `if` chain with a `smoothstep`."

---

## 6. WGSL and pipeline specifics implementers must get right

### Splat fragment output must match the target's component count

Naga validates the fragment return type against the colour attachment format.
With an `R16Float` attachment the shader **must** return a scalar:

```wgsl
@fragment
fn fs_splat(v: SplatOut) -> @location(0) f32 {
    let d2 = dot(v.local, v.local);
    // Gaussian truncated at the quad edge so adjacent splats do not seam.
    let g = exp(-4.5 * d2) - exp(-4.5);
    return max(g, 0.0) * v.weight;
}
```

Returning `vec4<f32>` here is a validation error. If you fall back to
`Rgba16Float` you must flip it back to `vec4<f32>`.

### Additive blend state (the whole trick)

```rust
blend: Some(wgpu::BlendState {
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
}),
```

`One/One/Add` on both channels. The density pass clears to
`LoadOp::Clear(Color::TRANSPARENT)` so the sum starts at zero.

### Texture usage flags

```rust
// density target
usage: TextureUsages::RENDER_ATTACHMENT   // splat into it
     | TextureUsages::TEXTURE_BINDING     // sample it in the iso pass
// (+ COPY_SRC in the probe only, for the readback assertion)
```

Production needs exactly those two.

### Bind group layouts

Two groups, both at index 0 of their own pipeline layout:

| Pipeline | group(0) bindings |
|---|---|
| poly, splat | `0` uniform `Camera` (mat4x4), **`ShaderStages::VERTEX`** |
| iso | `0` `texture_2d<f32>` filterable · `1` `Sampler(Filtering)` · `2` uniform `IsoParams` — all **`FRAGMENT`** |

The poly and splat pipelines deliberately share one `PipelineLayout`, so the
camera bind group is set once and reused across both.

### Iso-band thresholding shader

```wgsl
struct IsoParams { inv_scale: f32, t0: f32, t1: f32, t2: f32 };
@group(0) @binding(0) var density_tex: texture_2d<f32>;
@group(0) @binding(1) var density_smp: sampler;
@group(0) @binding(2) var<uniform> iso: IsoParams;

@fragment
fn fs_iso(v: IsoOut) -> @location(0) vec4<f32> {
    let d = textureSample(density_tex, density_smp, v.uv).r * iso.inv_scale;
    var rgb = vec3<f32>(0.0);
    var a   = 0.0;
    // Hard steps, NOT a smoothstep. §10.4: "never a continuous blur".
    if (d >= iso.t2)      { rgb = vec3<f32>(0.36, 0.85, 0.80); a = 0.55; } // core
    else if (d >= iso.t1) { rgb = vec3<f32>(0.24, 0.56, 0.70); a = 0.38; } // body
    else if (d >= iso.t0) { rgb = vec3<f32>(0.16, 0.32, 0.48); a = 0.22; } // fringe
    else                  { discard; }
    return vec4<f32>(rgb * a, a);   // premultiplied, matches the blend state
}
```

`discard` below `t0` is what lets the city show through; the alternative
(returning `a = 0.0`) also works but still pays the blend.

### Fullscreen pass and the UV flip

The iso pass is a 3-vertex oversized triangle with **no vertex buffer**
(`buffers: &[]`, `draw(0..3, 0..1)`). NDC→UV must flip Y, because wgpu's texture
origin is top-left while NDC +Y is up:

```wgsl
o.uv = vec2<f32>(p.x * 0.5 + 0.5, 0.5 - p.y * 0.5);
```

### Splat quads are instanced with no vertex data

6 vertices generated from `@builtin(vertex_index)`, one instance per splat, with
the instance buffer stepping `VertexStepMode::Instance`:
`rp.draw(0..6, 0..splat_count)`. 4 floats per splat (`center.xy`, `radius`,
`weight`), so a hundred kernels is 1.6 KB — the PRD's "free" is correct.

### Camera uniform is column-major

`[[f32; 4]; 4]` maps to WGSL `mat4x4<f32>` column-major, so the translation lives
in the **last row** of the Rust literal, not the last column.

---

## 7. API signatures that changed — the stale-memory hazards

These are the things that broke the probe. All signatures below are
compiler-verified against eframe 0.36.1 / wgpu 30.0.1.

### eframe 0.36: `App::ui`, not `App::update`

```rust
impl eframe::App for ProbeApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) { … }
}
```

There is **no `update(&mut self, ctx: &egui::Context, frame: &mut Frame)`** and
**no `CentralPanel::default().show(ctx, …)`**. The `Ui` you are handed *is* the
central panel, with no margin and no background — if you want a background you
paint it yourself. Get the context with `ui.ctx().clone()`.

### `egui_wgpu::CallbackTrait`

```rust
fn prepare(
    &self,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    screen: &egui_wgpu::ScreenDescriptor,
    egui_encoder: &mut wgpu::CommandEncoder,
    resources: &mut egui_wgpu::CallbackResources,
) -> Vec<wgpu::CommandBuffer>;

fn paint(
    &self,
    info: egui::PaintCallbackInfo,
    render_pass: &mut wgpu::RenderPass<'static>,   // note: 'static
    resources: &egui_wgpu::CallbackResources,
);
```

`paint` takes `RenderPass<'static>` — that lifetime is why GPU resources cannot
be borrowed from the callback struct and must live in `CallbackResources` (a
type-map) instead. Insert them once at startup:

```rust
cc.wgpu_render_state.as_ref().unwrap()
  .renderer.write().callback_resources.insert(renderer);
```

and fetch with `resources.get::<T>()` / `get_mut::<T>()`.

Attach with `egui_wgpu::Callback::new_paint_callback(rect, cb)` pushed onto a
painter: `ui.painter().add(...)`.

**You cannot begin a render pass inside `paint`** — you are already in egui's.
The offscreen density pass therefore has to be encoded in `prepare`, using the
`egui_encoder` you are given. That ordering is the single most important
structural fact in this document.

### egui 0.36 layout API

`ui.allocate_ui_at_rect` is gone. Use:

```rust
ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| { … });
```

### wgpu 30 changes

| Item | wgpu 30 form |
|---|---|
| `VertexState::buffers` | `&[Option<VertexBufferLayout>]` — wrap each in `Some(..)` |
| `PipelineLayoutDescriptor::bind_group_layouts` | `&[Option<&BindGroupLayout>]` |
| `PipelineLayoutDescriptor` | gained `immediate_size: 0` |
| `VertexState/FragmentState::entry_point` | `Option<&str>` — `Some("vs_main")` |
| `Device::on_uncaptured_error` | takes `Arc<dyn UncapturedErrorHandler>`, **not** `Box` |
| `InstanceDescriptor` | no `Default`; use `new_without_display_handle_from_env()` |
| `RenderPassColorAttachment` | gained `depth_slice: None` |
| `RenderPipelineDescriptor` / `RenderPassDescriptor` | gained `multiview_mask: None` |
| `Device::poll` | returns `Result`; use `wgpu::PollType::wait_indefinitely()` |
| `BufferSlice::get_mapped_range` | returns `Result` |
| `Adapter::request_adapter` / `request_device` | `async`, return `Result` (not `Option`) |
| copy structs | `TexelCopyTextureInfo` / `TexelCopyBufferInfo` / `TexelCopyBufferLayout` (ex-`ImageCopy*`) |
| sampler mipmap field | `MipmapFilterMode`, not `FilterMode` |

---

## 8. Layer ordering (proof d)

egui composites paint commands in **submission order**. The probe adds the wgpu
callback first and the labels after, and `polis-window.png` shows
`src/auth/session.rs` and `tests/integration.rs` legible over both the clouds and
the polygons. This validates §13's "text lives in a UI overlay, not the GPU
layer" — no glyph atlas work is needed anywhere in polis.

Within the custom pass the order is clouds → polygons, matching §10.3 ("clouds
rendered beneath district outlines").

---

## 9. Reproducibility

The probe was copied to a fresh directory with an empty `target/` and rebuilt
with `cargo build --release --locked`, to confirm the lockfile alone reproduces
the build with no stale artifacts. **Cold release build: 47.1 s** on 24 cores,
exit 0. The resulting binary produced numerically identical output to the
incremental one (`max 2.5371`, `1750` texels above 1.0, 7 distinct colours, same
per-band pixel counts), so the pipeline is deterministic across builds — which
matters given §7.4's hard determinism requirement.

Both `headless` and `window` were also re-run after the switch to eframe's
re-exported wgpu, and both still exit 0.

**Zero wgpu validation errors were hit in the final build.** Headless installs a
hard `on_uncaptured_error` handler that `exit(3)`s, and 120 windowed frames ran
clean. The three errors encountered along the way were all Rust-level API drift,
listed in §7 — not validation failures.

---

## 10. Known gaps for `polis-render`

Not blockers, but do not rediscover them:

- **Aspect ratio.** The probe's ortho box is a fixed `[-1,1]²`, so content
  stretches in a non-square window (visible in `polis-window.png` at 1575×1225).
  `ortho(l, r, b, t)` already takes the four bounds — feed it aspect-corrected
  values from `ScreenDescriptor`.
- **No MSAA.** `MultisampleState::default()` (1 sample). Polygon edges are hard.
  R16Float advertises `MULTISAMPLE_X2/X4/X8 | MULTISAMPLE_RESOLVE` on this
  adapter if the density pass ever wants it.
- **Density texture is fixed 512²** and not resized with the window. §10.4
  specifies 512², so this is probably correct as-is.
- **sRGB mismatch** between the `Rgba8Unorm` window surface and the
  `Rgba8UnormSrgb` headless target — see §3. Pick one convention before tuning
  the §10.3 contrast budget, or the palette work will be done twice.
- **Cloud cap** (§10.4 "cap the number of visible clouds") is not implemented;
  the probe draws every splat unconditionally.
