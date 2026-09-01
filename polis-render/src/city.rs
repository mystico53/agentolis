//! The base map — layers 1 and 2 (PRD §10.3, §13).
//!
//! > **Base map cached to a texture.** The city changes on the order of seconds;
//! > agents move continuously. Redraw the base only on layout change; composite
//! > the agent and attention layers per frame.
//!
//! > **Buildings** are irregular polygons — triangulate once (`lyon`), cache in a
//! > vertex buffer, batch into a single draw call. A few thousand irregular
//! > buildings is nothing.
//!
//! # Dynamic range, not omission (PRD §10.3)
//!
//! > Draw everything, but keep layers 1–2 inside roughly the bottom fifth of the
//! > contrast range and spend the rest on layers 4–5. Weather charts do exactly
//! > this: the coastline is always drawn and always faint; the storm system gets
//! > the ink.
//!
//! So nothing here is ever culled for being unimportant — it is drawn faint. The
//! only culling is spatial (off-screen) and semantic (sub-pixel buildings at the
//! city tier).

use eframe::wgpu;
use polis_layout::CityLayout;

/// Triangulated, cached city geometry.
#[derive(Debug)]
pub struct BaseMap {
    _private: (),
}

impl BaseMap {
    /// Triangulates a layout with `lyon` and uploads one interleaved vertex
    /// buffer plus one index buffer.
    pub fn build(device: &wgpu::Device, layout: &CityLayout) -> Self {
        let _ = (device, layout);
        todo!("PRD §13 — lyon once, one batched draw_indexed")
    }

    /// Re-renders the cached texture. Only on layout change.
    pub fn refresh(&mut self, encoder: &mut wgpu::CommandEncoder) {
        let _ = encoder;
        todo!("PRD §13 — redraw the base only when the city actually changed")
    }

    /// Composites the cached texture into the caller's pass.
    pub fn draw(&self, pass: &mut wgpu::RenderPass<'static>) {
        let _ = pass;
        todo!("PRD §10.3 — layers 1-2, bottom fifth of the contrast range")
    }
}

/// Label placement and decluttering for the text overlay (PRD §13).
///
/// > It also gives you label collision and decluttering, which is the one
/// > genuinely hard thing a map engine like MapLibre would have bought you.
///
/// Monuments are exempt: they are **always labelled at every zoom**, because they
/// are the wayfinding layer in an organic city (PRD §8).
#[derive(Debug, Default)]
pub struct LabelLayer {
    _private: (),
}

impl LabelLayer {
    /// Chooses which labels survive at this zoom, in screen space.
    pub fn declutter(&mut self, candidates: &[LabelCandidate]) -> Vec<usize> {
        let _ = candidates;
        todo!("PRD §13 — collision-driven decluttering; monuments never drop out")
    }
}

/// One label the overlay could draw.
#[derive(Debug, Clone)]
pub struct LabelCandidate {
    /// Text.
    pub text: String,
    /// Screen position.
    pub at: (f32, f32),
    /// Screen-space bounds.
    pub size: (f32, f32),
    /// Monuments and district names survive decluttering; building names do not.
    pub priority: LabelPriority,
}

/// Which labels are load-bearing for wayfinding (PRD §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LabelPriority {
    /// Never dropped. Monuments are the orientation anchors.
    Monument,
    /// The district skeleton, which must stay readable at all zooms.
    District,
    /// Dropped first.
    Building,
}
