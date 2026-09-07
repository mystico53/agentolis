//! The map camera (PRD §12).
//!
//! > **Camera**: top-down orthographic. Pan (drag / arrows), zoom (scroll /
//! > `+-`). No rotation, no tilt.
//!
//! That is not a preference and this type is where it is enforced: there is no
//! angle anywhere in it. A camera is a centre, a scale and a viewport, and the
//! transform it produces is a translation and a uniform scale — the only
//! transform a top-down orthographic map can have.
//!
//! # Which space this camera lives in
//!
//! Not world space. The base map is a raster produced by
//! [`polis_render::plan::render_base_map`], and the one transform that has to be
//! exact is the one between that raster and the screen — a millimetre of drift
//! there and the vector overlay peels away from the picture underneath it. So
//! the camera's units are **base-map pixels**, and world coordinates reach the
//! screen through [`crate::basemap::BaseMap::to_map`] first. That composition is
//! `world → map px → screen px`, and every layer in the window uses it, which is
//! why the overlay lines up with the texture at every zoom.
//!
//! This is the only camera in the tree. `polis-render` once declared a second
//! one — an orthographic projection for the wgpu pass PRD §13 specifies — but
//! that pass was never implemented and both were removed (ADR-0110). What the
//! renderer and the window still have to agree about is [`ZoomTier`], which is
//! why that enum is imported from there rather than redefined here.

use eframe::egui::{self, Pos2, Rect, Vec2};

pub use polis_render::zoom::ZoomTier;

/// How far out the camera may zoom, as a multiple of the fit-the-whole-city
/// scale.
///
/// Below 1.0 the city no longer fills the window. It is allowed because the
/// [`ZoomTier::City`] representation — districts and monuments, buildings not
/// drawn individually — is only honest once a building really is a few pixels
/// across, and on a 120-file repository that does not happen until the city is
/// small on screen.
pub const MIN_ZOOM_FACTOR: f32 = 0.15;

/// How far in the camera may zoom, as a multiple of the fit scale.
pub const MAX_ZOOM_FACTOR: f32 = 60.0;

/// One notch of the scroll wheel, or one press of `+`.
pub const ZOOM_STEP: f32 = 1.15;

/// Arrow-key pan, in screen pixels per press (times `dt` for held keys).
pub const KEY_PAN_SPEED: f32 = 900.0;

/// A building narrower than this on screen is sub-pixel enough that
/// [`ZoomTier::City`] is the honest representation (PRD §12).
pub const CITY_TIER_BUILDING_PX: f32 = 4.5;

/// A building this wide on screen can carry its own label and its own detail
/// panel: [`ZoomTier::Building`].
pub const BUILDING_TIER_BUILDING_PX: f32 = 30.0;

/// A top-down orthographic camera over the base map.
#[derive(Debug, Clone, Copy)]
pub struct Camera {
    /// The base-map pixel under the centre of the viewport.
    centre: Pos2,
    /// Screen pixels per base-map pixel.
    scale: f32,
    /// The base map's edge length, in base-map pixels.
    map_edge: f32,
    /// The screen rectangle the map is drawn into. Refreshed once per frame.
    viewport: Rect,
    /// A typical building's diameter in base-map pixels, which is what makes
    /// [`Camera::tier`] a statement about apparent size rather than about a
    /// zoom number.
    building_px: f32,
}

impl Camera {
    /// A camera showing the whole city.
    pub fn fit(map_edge: f32, building_px: f32, viewport: Rect) -> Self {
        let mut camera = Self {
            centre: Pos2::new(map_edge * 0.5, map_edge * 0.5),
            scale: 1.0,
            map_edge,
            viewport,
            building_px: building_px.max(0.05),
        };
        camera.scale = camera.fit_scale();
        camera
    }

    /// The scale at which the whole city just fits the viewport.
    pub fn fit_scale(&self) -> f32 {
        let w = self.viewport.width().max(1.0);
        let h = self.viewport.height().max(1.0);
        (w / self.map_edge).min(h / self.map_edge).max(1e-4)
    }

    /// Refreshes the viewport, keeping the same map point under the centre and
    /// the same zoom *relative to fit*.
    ///
    /// Called once per frame. Two things must survive a resize: the map must not
    /// teleport, so the centre is preserved; and a window twice as tall must
    /// show the map twice as large rather than the same map with more empty sea
    /// around it, so it is the zoom factor that is preserved and not the
    /// absolute scale. Every map application behaves this way and the first
    /// version of this method did not, which showed up the moment the window was
    /// resized: the city stayed the size it had been and drifted into a corner.
    pub fn set_viewport(&mut self, viewport: Rect) {
        if viewport == self.viewport {
            return;
        }
        let zoom = self.zoom();
        self.viewport = viewport;
        self.scale = (self.fit_scale() * zoom).clamp(
            self.fit_scale() * MIN_ZOOM_FACTOR,
            self.fit_scale() * MAX_ZOOM_FACTOR,
        );
        self.clamp();
    }

    /// The screen rectangle the map is drawn into.
    pub fn viewport(&self) -> Rect {
        self.viewport
    }

    /// Screen pixels per base-map pixel.
    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// Zoom as a multiple of the fit scale — what the status bar shows.
    pub fn zoom(&self) -> f32 {
        self.scale / self.fit_scale()
    }

    /// The base-map pixel under the centre of the viewport.
    pub fn centre(&self) -> Pos2 {
        self.centre
    }

    /// Base-map pixel to screen pixel.
    pub fn to_screen(&self, map: Pos2) -> Pos2 {
        self.viewport.center() + (map - self.centre) * self.scale
    }

    /// Screen pixel to base-map pixel.
    pub fn to_map(&self, screen: Pos2) -> Pos2 {
        self.centre + (screen - self.viewport.center()) / self.scale
    }

    /// The screen rectangle the whole base map occupies. This is the rectangle
    /// the texture is blitted into, so it is also the definition of "the overlay
    /// lines up".
    pub fn map_rect(&self) -> Rect {
        Rect::from_min_max(
            self.to_screen(Pos2::ZERO),
            self.to_screen(Pos2::new(self.map_edge, self.map_edge)),
        )
    }

    /// The part of the base map currently on screen, in base-map pixels. Used
    /// to cull every vector layer to the visible rectangle.
    pub fn visible_map_rect(&self) -> Rect {
        Rect::from_min_max(
            self.to_map(self.viewport.min),
            self.to_map(self.viewport.max),
        )
    }

    /// Pans by a screen-space delta (a drag, or an arrow key).
    pub fn pan(&mut self, screen_delta: Vec2) {
        self.centre -= screen_delta / self.scale;
        self.clamp();
    }

    /// Cuts to a base-map point without interpolating (PRD §12: follow a thread
    /// is a **cut**, never a pan — "a camera that smoothly travels from
    /// `src/auth` to `docs/` spends most of its life showing empty space").
    pub fn cut_to(&mut self, map: Pos2) {
        self.centre = map;
        self.clamp();
    }

    /// Zooms about a screen point, so the map pixel under the cursor stays put.
    pub fn zoom_by(&mut self, factor: f32, anchor: Pos2) {
        let fit = self.fit_scale();
        let before = self.to_map(anchor);
        self.scale = (self.scale * factor).clamp(fit * MIN_ZOOM_FACTOR, fit * MAX_ZOOM_FACTOR);
        let after = self.to_map(anchor);
        self.centre -= after - before;
        self.clamp();
    }

    /// Sets the zoom as a multiple of the fit scale, about the viewport centre.
    pub fn set_zoom(&mut self, zoom: f32) {
        let target = self.fit_scale() * zoom;
        let factor = target / self.scale;
        let centre = self.viewport.center();
        self.zoom_by(factor, centre);
    }

    /// Which representation to draw (PRD §12).
    ///
    /// Keyed on how wide a typical building is **on screen**, not on the zoom
    /// number: a 5 000-file repository has buildings a few pixels across when it
    /// fits the window, and a 120-file one does not, and the tier is a claim
    /// about what the operator can actually resolve.
    pub fn tier(&self) -> ZoomTier {
        let px = self.building_px * self.scale;
        if px < CITY_TIER_BUILDING_PX {
            ZoomTier::City
        } else if px < BUILDING_TIER_BUILDING_PX {
            ZoomTier::District
        } else {
            ZoomTier::Building
        }
    }

    /// A typical building's diameter in screen pixels — the number the tier is
    /// decided on, shown in the status bar so the tier is never mysterious.
    pub fn building_screen_px(&self) -> f32 {
        self.building_px * self.scale
    }

    /// Keeps the city from being panned off the edge of the world.
    ///
    /// A margin of half a viewport is allowed on each side, so a building at the
    /// city limit can still be centred.
    fn clamp(&mut self) {
        let half = self.viewport.size() * 0.5 / self.scale;
        let lo = Pos2::new(-half.x, -half.y);
        let hi = Pos2::new(self.map_edge + half.x, self.map_edge + half.y);
        self.centre.x = self.centre.x.clamp(lo.x, hi.x);
        self.centre.y = self.centre.y.clamp(lo.y, hi.y);
    }
}

/// Everything the camera consumed from one frame's input, so the caller can tell
/// "the view moved" from "nothing happened" — which is what PRD §13.1's idle
/// budget is decided by.
#[derive(Debug, Default, Clone, Copy)]
pub struct CameraInput {
    /// Whether the camera moved at all this frame.
    pub moved: bool,
    /// Whether a key is still held, so the next frame must be requested.
    pub key_held: bool,
}

/// Applies pan and zoom input to a camera.
///
/// `drag` is the map area's drag delta; keyboard input is read from the context
/// only when no text field has focus.
pub fn apply_input(
    camera: &mut Camera,
    ui: &egui::Ui,
    response: &egui::Response,
    dt: f32,
) -> CameraInput {
    let mut out = CameraInput::default();

    if response.dragged() {
        let delta = response.drag_delta();
        if delta != Vec2::ZERO {
            camera.pan(delta);
            out.moved = true;
        }
    }

    let hovering = response.hovered();
    let (scroll, pointer) = ui.input(|i| (i.smooth_scroll_delta.y, i.pointer.hover_pos()));
    if hovering && scroll.abs() > 0.0 {
        let anchor = pointer.unwrap_or_else(|| camera.viewport().center());
        camera.zoom_by(ZOOM_STEP.powf(scroll / 22.0), anchor);
        out.moved = true;
    }

    // Same rule as `crate::app::read_keys`: a *text field* takes the keyboard,
    // a focused button does not.
    if ui.ctx().egui_wants_keyboard_input() {
        return out;
    }

    let mut pan = Vec2::ZERO;
    ui.input(|i| {
        if i.key_down(egui::Key::ArrowLeft) {
            pan.x += 1.0;
        }
        if i.key_down(egui::Key::ArrowRight) {
            pan.x -= 1.0;
        }
        if i.key_down(egui::Key::ArrowUp) {
            pan.y += 1.0;
        }
        if i.key_down(egui::Key::ArrowDown) {
            pan.y -= 1.0;
        }
    });
    if pan != Vec2::ZERO {
        camera.pan(pan * KEY_PAN_SPEED * dt);
        out.moved = true;
        out.key_held = true;
    }

    let centre = camera.viewport().center();
    let (zoom_in, zoom_out) = ui.input(|i| {
        (
            i.key_pressed(egui::Key::Plus) || i.key_pressed(egui::Key::Equals),
            i.key_pressed(egui::Key::Minus),
        )
    });
    if zoom_in {
        camera.zoom_by(ZOOM_STEP * ZOOM_STEP, centre);
        out.moved = true;
    }
    if zoom_out {
        camera.zoom_by(1.0 / (ZOOM_STEP * ZOOM_STEP), centre);
        out.moved = true;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn camera() -> Camera {
        Camera::fit(
            1600.0,
            20.0,
            Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 800.0)),
        )
    }

    #[test]
    fn fitting_puts_the_whole_city_inside_the_viewport() {
        let c = camera();
        let rect = c.map_rect();
        assert!(rect.width() <= 1000.5 && rect.height() <= 800.5, "{rect:?}");
        // The shorter axis is the binding one, so it fills.
        assert!((rect.height() - 800.0).abs() < 0.5, "{rect:?}");
    }

    #[test]
    fn screen_and_map_are_exact_inverses() {
        let mut c = camera();
        c.zoom_by(3.7, Pos2::new(120.0, 640.0));
        c.pan(Vec2::new(-37.0, 11.0));
        for p in [Pos2::ZERO, Pos2::new(800.0, 800.0), Pos2::new(1599.0, 3.0)] {
            let back = c.to_map(c.to_screen(p));
            assert!((back - p).length() < 1e-2, "{p:?} -> {back:?}");
        }
    }

    /// The whole point of anchored zoom: the pixel under the cursor does not
    /// move, which is what makes a map feel like a map.
    #[test]
    fn zooming_keeps_the_point_under_the_cursor_still() {
        let mut c = camera();
        let anchor = Pos2::new(311.0, 207.0);
        let before = c.to_map(anchor);
        for _ in 0..12 {
            c.zoom_by(ZOOM_STEP, anchor);
        }
        let after = c.to_map(anchor);
        assert!((after - before).length() < 0.5, "{before:?} -> {after:?}");
    }

    #[test]
    fn zoom_is_clamped_at_both_ends() {
        let mut c = camera();
        let fit = c.fit_scale();
        for _ in 0..200 {
            c.zoom_by(ZOOM_STEP, Pos2::new(500.0, 400.0));
        }
        assert!(c.scale() <= fit * MAX_ZOOM_FACTOR + 1e-3);
        for _ in 0..400 {
            c.zoom_by(1.0 / ZOOM_STEP, Pos2::new(500.0, 400.0));
        }
        assert!(c.scale() >= fit * MIN_ZOOM_FACTOR - 1e-6);
    }

    /// PRD §12's three tiers are about apparent size. Same camera, two
    /// repositories: the one whose buildings are three pixels wide gets the city
    /// representation and the one whose buildings are forty gets the detail one.
    #[test]
    fn the_tier_follows_apparent_building_size_not_the_zoom_number() {
        let viewport = Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 800.0));
        let dense = Camera::fit(1600.0, 3.0, viewport);
        let sparse = Camera::fit(1600.0, 90.0, viewport);
        // Bit-exact on purpose: both cameras were fitted to the same viewport
        // and the same map edge, so the zoom numbers are the same arithmetic.
        #[allow(clippy::float_cmp)]
        {
            assert_eq!(dense.zoom(), sparse.zoom(), "same zoom number");
        }
        assert_eq!(dense.tier(), ZoomTier::City);
        assert_eq!(sparse.tier(), ZoomTier::Building);
    }

    #[test]
    fn zooming_in_walks_up_the_tiers_and_never_back() {
        let mut c = Camera::fit(
            1600.0,
            2.0,
            Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 800.0)),
        );
        let mut seen = vec![c.tier()];
        for _ in 0..80 {
            c.zoom_by(ZOOM_STEP, Pos2::new(500.0, 400.0));
            if *seen.last().expect("seeded") != c.tier() {
                seen.push(c.tier());
            }
        }
        assert_eq!(
            seen,
            vec![ZoomTier::City, ZoomTier::District, ZoomTier::Building]
        );
    }

    /// No rotation, no tilt (PRD §12). Enforced structurally: the transform is a
    /// translation and a uniform scale, so a horizontal segment stays horizontal
    /// and aspect is preserved at every zoom and pan.
    #[test]
    fn the_projection_has_no_rotation_and_no_tilt() {
        let mut c = camera();
        c.zoom_by(5.0, Pos2::new(10.0, 10.0));
        c.pan(Vec2::new(83.0, -17.0));
        let a = c.to_screen(Pos2::new(100.0, 400.0));
        let b = c.to_screen(Pos2::new(300.0, 400.0));
        assert!(
            (a.y - b.y).abs() < 1e-3,
            "a horizontal line stayed horizontal"
        );
        let d = c.to_screen(Pos2::new(100.0, 600.0));
        assert!(
            ((b.x - a.x) / 200.0 - (d.y - a.y) / 200.0).abs() < 1e-4,
            "x and y scale identically"
        );
    }

    #[test]
    fn a_resize_does_not_teleport_the_map() {
        let mut c = camera();
        c.zoom_by(2.0, Pos2::new(500.0, 400.0));
        let centre = c.centre();
        c.set_viewport(Rect::from_min_size(Pos2::ZERO, Vec2::new(1400.0, 900.0)));
        assert!((c.centre() - centre).length() < 1e-3);
    }

    /// A bigger window shows a bigger map, not the same map with more sea
    /// around it.
    #[test]
    fn a_resize_keeps_the_zoom_relative_to_fit() {
        let mut c = camera();
        assert!((c.zoom() - 1.0).abs() < 1e-6);
        c.set_viewport(Rect::from_min_size(Pos2::ZERO, Vec2::new(2000.0, 1600.0)));
        assert!((c.zoom() - 1.0).abs() < 1e-6, "still fitted: {}", c.zoom());
        assert!(
            c.map_rect().height() > 1590.0,
            "the map grew with the window: {:?}",
            c.map_rect()
        );

        c.zoom_by(3.0, Pos2::new(1000.0, 800.0));
        let zoom = c.zoom();
        c.set_viewport(Rect::from_min_size(Pos2::ZERO, Vec2::new(900.0, 700.0)));
        assert!((c.zoom() - zoom).abs() < 1e-3, "{zoom} -> {}", c.zoom());
    }
}
