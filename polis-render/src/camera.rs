//! Camera and semantic zoom (PRD §12).
//!
//! > **Camera**: top-down orthographic. Pan (drag / arrows), zoom (scroll / +-).
//! > No rotation, no tilt.
//!
//! The probe's fixed `[-1, 1]` ortho stretched content in a non-square window, so
//! [`Camera::ortho`] takes all four bounds and callers must feed aspect-corrected
//! values.

use polis_layout::Point;
use polis_world::ThreadStatus;

/// A top-down orthographic camera.
#[derive(Debug, Clone, Copy)]
pub struct Camera {
    /// Centre in city space.
    pub centre: Point,
    /// City-space units across the viewport's shorter axis.
    pub extent: f32,
    /// Viewport aspect ratio, applied so content never stretches.
    pub aspect: f32,
}

impl Camera {
    /// The orthographic matrix for this camera.
    pub fn ortho(&self) -> [[f32; 4]; 4] {
        todo!("PRD §12 — aspect-corrected orthographic projection")
    }

    /// Which representation to draw at the current zoom.
    pub fn tier(&self) -> ZoomTier {
        todo!("PRD §12 — three tiers, each a different representation")
    }

    /// Projects a city-space point to screen space, for the text overlay.
    ///
    /// > **Text lives in a UI overlay, not the GPU layer.** Project world
    /// > coordinates to screen and position `egui` labels. (PRD §13)
    pub fn to_screen(&self, at: Point, viewport: (f32, f32)) -> (f32, f32) {
        let _ = (at, viewport);
        todo!("PRD §13 — world to screen, for egui label placement")
    }
}

/// Semantic zoom (PRD §12).
///
/// > three tiers, each a genuinely different representation rather than a scale
/// > factor
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ZoomTier {
    /// Districts, monuments, skyline, clouds. Buildings are sub-pixel and are
    /// **not drawn individually**.
    City,
    /// Buildings, streets, workers, trails.
    District,
    /// The file detail panel: recent operations, which threads touched it, diff
    /// size, verification status.
    Building,
}

/// Binds the camera to a thread (PRD §12).
///
/// > **Cut, do not pan.** Agents jump discontinuously across the tree; a camera
/// > that smoothly travels from `src/auth` to `docs/` spends most of its life
/// > showing empty space.
#[derive(Debug, Default)]
pub struct FollowCamera {
    _private: (),
}

impl FollowCamera {
    /// Recentres on a thread's current focus, cutting rather than panning.
    pub fn follow(&mut self, camera: &mut Camera, focus: Point, status: ThreadStatus) {
        let _ = (camera, focus, status);
        todo!("PRD §12 — cut, never interpolate the position")
    }
}
