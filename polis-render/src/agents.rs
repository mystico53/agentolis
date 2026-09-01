//! Layer 4 — agents, trails and tethers (PRD §10.1, §10.3, §13).
//!
//! > **Shape encodes what, colour encodes how it went. Never conflate them.**
//!
//! > **Interpolate everything.** Events arrive discretely; tween agent positions,
//! > cloud density, and building heights between updates. Cheap, and it is the
//! > entire difference between "alive" and "steppy".
//!
//! Budget: this layer plus [`crate::marks`] must draw in under 4 ms (PRD §13.1).

use eframe::wgpu;
use polis_events::{Glyph, Outcome};
use polis_layout::Point;

/// The agent layer's instance buffers.
#[derive(Debug)]
pub struct AgentLayer {
    _private: (),
}

impl AgentLayer {
    /// Uploads this frame's glyphs, trails and tethers.
    pub fn upload(&mut self, queue: &wgpu::Queue, frame: &AgentFrame) {
        let _ = (queue, frame);
        todo!("PRD §13 — tween positions toward their targets, never snap")
    }

    /// Draws the layer.
    pub fn draw(&self, pass: &mut wgpu::RenderPass<'static>) {
        let _ = pass;
        todo!("PRD §10.3 — layer 4, above clouds and below attention")
    }
}

/// One frame's worth of agent state, already interpolated.
#[derive(Debug, Default)]
pub struct AgentFrame {
    /// Operation marks.
    pub glyphs: Vec<GlyphInstance>,
    /// Trails, one polyline per thread.
    ///
    /// > **Trails persist and fade** whether or not you are following, giving you
    /// > history without a timeline scrubber. (PRD §12)
    ///
    /// PRD §17 open question 3: whether the trail needs an explicit time encoding
    /// (dash density, opacity ramp) or fade alone is enough.
    pub trails: Vec<Vec<Point>>,
    /// Lines joining a worker to its thread.
    pub tethers: Vec<(Point, Point)>,
}

/// One operation mark.
#[derive(Debug, Clone, Copy)]
pub struct GlyphInstance {
    /// Where.
    pub at: Point,
    /// Shape — what the operation *was*.
    pub glyph: Glyph,
    /// Colour — how it *went*. Kept a separate field from `glyph` so the two
    /// channels cannot be conflated by accident.
    pub outcome: Outcome,
    /// Age in seconds, for the fade.
    pub age: f32,
}

/// Scaffolding — a file currently under edit (PRD §8).
///
/// A temporary-looking overlay on the building. It should read as impermanent,
/// which is a rendering decision rather than a data one: the same
/// `FileState::last_touched` drives it and the overgrowth at the other end of
/// the time scale.
#[derive(Debug, Clone, Copy)]
pub struct Scaffolding {
    /// Which building.
    pub at: Point,
    /// How long the file has been under edit.
    pub since_secs: f32,
}
