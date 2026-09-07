//! Semantic zoom (PRD §12).
//!
//! > three tiers, each a genuinely different representation rather than a scale
//! > factor
//!
//! This module used to also carry an orthographic `Camera` and a `FollowCamera`,
//! declared for the wgpu path PRD §13 specifies and never implemented. Both were
//! `todo!()` bodies; the window's camera is
//! [`polis_app::camera::Camera`](../../polis_app/camera/struct.Camera.html),
//! which works in base-map pixels rather than in a projection matrix because the
//! base map is a raster (ADR-0057, ADR-0110). The tier enum is the one thing the
//! renderer and the window genuinely have to agree about, so it stays here and
//! the window imports it.

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
