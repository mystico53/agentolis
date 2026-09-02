//! The cached base map, and the world-to-map projection every layer shares
//! (PRD §13).
//!
//! > **Base map cached to a texture.** The city changes on the order of seconds;
//! > agents move continuously. Redraw the base only on layout change; composite
//! > the agent and attention layers per frame.
//!
//! `polis-render`'s CPU rasteriser already draws the whole city plan and already
//! enforces PRD §10.3's contrast budget on the pixels it produces, so the base
//! texture is exactly [`polis_render::plan::render_base_map`]'s output uploaded
//! once. Nothing in this module draws a city; it projects one, caches one, and
//! answers "which building is under the cursor".
//!
//! # Why the projection is reconstructed rather than borrowed
//!
//! `render_base_map` fits the city to its canvas with a private `world_bounds`
//! and a [`polis_render::plan::View`]. The overlay has to agree with that fit to
//! the pixel or the vector layers peel away from the picture. `View::fit` is
//! public and its inputs are the block ring vertices, which are public too, so
//! the fit is reproduced here from the same inputs rather than by guessing at
//! the result — and `the_projection_agrees_with_the_rasteriser` asserts the two
//! land on the same pixels.
//!
//! The rasteriser supersamples and then box-filters down. `View::fit` is linear
//! in width, height and margin, so fitting at the *output* size is exactly the
//! downsampled fit; that identity is asserted too, because it is the one step
//! where a factor of two would go unnoticed until the overlay was subtly wrong.

// Projection and hit-testing are numeric geometry: every cast here lands in a
// pixel index or a screen coordinate that is clamped on purpose, and `a`, `b`,
// `p` are the names the geometry itself uses.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::many_single_char_names,
    clippy::similar_names
)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use eframe::egui::{self, Pos2, Rect, Vec2};
use polis_events::LogicalPath;
use polis_layout::city::City;
use polis_layout::{CityLayout, Point, Polygon};
use polis_render::plan::{self, View};

/// The base map's edge length in texture pixels.
///
/// The same 1 600 `polis snapshot` defaults to, so the window and the PNG are
/// the same picture. It is the ground the vector layers are drawn over rather
/// than the source of detail — from [`polis_render::camera::ZoomTier::District`]
/// up, buildings and roads are redrawn as crisp vectors on top, which is what
/// makes zooming in sharpen the map instead of magnifying its pixels.
pub const BASE_MAP_PIXELS: usize = 1600;

/// Supersampling factor for the base map. Two is the useful setting.
pub const BASE_MAP_SUPERSAMPLE: usize = 2;

/// The margin `polis_render::plan` fits the city inside, as a fraction of the
/// canvas edge. Reproduced here; asserted against the rasteriser below.
const FIT_MARGIN: f64 = 0.022;

/// A polygon already projected into base-map pixels, with its bounding box.
#[derive(Debug, Clone)]
pub struct MapShape {
    /// The ring, in base-map pixels.
    pub ring: Vec<Pos2>,
    /// Its bounding box, for culling and for a cheap hit-test reject.
    pub bounds: Rect,
    /// Its centroid, which is where a label or a mark is anchored.
    pub centre: Pos2,
}

impl MapShape {
    /// Whether a base-map point is inside the ring.
    pub fn contains(&self, p: Pos2) -> bool {
        if !self.bounds.contains(p) {
            return false;
        }
        // Even-odd crossing count. The rings come from `polis-layout`, which
        // guarantees them simple.
        let mut inside = false;
        let n = self.ring.len();
        for i in 0..n {
            let a = self.ring[i];
            let b = self.ring[(i + 1) % n];
            if (a.y > p.y) != (b.y > p.y) {
                let t = (p.y - a.y) / (b.y - a.y);
                if p.x < a.x + t * (b.x - a.x) {
                    inside = !inside;
                }
            }
        }
        inside
    }

    /// The larger of the bounding box's two sides.
    pub fn diameter(&self) -> f32 {
        self.bounds.width().max(self.bounds.height())
    }
}

/// Every static shape of the city, projected once into base-map pixels.
///
/// Rebuilt only when the layout changes, which PRD §13 says is on the order of
/// seconds. Per frame the vector layers are a translate-and-scale of these.
#[derive(Debug)]
pub struct Geometry {
    /// Building footprints, in the layout's own path order.
    pub buildings: BTreeMap<LogicalPath, MapShape>,
    /// District boundaries.
    pub districts: BTreeMap<LogicalPath, MapShape>,
    /// Industrial masses (PRD §8) — drawn as one shape, never as buildings.
    pub industrial: Vec<(LogicalPath, MapShape)>,
    /// Road segments, as base-map pixel pairs with their class.
    pub roads: Vec<(Pos2, Pos2, polis_layout::RoadClass)>,
    /// Cross-district import streets (PRD §9), as polylines.
    pub streets: Vec<(Vec<Pos2>, u32)>,
    /// Monuments, strongest first, with the point to label (PRD §8).
    pub monuments: Vec<(LogicalPath, Pos2, u32)>,
    /// A typical building's diameter in base-map pixels: the median, which is
    /// what [`crate::camera::Camera::tier`] is keyed on.
    pub median_building_px: f32,
}

impl Geometry {
    /// Projects a whole city.
    pub fn build(city: &City, view: &View) -> Self {
        let project = |poly: &Polygon| -> MapShape {
            let ring: Vec<Pos2> = poly
                .vertices
                .iter()
                .map(|v| project_point(view, *v))
                .collect();
            shape_of(ring)
        };

        let buildings: BTreeMap<LogicalPath, MapShape> = city
            .layout
            .buildings
            .iter()
            .map(|(path, b)| (path.clone(), project(&b.footprint)))
            .collect();

        let districts = city
            .layout
            .districts
            .iter()
            .map(|(path, d)| (path.clone(), project(&d.boundary)))
            .collect();

        let industrial = city
            .industrial
            .iter()
            .map(|mass| (mass.district.clone(), project(&mass.boundary)))
            .collect();

        let roads = city
            .layout
            .roads
            .segments
            .iter()
            .filter_map(|s| {
                let a = city.layout.roads.position(s.from)?;
                let b = city.layout.roads.position(s.to)?;
                Some((project_point(view, a), project_point(view, b), s.class))
            })
            .collect();

        let streets = city
            .layout
            .streets
            .iter()
            .map(|s| {
                (
                    s.polyline.iter().map(|p| project_point(view, *p)).collect(),
                    s.edge_count,
                )
            })
            .collect();

        let monuments = city
            .monuments
            .iter()
            .filter_map(|m| {
                let shape = buildings.get(&m.path)?;
                Some((m.path.clone(), shape.centre, m.rank))
            })
            .collect();

        let mut diameters: Vec<f32> = buildings.values().map(MapShape::diameter).collect();
        diameters.sort_by(f32::total_cmp);
        let median_building_px = diameters
            .get(diameters.len() / 2)
            .copied()
            .unwrap_or(8.0)
            .max(0.25);

        Self {
            buildings,
            districts,
            industrial,
            roads,
            streets,
            monuments,
            median_building_px,
        }
    }

    /// The building under a base-map point, if any.
    ///
    /// Linear with a bounding-box reject, which at 5 000 buildings is a few tens
    /// of microseconds and only runs while the pointer is over the map.
    pub fn building_at(&self, p: Pos2) -> Option<&LogicalPath> {
        self.buildings
            .iter()
            .find(|(_, shape)| shape.contains(p))
            .map(|(path, _)| path)
    }

    /// The district under a base-map point, if any. Smallest match wins, so a
    /// nested district is not swallowed by its parent.
    pub fn district_at(&self, p: Pos2) -> Option<&LogicalPath> {
        self.districts
            .iter()
            .filter(|(_, shape)| shape.contains(p))
            .min_by(|a, b| a.1.bounds.area().total_cmp(&b.1.bounds.area()))
            .map(|(path, _)| path)
    }

    /// Where to draw a mark for a logical path: its building, else its
    /// district, else the nearest ancestor district that exists.
    ///
    /// The same fallback ladder `polis_world::World::position_of` walks, in
    /// base-map pixels rather than world units — an event about a file that has
    /// no building still has somewhere honest to point.
    pub fn position_of(&self, path: &LogicalPath) -> Option<Pos2> {
        if let Some(shape) = self.buildings.get(path) {
            return Some(shape.centre);
        }
        if let Some(shape) = self.districts.get(path) {
            return Some(shape.centre);
        }
        let mut at = path.parent();
        while let Some(parent) = at {
            if let Some(shape) = self.districts.get(&parent) {
                return Some(shape.centre);
            }
            at = parent.parent();
        }
        None
    }
}

/// The base map: one texture, the projection that produced it, and the geometry
/// projected the same way.
///
/// `Debug` is written out because `egui::TextureHandle` has none, and the
/// workspace denies `missing_debug_implementations`.
pub struct BaseMap {
    /// The uploaded texture.
    texture: egui::TextureHandle,
    /// World to base-map pixels.
    view: View,
    /// Texture edge in base-map pixels.
    edge: f32,
    /// The projected city.
    pub geometry: Geometry,
    /// Which layout this was drawn from, so a republished snapshot that reuses
    /// the layout does not redraw the texture (PRD §13).
    key: usize,
    /// Whether PRD §9's street layer was baked in.
    streets: bool,
    /// How long the raster took, for the cold-start line in the status bar.
    pub render_ms: f64,
    /// The raster's own background colour.
    ///
    /// Taken from the canvas rather than restated, so the window outside the
    /// texture and the sea inside it are the same colour and the texture has no
    /// visible edge. Restating it as a constant is what put a faint grey square
    /// around the city in the first window that opened.
    background: egui::Color32,
}

impl std::fmt::Debug for BaseMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BaseMap")
            .field("edge", &self.edge)
            .field("buildings", &self.geometry.buildings.len())
            .field("streets", &self.streets)
            .field("render_ms", &self.render_ms)
            .finish_non_exhaustive()
    }
}

impl BaseMap {
    /// Rasterises the city and uploads it.
    ///
    /// The expensive call in the window's whole startup, and the reason PRD §13
    /// says to cache it: it is redone only when the layout identity changes.
    pub fn render(
        ctx: &egui::Context,
        city: &City,
        layout: &Arc<CityLayout>,
        streets: bool,
    ) -> Self {
        let started = Instant::now();
        let canvas = plan::render_base_map(city, BASE_MAP_PIXELS, BASE_MAP_SUPERSAMPLE, streets);
        let image = egui::ColorImage::from_rgb([canvas.width, canvas.height], &canvas.pixels);
        let texture = ctx.load_texture(
            "polis-base-map",
            image,
            egui::TextureOptions {
                // Linear both ways: the base map is a photograph of the city,
                // and nearest-neighbour magnification would put aliasing into
                // the one layer PRD §10.3 wants quietest.
                magnification: egui::TextureFilter::Linear,
                minification: egui::TextureFilter::Linear,
                ..Default::default()
            },
        );
        let view = fit_view(city, canvas.width);
        let geometry = Geometry::build(city, &view);
        let background =
            egui::Color32::from_rgb(canvas.pixels[0], canvas.pixels[1], canvas.pixels[2]);
        Self {
            texture,
            view,
            edge: canvas.width as f32,
            geometry,
            key: Arc::as_ptr(layout).cast::<()>() as usize,
            streets,
            render_ms: started.elapsed().as_secs_f64() * 1_000.0,
            background,
        }
    }

    /// The raster's background colour, which the window paints behind it.
    pub fn background(&self) -> egui::Color32 {
        self.background
    }

    /// Whether this texture is still the right one for a layout and a street
    /// setting.
    pub fn matches(&self, layout: &Arc<CityLayout>, streets: bool) -> bool {
        self.key == Arc::as_ptr(layout).cast::<()>() as usize && self.streets == streets
    }

    /// The texture, for the one `painter.image` call that draws it.
    pub fn texture(&self) -> &egui::TextureHandle {
        &self.texture
    }

    /// The texture's edge length in base-map pixels.
    pub fn edge(&self) -> f32 {
        self.edge
    }

    /// A world point in base-map pixels.
    pub fn to_map(&self, p: Point) -> Pos2 {
        project_point(&self.view, p)
    }

    /// Base-map pixels per world unit.
    pub fn map_px_per_world(&self) -> f32 {
        self.view.scale() as f32
    }
}

/// The fit `polis_render::plan` uses, reconstructed at the output size.
fn fit_view(city: &City, edge: usize) -> View {
    let (lo, hi) = world_bounds(city);
    View::fit(lo, hi, edge, edge, edge as f64 * FIT_MARGIN)
}

/// Mirrors `polis_render::plan`'s private `world_bounds`: the block ring
/// vertices, falling back to the layout extent when there are no blocks.
fn world_bounds(city: &City) -> ([f64; 2], [f64; 2]) {
    let mut lo = [f64::INFINITY, f64::INFINITY];
    let mut hi = [f64::NEG_INFINITY, f64::NEG_INFINITY];
    for block in &city.layout.blocks {
        for v in &block.boundary.vertices {
            lo[0] = lo[0].min(f64::from(v.x));
            lo[1] = lo[1].min(f64::from(v.y));
            hi[0] = hi[0].max(f64::from(v.x));
            hi[1] = hi[1].max(f64::from(v.y));
        }
    }
    if !lo[0].is_finite() {
        let e = f64::from(city.layout.extent).max(1.0);
        return ([-e, -e], [e, e]);
    }
    (lo, hi)
}

fn project_point(view: &View, p: Point) -> Pos2 {
    let px = view.at(p);
    Pos2::new(px[0] as f32, px[1] as f32)
}

fn shape_of(ring: Vec<Pos2>) -> MapShape {
    let mut min = Pos2::new(f32::INFINITY, f32::INFINITY);
    let mut max = Pos2::new(f32::NEG_INFINITY, f32::NEG_INFINITY);
    let mut sum = Vec2::ZERO;
    for p in &ring {
        min.x = min.x.min(p.x);
        min.y = min.y.min(p.y);
        max.x = max.x.max(p.x);
        max.y = max.y.max(p.y);
        sum += p.to_vec2();
    }
    if ring.is_empty() {
        return MapShape {
            ring,
            bounds: Rect::NOTHING,
            centre: Pos2::ZERO,
        };
    }
    MapShape {
        centre: (sum / ring.len() as f32).to_pos2(),
        bounds: Rect::from_min_max(min, max),
        ring,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polis_layout::city;

    fn city() -> City {
        let tree = polis_repo::synthetic::repository(160, 0x51);
        city::generate_with(&tree, &polis_layout::city::LayoutInputs::default())
    }

    /// The whole reason this module reproduces a private function: if the fit
    /// drifts, every vector layer peels away from the texture underneath it.
    ///
    /// Asserted by rendering the real base map and checking that the projected
    /// building centroids land on ink rather than on the sea — a fit that was
    /// off by a margin, a flip or a factor of two would put them outside the
    /// drawn city entirely.
    #[test]
    fn the_projection_agrees_with_the_rasteriser() {
        let city = city();
        let canvas = plan::render_base_map(&city, 400, 1, false);
        let view = fit_view(&city, canvas.width);
        let sea = canvas.pixels[..3].to_vec();

        let mut on_ink = 0;
        let mut total = 0;
        for building in city.layout.buildings.values() {
            let p = project_point(&view, building.footprint.centroid());
            let (x, y) = (p.x.round() as i64, p.y.round() as i64);
            assert!(
                x >= 0 && y >= 0 && (x as usize) < canvas.width && (y as usize) < canvas.height,
                "building projected outside the canvas: {p:?}"
            );
            let i = (y as usize * canvas.width + x as usize) * 3;
            total += 1;
            if canvas.pixels[i..i + 3] != sea[..] {
                on_ink += 1;
            }
        }
        assert!(total > 50, "the fixture should have buildings");
        assert!(
            on_ink * 10 >= total * 9,
            "{on_ink} of {total} building centroids landed on drawn city"
        );
    }

    /// `View::fit` is linear in width, height and margin, so fitting at the
    /// output size is exactly the supersampled fit divided by the factor. This
    /// is the step where a silent factor of two would live.
    #[test]
    fn fitting_at_the_output_size_equals_the_supersampled_fit_downsampled() {
        let city = city();
        let (lo, hi) = world_bounds(&city);
        let out = View::fit(lo, hi, 800, 800, 800.0 * FIT_MARGIN);
        let ss = View::fit(lo, hi, 3200, 3200, 3200.0 * FIT_MARGIN);
        for p in city.layout.buildings.values().take(24) {
            let c = p.footprint.centroid();
            let a = out.at(c);
            let b = ss.at(c);
            assert!(
                (a[0] - b[0] / 4.0).abs() < 1e-6 && (a[1] - b[1] / 4.0).abs() < 1e-6,
                "{a:?} vs {b:?}"
            );
        }
    }

    #[test]
    fn a_building_is_found_under_its_own_centroid() {
        let city = city();
        let view = fit_view(&city, BASE_MAP_PIXELS);
        let geometry = Geometry::build(&city, &view);
        let mut hits = 0;
        let mut tried = 0;
        for (path, shape) in geometry.buildings.iter().take(40) {
            tried += 1;
            if geometry.building_at(shape.centre) == Some(path) {
                hits += 1;
            }
        }
        assert!(hits * 4 >= tried * 3, "{hits} of {tried} centroids hit");
    }

    #[test]
    fn a_path_with_no_building_falls_back_to_an_ancestor_district() {
        let city = city();
        let view = fit_view(&city, BASE_MAP_PIXELS);
        let geometry = Geometry::build(&city, &view);
        let known = city
            .layout
            .buildings
            .keys()
            .next()
            .expect("a building")
            .clone();
        let ghost = known
            .parent()
            .expect("a parent")
            .join("no-such-file.rs")
            .expect("a valid path");
        assert!(!geometry.buildings.contains_key(&ghost));
        assert!(
            geometry.position_of(&ghost).is_some(),
            "an unmapped file still has somewhere honest to point"
        );
    }

    #[test]
    fn the_median_building_is_a_real_size() {
        let city = city();
        let view = fit_view(&city, BASE_MAP_PIXELS);
        let geometry = Geometry::build(&city, &view);
        assert!(
            geometry.median_building_px > 1.0 && geometry.median_building_px < 400.0,
            "{}",
            geometry.median_building_px
        );
    }
}
