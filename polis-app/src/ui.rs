//! The egui overlay (PRD §12, §13).
//!
//! > **Text lives in a UI overlay, not the GPU layer.** Project world
//! > coordinates to screen and position DOM/`egui` labels. Text rendering is the
//! > classic time sink in custom renderers, and skipping it entirely gets you
//! > crisp glyphs and free styling.
//!
//! Everything with a glyph in it lives here. The wgpu layer draws only geometry.

use eframe::egui;
use polis_world::snapshot::WorldSnapshot;

/// The overlay's persistent state: selection, panel visibility, hover.
#[derive(Debug, Default)]
pub struct Overlay {
    _private: (),
}

impl Overlay {
    /// Draws every text element over the map layer.
    pub fn draw(&mut self, ui: &mut egui::Ui, snapshot: &WorldSnapshot) {
        let _ = (ui, snapshot);
        todo!("PRD §13 — labels, status rail, status bar, drill-down")
    }

    /// The file detail panel (PRD §12, building tier).
    ///
    /// > **Fuzzy above, exact below.** Soft cloud edges are honest for the
    /// > ambient layer and useless when acting. Hovering a building must give a
    /// > definite list of which threads touched it and when.
    pub fn building_panel(&mut self, ui: &mut egui::Ui, snapshot: &WorldSnapshot) {
        let _ = (ui, snapshot);
        todo!("PRD §12 — recent operations, threads that touched it, diff size, verification")
    }

    /// The linked filesystem tree (PRD §12).
    ///
    /// > **Linked filesystem view** — a plain tree, co-equal with the map, not a
    /// > fallback. Shared selection and highlight state; one keystroke swaps.
    /// > Both are renderings of one data structure. The map is a lossy projection
    /// > and the operator will need ground truth to check it against.
    pub fn tree_view(&mut self, ui: &mut egui::Ui, snapshot: &WorldSnapshot) {
        let _ = (ui, snapshot);
        todo!("PRD §12 — co-equal with the map, shared selection")
    }

    /// The status rail: one row per thread, including threads whose territory has
    /// not converged and therefore have no cloud yet (PRD §6.2).
    pub fn status_rail(&mut self, ui: &mut egui::Ui, snapshot: &WorldSnapshot) {
        let _ = (ui, snapshot);
        todo!("PRD §6.2 — unplaced threads live here until their territory converges")
    }

    /// The status bar: dropped events, degraded channels, schema drift
    /// (PRD §4.5, §17).
    ///
    /// > a schema-drift warning in the status bar rather than a crash
    ///
    /// It must also show when subagent attribution is degraded, because a Polis
    /// silently attributing every worker's edits to its main agent looks exactly
    /// like a Polis that is working.
    pub fn status_bar(&mut self, ui: &mut egui::Ui, snapshot: &WorldSnapshot) {
        let _ = (ui, snapshot);
        todo!("PRD §4.5 — dropped counter, degraded channels, drift, attribution state")
    }
}
