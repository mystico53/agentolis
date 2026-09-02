//! `polis-render` — the renderer (PRD §10, §13, §14).
//!
//! # This crate takes wgpu through eframe, and declares neither wgpu nor winit
//!
//! `eframe` re-exports `egui`, `egui_wgpu` and `wgpu` from its root, and pins
//! wgpu 30.0.1 and winit 0.30.13. A direct `wgpu = "30.0.1"` dependency silently
//! resolves to a *different* wgpu the day eframe bumps, and the `Device`/`Queue`
//! type mismatch that follows is a notoriously opaque error. Verified by
//! building the probe both ways (ADR-0012). `winit 0.31.0-beta.2` exists on
//! crates.io but eframe has not adopted it; using it would fork the window
//! system.
//!
//! # The shape of this crate is dictated by egui-wgpu's callback contract
//!
//! PRD §14 lists this crate as if it were independent of the UI shell. It is
//! not, and cannot be:
//!
//! * **The offscreen density pass must be encoded in
//!   `CallbackTrait::prepare()`**, using the caller's encoder — never in
//!   `paint()`, which already runs inside egui's render pass and cannot nest one.
//!   This single constraint dictates the whole renderer shape.
//! * **`paint()` receives `RenderPass<'static>`**, so GPU resources cannot be
//!   borrowed from the callback struct and must live in
//!   `egui_wgpu::CallbackResources`, a type-map.
//!
//! So [`PolisRenderer`] exposes an encode-into-the-caller's-encoder entry point
//! and a draw-into-the-caller's-pass entry point, and is storable in a type-map.
//!
//! # PRD §10.3's contrast budget, and where it is decided
//!
//! The budget — layers 1–2 inside the bottom fifth of the contrast range, the
//! rest reserved for layers 4–5 — is **defined and enforced in [`plan`]**, in
//! 8-bit sRGB channel space, as [`plan::BASE_MAP_CEILING`] plus the reserved
//! bands [`plan::CLOUD_BAND`], [`plan::TYPE_BAND`], [`plan::AGENT_BAND`] and
//! [`plan::ATTENTION_BAND`]. Read that module before adding any colour to this
//! crate; the ceiling is a channel bound precisely so that it survives blending
//! and downsampling, which is what makes it enforceable rather than aspirational.
//!
//! [`plan::TYPE_BAND`] is the one that is easy to skip past and is the one a
//! review caught: text is not one of §10.3's five layers, and the first version
//! read that as an exemption and drew district labels at `L 142` — inside the
//! band reserved for live agents, on the image built to validate the budget. It
//! now has an allocation of its own between the clouds and the agents, and
//! `plan`'s `nothing_in_the_map_frame_enters_the_agent_band` asserts it on
//! rendered pixels. The wgpu overlay owes the same rule.
//!
//! ADR-0021 asked for the colour-space convention to be settled before that
//! tuning, and for the PNG path it now is: the CPU rasteriser writes sRGB bytes
//! straight into the file, with no surface format in the way.
//!
//! **The wgpu path still has to reproduce it, and cannot do so by copying
//! constants blindly.** eframe's surface format is non-sRGB and
//! backend-dependent (`Rgba8Unorm` on Vulkan, `Bgra8Unorm` on DX12) while an
//! offscreen `Rgba8UnormSrgb` target encodes differently, so identical shader
//! output looks darker in the window than headless. The budget travels as a
//! *luminance* bound (`L* ≤ 19.9` for layers 1–2); convert it into whatever
//! `render_state.target_format` reports at runtime, and never hardcode the
//! format.

pub mod agents;
pub mod camera;
pub mod city;
pub mod density;
pub mod frame;
pub mod gif;
pub mod live;
pub mod marks;
pub mod plan;
pub mod raster;

use eframe::{egui, egui_wgpu, wgpu};
use polis_world::snapshot::WorldSnapshot;

/// Everything Polis draws with. Lives in `egui_wgpu::CallbackResources`.
#[derive(Debug)]
pub struct PolisRenderer {
    _private: (),
}

impl PolisRenderer {
    /// Builds every pipeline.
    ///
    /// `target_format` must come from `render_state.target_format` at runtime,
    /// never from a constant.
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        target_format: wgpu::TextureFormat,
    ) -> Self {
        let _ = (device, queue, target_format);
        todo!("PRD §13 — build the base-map, density, agent and attention pipelines")
    }

    /// Uploads per-frame state and encodes the **offscreen** passes into the
    /// caller's encoder.
    ///
    /// Called from `CallbackTrait::prepare`. The density pass needs its own
    /// render pass, so it can only be encoded here.
    pub fn prepare(
        &mut self,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        snapshot: &WorldSnapshot,
        camera: &camera::Camera,
    ) {
        let _ = (queue, encoder, snapshot, camera);
        todo!("PRD §10.4 — encode the density pass here, never in paint()")
    }

    /// Draws into egui's render pass.
    ///
    /// Called from `CallbackTrait::paint`. Layer order is PRD §10.3's, bottom to
    /// top: terrain and vacant lots, city, clouds, agents, attention. Clouds are
    /// **beneath** district outlines and labels so the map stays readable.
    pub fn paint(&self, pass: &mut wgpu::RenderPass<'static>) {
        let _ = pass;
        todo!("PRD §10.3 — five layers, bottom to top")
    }

    /// Invalidates the cached base-map texture.
    ///
    /// > **Base map cached to a texture.** The city changes on the order of
    /// > seconds; agents move continuously. Redraw the base only on layout
    /// > change; composite the agent and attention layers per frame. (PRD §13)
    pub fn invalidate_base_map(&mut self) {
        todo!("PRD §13 — redraw the base only on layout change")
    }
}

/// The paint callback that binds a frame's state to [`PolisRenderer`].
///
/// Carries data, never GPU handles: `paint()` takes `RenderPass<'static>`, so
/// nothing borrowed from here can survive into it.
#[derive(Debug)]
pub struct PolisCallback {
    /// Camera for this frame.
    pub camera: camera::Camera,
}

impl egui_wgpu::CallbackTrait for PolisCallback {
    fn prepare(
        &self,
        _device: &wgpu::Device,
        _queue: &wgpu::Queue,
        _screen: &egui_wgpu::ScreenDescriptor,
        _egui_encoder: &mut wgpu::CommandEncoder,
        _resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        todo!("PRD §13 — fetch PolisRenderer from the type-map, then prepare()")
    }

    fn paint(
        &self,
        _info: egui::PaintCallbackInfo,
        _pass: &mut wgpu::RenderPass<'static>,
        _resources: &egui_wgpu::CallbackResources,
    ) {
        todo!("PRD §13 — fetch PolisRenderer from the type-map, then paint()")
    }
}
