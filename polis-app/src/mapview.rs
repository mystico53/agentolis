//! The map itself: the cached base texture, the live layers over it, and
//! picking (PRD §10.3, §12, §13).
//!
//! # Layer order is PRD §10.3's, bottom to top
//!
//! Terrain and vacant lots, then the city, then clouds — *"rendered beneath
//! district outlines and labels so the map stays readable"* — then agents, then
//! attention. Layers 1–2 arrive as one texture from `polis-render`'s rasteriser;
//! from [`ZoomTier::District`] up they are redrawn as vectors on top of it,
//! which is what makes zooming in sharpen the map rather than magnify its
//! pixels.
//!
//! # Semantic zoom is three representations, not three scales
//!
//! > **Semantic zoom**, three tiers, each a genuinely different representation
//! > rather than a scale factor. (PRD §12)
//!
//! * [`ZoomTier::City`] — districts, monuments, skyline, clouds. Buildings are
//!   sub-pixel and **not drawn individually**: the district polygons carry the
//!   shape and a per-district skyline bar carries the mass.
//! * [`ZoomTier::District`] — buildings, streets, workers, trails. Every
//!   building is a polygon of its own and the roads are drawn.
//! * [`ZoomTier::Building`] — the above plus file labels, operation glyphs at
//!   each stop on a trail, and the detail panel [`crate::ui`] opens.
//!
//! # Text is not drawn here
//!
//! Every glyph goes through [`crate::labels`] and is painted with `egui`'s own
//! text, per PRD §13. This module places anchors; the placer decides which
//! survive.

// The map is numeric geometry against a pixel grid; the `cast_*` family and
// `many_single_char_names` fire on nearly every line of it without saying
// anything, and `too_many_lines` would only push one ordered layer stack into
// fragments each called once.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::many_single_char_names,
    clippy::similar_names,
    clippy::too_many_lines
)]

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, Vec2};
use polis_events::{Glyph, LogicalPath, ThreadId, WorkerId};
use polis_world::attention::AttentionKind;
use polis_world::snapshot::WorldSnapshot;
use polis_world::{Thread, ThreadStatus};

use polis_render::{live, salience};

use crate::basemap::{BaseMap, MapShape};
use crate::camera::{Camera, ZoomTier};
use crate::clouds::Clouds;
use crate::labels::{LabelPlacer, Priority};
use crate::palette;

/// How many of a thread's most recent operations the alarm scans for failures.
///
/// The same 48 the glyph loop draws, so an alarm can never be about a failure
/// the operator cannot then find inside it — and the world caps `Thread::ops` at
/// `polis_world::OPS_CAP` anyway.
const MARKS_SCANNED: usize = 48;

/// A building touched more recently than this is under construction, and gets
/// PRD §8's scaffolding overlay: *"Temporary-looking overlay on the building.
/// Should read as impermanent."*
pub const SCAFFOLD_WINDOW: Duration = Duration::from_secs(25);

/// How long a worker mark takes to ease from one building to the next.
///
/// > **Interpolate everything.** Events arrive discretely; tween agent
/// > positions, cloud density, and building heights between updates. Cheap, and
/// > it is the entire difference between "alive" and "steppy." (PRD §13)
pub const EASE: f32 = 0.18;

/// Above this many buildings in view the vector layer is skipped and the
/// texture carries the city on its own.
///
/// A guard rather than a policy: the tier rule already means a repository whose
/// buildings are sub-pixel is drawn at [`ZoomTier::City`], so this only fires on
/// a shape of city the tier rule did not anticipate.
pub const VECTOR_BUILDING_CAP: usize = 4_000;

/// What the operator has selected, toggled and is pointing at.
///
/// Shared with [`crate::treeview`] — PRD §12's *"Shared selection and highlight
/// state"* is this one struct being read by both renderings.
#[derive(Debug, Default)]
pub struct ViewState {
    /// The selected path. Set by clicking a building or a tree row.
    pub selected: Option<LogicalPath>,
    /// The path under the pointer, in whichever view the pointer is over.
    pub hovered: Option<LogicalPath>,
    /// PRD §9's street layer. Off by default at the widest zoom.
    pub streets: bool,
    /// The thread the camera is bound to (PRD §12: cut, do not pan).
    pub follow: Option<ThreadId>,
    /// The selected thread. Set by clicking a rail row or a thread's anchor.
    ///
    /// > **Shared selection and highlight state** between the map and the tree.
    /// > (PRD §12)
    ///
    /// The rail is the third view of the same threads and behaves the same
    /// way. Separate from [`Self::follow`] because they are different verbs:
    /// following moves the *camera* and can only be on one thread; selecting
    /// says which thread the operator is reading about, and an operator
    /// following one thread while reading another is a normal thing to do.
    pub selected_thread: Option<ThreadId>,
    /// The thread under the pointer, as every view **reads** it this frame.
    ///
    /// Written by the rail when a row is hovered and by the map when a thread's
    /// own marks are, and read by both — which is what makes the highlight
    /// travel in either direction.
    hovered_thread: Option<ThreadId>,
    /// What the views have said about the pointer **during** this frame.
    ///
    /// Hover has to be double-buffered because the two views are drawn in
    /// sequence, and whichever runs second would otherwise be the only one able
    /// to answer: the rail is drawn before the map, so with a single field the
    /// map could light a row and the rail could never light a cloud. Swapping
    /// once per frame gives both directions the same one-frame latency —
    /// 16 ms, and symmetric, which is the part that matters — instead of one
    /// direction working and the other silently not.
    next_hovered_thread: Option<ThreadId>,
    /// Tween state, so agents move rather than teleport.
    pub motion: Motion,
}

impl ViewState {
    /// Publishes what the views said about the pointer last frame, and starts a
    /// fresh answer. Called once, by the caller that draws both views.
    ///
    /// A pointer that has left every view writes nothing, so the highlight
    /// clears by itself on the next frame.
    pub fn begin_frame(&mut self) {
        self.hovered_thread = self.next_hovered_thread.take();
    }

    /// A view reporting that the pointer is over this thread.
    pub fn hover_thread(&mut self, id: &ThreadId) {
        self.next_hovered_thread = Some(id.clone());
    }

    /// The thread under the pointer, as of this frame.
    #[must_use]
    pub fn hovered_thread(&self) -> Option<&ThreadId> {
        self.hovered_thread.as_ref()
    }

    /// Whether this thread is the one the operator is pointing at or has
    /// selected — the highlight the map and the rail share.
    #[must_use]
    pub fn emphasises(&self, id: &ThreadId) -> bool {
        self.hovered_thread.as_ref() == Some(id) || self.selected_thread.as_ref() == Some(id)
    }

    /// Whether the operator has **asked about** this thread, by any of the three
    /// verbs that mean it: pointing at it, selecting it, or following it.
    ///
    /// This is the gate on PRD §12's *"exact below"* — the marks that answer a
    /// question rather than set a scene. Ownership tethers are the first of
    /// them: drawn ambiently they were a line across the whole map per worker,
    /// and one session on this machine has a hundred workers.
    ///
    /// Wider than [`Self::emphasises`] by [`Self::follow`] on purpose. Emphasis
    /// is a *highlight* and follow is a *camera binding*, so they are different
    /// verbs (see the field docs) — but both are the operator saying "this one",
    /// and a thread the camera is bound to is exactly a thread being
    /// interrogated.
    #[must_use]
    pub fn interrogates(&self, id: &ThreadId) -> bool {
        self.emphasises(id) || self.follow.as_ref() == Some(id)
    }
}

/// Eased positions, so a worker glides between buildings instead of jumping
/// (PRD §13).
#[derive(Debug, Default)]
pub struct Motion {
    workers: BTreeMap<WorkerId, Pos2>,
}

impl Motion {
    /// Eases a worker's mark toward its target and reports where to draw it.
    fn ease(&mut self, worker: &WorkerId, target: Pos2) -> (Pos2, bool) {
        let current = self.workers.entry(worker.clone()).or_insert(target);
        let delta = target - *current;
        if delta.length() < 0.4 {
            *current = target;
            return (target, false);
        }
        *current += delta * EASE;
        (*current, true)
    }

    /// Drops workers that are no longer in the world, so the map does not
    /// remember a thread that ended.
    fn retain(&mut self, live: &dyn Fn(&WorkerId) -> bool) {
        self.workers.retain(|id, _| live(id));
    }
}

/// What one frame of the map did, for the caller's repaint decision and for the
/// status bar.
///
/// `Default` is written out because [`ZoomTier`] has none and should not: which
/// representation to draw is a decision the camera makes, so there is no such
/// thing as a default one. A frame that never reached the map reports
/// [`ZoomTier::City`], which is what an unopened map is showing.
#[derive(Debug, Clone)]
pub struct MapFrame {
    /// The building under the pointer, if any.
    pub hovered: Option<LogicalPath>,
    /// The building clicked this frame.
    pub clicked: Option<LogicalPath>,
    /// Whether the camera moved.
    pub camera_moved: bool,
    /// Whether an animation is still in flight, so the next frame must be
    /// requested. This is the whole of PRD §13.1's idle budget: no animation,
    /// no repaint.
    pub animating: bool,
    /// Whether a key is being held.
    pub key_held: bool,
    /// Labels drawn.
    pub labels_placed: usize,
    /// Labels decluttered away — surfaced so "decluttered" and "not drawn"
    /// never look the same.
    pub labels_dropped: usize,
    /// Which representation was drawn.
    pub tier: ZoomTier,
    /// Buildings drawn as vectors this frame.
    pub buildings_drawn: usize,
    /// How long the whole map layer took.
    pub draw_ms: f64,
}

impl Default for MapFrame {
    fn default() -> Self {
        Self {
            hovered: None,
            clicked: None,
            camera_moved: false,
            animating: false,
            key_held: false,
            labels_placed: 0,
            labels_dropped: 0,
            tier: ZoomTier::City,
            buildings_drawn: 0,
            draw_ms: 0.0,
        }
    }
}

/// Draws the map and handles its input.
///
/// The argument list is long because a frame of the map genuinely depends on
/// nine things and bundling them into a context struct would only move the list
/// somewhere the caller cannot see it.
#[allow(clippy::too_many_arguments)]
pub fn draw(
    ui: &mut egui::Ui,
    rect: Rect,
    base: &BaseMap,
    camera: &mut Camera,
    clouds: &mut Clouds,
    snapshot: &WorldSnapshot,
    state: &mut ViewState,
    cloud_cap: usize,
    dt: f32,
) -> MapFrame {
    let started = Instant::now();
    let mut out = MapFrame::default();

    camera.set_viewport(rect);
    let response = ui.interact(rect, ui.id().with("polis-map"), Sense::click_and_drag());
    let input = crate::camera::apply_input(camera, ui, &response, dt);
    out.camera_moved = input.moved;
    out.key_held = input.key_held;

    let tier = camera.tier();
    out.tier = tier;
    let painter = ui.painter_at(rect);
    let vis = camera.visible_map_rect();
    let scale = camera.scale();

    // --- 0/1/2. Terrain and city, as the cached texture (PRD §13) -----------
    painter.rect_filled(rect, 0.0, base.background());
    // The texture fades back as the vector layers take over, so the crisp
    // geometry dominates instead of fighting a magnified raster.
    let tint = match tier {
        ZoomTier::City => Color32::WHITE,
        ZoomTier::District => Color32::from_gray(210),
        ZoomTier::Building => Color32::from_gray(150),
    };
    painter.image(
        base.texture().id(),
        camera.map_rect(),
        Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
        tint,
    );

    // --- 2. The vector city, from District tier up --------------------------
    if tier != ZoomTier::City {
        out.buildings_drawn = draw_vector_city(&painter, base, camera, snapshot, state, vis, tier);
    }

    // --- 3. Clouds — beneath district outlines and labels (PRD §10.3) -------
    if let Some((texture, cloud_rect)) = clouds.update(ui.ctx(), base, snapshot, cloud_cap, dt) {
        let screen = Rect::from_min_max(
            camera.to_screen(cloud_rect.min),
            camera.to_screen(cloud_rect.max),
        );
        if screen.intersects(rect) {
            painter.image(
                texture.id(),
                screen,
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                Color32::WHITE,
            );
        }
    }

    // --- 2b. The wayfinding skeleton, which survives every declutter --------
    draw_districts(&painter, base, camera, snapshot, vis, tier);

    // --- 4. Agents: trails, tethers, workers, operation glyphs --------------
    // The pointer is read before the layer rather than after it, because the
    // agent layer is where a thread is picked: PRD §12's shared highlight has to
    // be settled on the same frame the rail is drawn, not one frame behind it.
    let pointer = response.hover_pos();
    let mut animating = draw_agents(&painter, base, camera, snapshot, state, tier, pointer);
    // PRD §13 tweens the cloud density, so the window owes itself a frame while
    // a territory is still arriving, drifting or dissipating.
    animating |= clouds.animating();

    // --- 4b. The alarm: failure given area (PRD §11.4) ----------------------
    animating |= draw_alarms(&painter, base, camera, snapshot, rect);

    // --- 5. Attention: the top of the contrast range ------------------------
    animating |= draw_attention(&painter, base, camera, snapshot);

    // --- Selection and hover, which are the operator's own attention --------
    if let Some(p) = pointer {
        let map = camera.to_map(p);
        out.hovered = base.geometry.building_at(map).cloned();
    }
    if std::env::var_os("POLIS_DEBUG_PICK").is_some() {
        eprintln!(
            "pick: rect={rect:?} hovered_rect={} ctx_pointer={:?} pointer={:?} map={:?} hit={:?}",
            response.hovered(),
            ui.ctx().pointer_hover_pos(),
            pointer,
            pointer.map(|p| camera.to_map(p)),
            out.hovered.as_ref().map(polis_events::LogicalPath::as_str),
        );
    }
    for (path, ink, width) in [
        (state.selected.as_ref(), palette::selection(), 2.4),
        (out.hovered.as_ref(), palette::hover(), 1.6),
    ] {
        if let Some(path) = path {
            if let Some(shape) = base.geometry.buildings.get(path) {
                let ring = project_ring(shape, camera);
                painter.add(egui::Shape::closed_line(
                    ring,
                    Stroke::new(width, ink.alpha(0.95)),
                ));
            } else if let Some(shape) = base.geometry.districts.get(path) {
                painter.add(egui::Shape::closed_line(
                    project_ring(shape, camera),
                    Stroke::new(width, ink.alpha(0.7)),
                ));
            }
        }
    }

    if response.clicked() {
        out.clicked.clone_from(&out.hovered);
    }

    // --- Text, last, through the declutterer (PRD §13, §8) ------------------
    let mut placer = LabelPlacer::new(rect);
    draw_labels(
        &painter,
        &mut placer,
        base,
        camera,
        snapshot,
        state,
        tier,
        scale,
    );
    out.labels_placed = placer.placed();
    out.labels_dropped = placer.dropped();

    state
        .motion
        .retain(&|id| snapshot.threads.iter().any(|t| t.worker(id).is_some()));

    out.animating = animating;
    out.draw_ms = started.elapsed().as_secs_f64() * 1_000.0;
    out
}

/// Layer 2 as vectors: roads, industrial masses, building footprints.
fn draw_vector_city(
    painter: &egui::Painter,
    base: &BaseMap,
    camera: &Camera,
    snapshot: &WorldSnapshot,
    state: &ViewState,
    vis: Rect,
    tier: ZoomTier,
) -> usize {
    let scale = camera.scale();

    if state.streets {
        for (polyline, edges) in &base.geometry.streets {
            let points: Vec<Pos2> = polyline.iter().map(|p| camera.to_screen(*p)).collect();
            if points.len() < 2 {
                continue;
            }
            let width = (1.0 + (*edges as f32).sqrt() * 0.6) * scale.min(2.0);
            painter.add(egui::Shape::line(
                points,
                Stroke::new(width, palette::street().alpha(0.85)),
            ));
        }
    }

    for (a, b, class) in &base.geometry.roads {
        if !vis.intersects(Rect::from_two_pos(*a, *b)) {
            continue;
        }
        let width = road_width(*class) * scale;
        painter.line_segment(
            [camera.to_screen(*a), camera.to_screen(*b)],
            Stroke::new(width.clamp(0.6, 14.0), palette::road().color()),
        );
    }

    // PRD §8: an industrial zone is "rendered as a single mass, not individual
    // buildings", and making it dull is the feature.
    for (_, shape) in &base.geometry.industrial {
        if !vis.intersects(shape.bounds) {
            continue;
        }
        painter.add(egui::Shape::convex_polygon(
            project_ring(shape, camera),
            palette::industrial().alpha(0.9),
            Stroke::new(1.0, palette::roof_edge().color()),
        ));
    }

    let visible: Vec<(&LogicalPath, &MapShape)> = base
        .geometry
        .buildings
        .iter()
        .filter(|(_, shape)| vis.intersects(shape.bounds))
        .collect();
    if visible.len() > VECTOR_BUILDING_CAP {
        return 0;
    }

    let now = snapshot.at;
    for (path, shape) in &visible {
        let ring = project_ring(shape, camera);
        painter.add(egui::Shape::convex_polygon(
            ring.clone(),
            palette::roof().color(),
            Stroke::new(
                if tier == ZoomTier::Building { 1.2 } else { 0.7 },
                palette::roof_edge().color(),
            ),
        ));

        let Some(file) = snapshot.file(path) else {
            continue;
        };
        // A building somebody has touched gets an agent-band outline whose
        // opacity is its recency: the map says who has been where without
        // needing a legend.
        let age = file
            .last_touched
            .map_or(polis_world::TRAIL_TTL, |t| now.saturating_duration_since(t));
        let fade = 1.0 - (age.as_secs_f32() / polis_world::TRAIL_TTL.as_secs_f32()).clamp(0.0, 1.0);
        if fade > 0.02 {
            painter.add(egui::Shape::closed_line(
                ring.clone(),
                Stroke::new(1.6, palette::trail().aged(fade)),
            ));
        }
        // PRD §8's scaffolding: temporary-looking, and it reads as impermanent
        // because it is a broken outline standing off the roof rather than part
        // of it.
        if age < SCAFFOLD_WINDOW && file.writes > 0 {
            scaffold(painter, &ring, palette::scaffold());
        }
    }
    visible.len()
}

/// PRD §8's wayfinding skeleton: district boundaries and, at
/// [`ZoomTier::City`], a per-district skyline bar standing in for the buildings
/// that are too small to draw.
fn draw_districts(
    painter: &egui::Painter,
    base: &BaseMap,
    camera: &Camera,
    snapshot: &WorldSnapshot,
    vis: Rect,
    tier: ZoomTier,
) {
    for (path, shape) in &base.geometry.districts {
        if !vis.intersects(shape.bounds) {
            continue;
        }
        let ring = project_ring(shape, camera);
        // At City tier the buildings are sub-pixel and are not drawn at all, so
        // the district *is* the unit: it gets a filled silhouette in its own
        // hue and a skyline bar for the live mass inside it. That is a different
        // representation, not the same one at a smaller scale.
        if tier == ZoomTier::City {
            if let Some(rgb) = base.geometry.district_colours.get(path) {
                painter.add(egui::Shape::convex_polygon(
                    ring.clone(),
                    palette::Ink::base(*rgb).alpha(0.55),
                    Stroke::NONE,
                ));
            }
            skyline(painter, shape, camera, snapshot, path);
        }
        painter.add(egui::Shape::closed_line(
            ring,
            Stroke::new(
                if tier == ZoomTier::City { 1.6 } else { 1.0 },
                palette::district_edge().color(),
            ),
        ));
    }
}

/// PRD §8's skyline profile, as one bar per district.
///
/// At [`ZoomTier::City`] no building is drawn, so the mass a district is
/// carrying has to be readable some other way. The bar's height is the live diff
/// mass inside the district — PRD §7.3's *"the tallest thing on the map is the
/// biggest unreviewed pile"*, aggregated to the only unit this tier draws.
fn skyline(
    painter: &egui::Painter,
    shape: &MapShape,
    camera: &Camera,
    snapshot: &WorldSnapshot,
    district: &LogicalPath,
) {
    let mass: u32 = snapshot
        .files
        .iter()
        .filter(|(path, _)| path.starts_with(district))
        .map(|(_, file)| file.diff_lines)
        .sum();
    if mass == 0 {
        return;
    }
    let width = shape.bounds.width() * camera.scale() * 0.5;
    if width < 6.0 {
        return;
    }
    let height = (f64::from(mass).sqrt() as f32 * 2.5).min(shape.bounds.height() * camera.scale());
    let base = camera.to_screen(Pos2::new(shape.centre.x, shape.bounds.max.y));
    painter.rect_filled(
        Rect::from_min_max(
            Pos2::new(base.x - width * 0.5, base.y - height),
            Pos2::new(base.x + width * 0.5, base.y),
        ),
        0.0,
        palette::trail().alpha(0.5),
    );
}

/// Layer 4 (PRD §10.3): trails, tethers, workers and operation glyphs.
///
/// # One thread, one colour
///
/// Everything here that *is* the thread — its trail, its tethers, its workers'
/// bodies, its anchor ring — is drawn in `palette::thread_*` at that role's own
/// brightness, so the operator can match a mark on the map against the
/// rectangle in the rail without reading a single character. The operation
/// glyphs are the exception and keep PRD §10.2's outcome colour; the reason is
/// written out on `polis_render::live::Mark`.
///
/// # Picking, which is the other half of PRD §12's shared highlight
///
/// `pointer` is the map's pointer, and the nearest thread mark under it wins
/// [`ViewState::hovered_thread`] — which is what the rail reads to light the
/// matching row. Anchors and worker bodies only: a trail is a long thin thing
/// that crosses half the city, and picking one would mean the pointer selected
/// a different thread every few pixels of travel.
fn draw_agents(
    painter: &egui::Painter,
    base: &BaseMap,
    camera: &Camera,
    snapshot: &WorldSnapshot,
    state: &mut ViewState,
    tier: ZoomTier,
    pointer: Option<Pos2>,
) -> bool {
    let mut animating = false;
    let now = snapshot.at;
    let ttl = polis_world::TRAIL_TTL.as_secs_f32();
    // Nearest pickable mark to the pointer, and whose it is.
    let mut pick: Option<(f32, ThreadId)> = None;
    let mut consider = |at: Pos2, radius: f32, id: &ThreadId| {
        if let Some(p) = pointer {
            let d = (at - p).length();
            if d <= radius && pick.as_ref().is_none_or(|(best, _)| d < *best) {
                pick = Some((d, id.clone()));
            }
        }
    };

    for thread in &snapshot.threads {
        // Assigned once by the world (PRD §11.4); never re-derived here, so
        // the map and a recorded frame read one answer.
        let tint = thread.tint;
        // Emphasis is **weight**, never a different colour: a highlighted
        // thread has to stay the colour the rail says it is, or the one thing
        // the swatch promises stops being true at exactly the moment the
        // operator is checking it (PRD §11.4).
        let lift = if state.emphasises(&thread.id) {
            1.0
        } else {
            0.0
        };

        // --- Trails: history without a timeline scrubber (PRD §12) ----------
        //
        // Drawn only for the thread the operator is asking about, for the same
        // reason as the tether below it. TRAIL_CAP is 192 points held for
        // TRAIL_TTL = 15 minutes, so five live threads put up to 960 persisting
        // segments across the city at once — measured on the operator's own map,
        // and their verdict was "still lines everywhere!!". §12 wants a trail to
        // show backtracking and thrashing; a permanent cobweb over every
        // district shows neither, and buries the buildings it is drawn on.
        //
        // The ambient answer to "whose work is this" is the thread's colour,
        // which is on every building it has touched. The trail is the exact
        // answer to "where has this one been", and §12 asks for exactly that
        // split: fuzzy above, exact below.
        let mut points: Vec<(Pos2, f32)> = if state.interrogates(&thread.id) {
            Vec::with_capacity(thread.trail.len())
        } else {
            Vec::new()
        };
        for (path, at) in thread
            .trail
            .iter()
            .filter(|_| state.interrogates(&thread.id))
        {
            let Some(map) = base.geometry.position_of(path) else {
                continue;
            };
            let age = now.saturating_duration_since(*at).as_secs_f32();
            let fade = 1.0 - (age / ttl).clamp(0.0, 1.0);
            points.push((camera.to_screen(map), fade));
        }
        for pair in points.windows(2) {
            let (a, fa) = pair[0];
            let (b, fb) = pair[1];
            let fade = f32::midpoint(fa, fb);
            if fade <= 0.02 {
                continue;
            }
            painter.line_segment(
                [a, b],
                Stroke::new(1.4 + lift, palette::thread_trail(tint).aged(fade)),
            );
        }

        // --- Operation glyphs: shape says what, colour says how it went -----
        if tier != ZoomTier::City {
            let radius = if tier == ZoomTier::Building { 6.0 } else { 3.6 };
            for op in thread.ops.iter().rev().take(48) {
                let Some(path) = &op.path else { continue };
                let Some(map) = base.geometry.position_of(path) else {
                    continue;
                };
                let age = now.saturating_duration_since(op.at).as_secs_f32();
                let fade = 1.0 - (age / ttl).clamp(0.0, 1.0);
                if fade <= 0.05 {
                    continue;
                }
                draw_glyph(
                    painter,
                    camera.to_screen(map),
                    radius,
                    op.glyph,
                    palette::outcome(op.outcome).aged(fade),
                );
            }
        }

        // --- Workers, and the tethers only the asked-about thread gets ------
        //
        // Ownership is carried ambiently by `tint`: the worker's body, the
        // thread's anchor ring, its trail, its cloud and the rail's swatch are
        // one hue. The *line* back to the anchor is the exact answer to a
        // question about one thread, so it is drawn only for the thread the
        // operator is pointing at, has selected or is following.
        // `polis_render::live::draw_tether` carries the measurement.
        // One ladder, walked from one place. `polis_render::frame::thread_anchor`
        // has always gone through `place::thread_position`; the window read
        // `centre_of_mass` and stopped, so a thread with a trail and no kernels
        // got a ring in a recorded GIF and none in the window — the same frame,
        // two answers. `WorldSnapshot::layout` is the `CityLayout` that makes
        // the shared ladder callable here, and `base.to_map` is the only thing
        // this side adds.
        let anchor = polis_world::place::thread_position(thread, &snapshot.layout)
            .map(|p| camera.to_screen(base.to_map(p)));
        let tethered = state.interrogates(&thread.id);
        // The renderer's rank, so the window and a recorded frame stop at the
        // same sixteen hands: running first, then the most recently active.
        let mut ranked: Vec<&polis_world::Worker> = thread.workers.iter().collect();
        ranked.sort_by(|a, b| {
            b.running
                .cmp(&a.running)
                .then(b.last_activity.cmp(&a.last_activity))
                .then(a.id.cmp(&b.id))
        });
        let fan = live::TETHERS_PER_THREAD.min(ranked.len()).max(2) - 1;
        for (rank, worker) in ranked.iter().enumerate() {
            let Some(focus) = &worker.focus else { continue };
            let Some(map) = base.geometry.position_of(focus) else {
                continue;
            };
            let (eased, moving) = state.motion.ease(&worker.id, map);
            animating |= moving;
            let at = camera.to_screen(eased);
            if let Some(anchor) = anchor.filter(|_| tethered && rank < live::TETHERS_PER_THREAD) {
                draw_tether(
                    painter,
                    anchor,
                    at,
                    (rank as f32 / fan as f32).mul_add(2.0, -1.0),
                    worker.running,
                    tint,
                );
            }
            let r = if worker.running { 5.0 } else { 3.5 };
            painter.add(egui::Shape::convex_polygon(
                vec![
                    at + Vec2::new(0.0, -r),
                    at + Vec2::new(r * 0.8, r * 0.7),
                    at + Vec2::new(-r * 0.8, r * 0.7),
                ],
                palette::thread(tint).alpha(if worker.running { 0.95 } else { 0.45 }),
                Stroke::NONE,
            ));
            consider(at, r + 4.0, &thread.id);
        }

        // The thread's own mark, at its territory's anchor — the kernel centre
        // its density field is highest at, never the mean of two lobes.
        if let Some(anchor) = anchor {
            // Two rings, two channels. The outer one is the thread's identity
            // and never changes; the inner dot is `status`, which does. Before
            // this the anchor was drawn in the status colour alone, so four
            // threads in the same state were four identical rings.
            let ink = palette::status(thread.status);
            painter.circle_stroke(
                anchor,
                7.0,
                Stroke::new(1.8 + lift * 1.2, palette::thread_anchor(tint).alpha(0.95)),
            );
            if thread.status == ThreadStatus::Working {
                painter.circle_filled(anchor, 2.4, ink.alpha(0.9));
            }
            if lift > 0.0 {
                // The highlight itself is a *shape* — a second ring standing off
                // the first — so it survives greyscale and the periphery, where
                // PRD §11.4 says colour does not.
                painter.circle_stroke(anchor, 11.5, Stroke::new(1.2, palette::hover().alpha(0.8)));
            }
            // PRD §10.4's drift: the redirect signal, drawn as a leading edge
            // only while the centre of mass is actually migrating.
            if let Some(mark) = thread.territory.drift_mark() {
                draw_drift(painter, base, camera, &mark, palette::thread_anchor(tint));
            }
            consider(anchor, 12.0, &thread.id);
        }
    }
    if let Some((_, id)) = pick {
        state.hover_thread(&id);
    }
    animating
}

/// How far a tether bows off the straight line, at most, in screen pixels.
///
/// `polis_render::live::draw_tether` states it as five glyph radii; the window's
/// worker mark is five pixels, so this is the same number in the units the
/// overlay works in. It is a cap on `len * 0.075`, which is what does the work
/// on the short tethers — a fan of workers inside one district would otherwise
/// bow by a few pixels and stay a ribbon.
const TETHER_BOW: f32 = 26.0;

/// One tether, in the window, drawn exactly as `polis_render::live::draw_tether`
/// draws it in a recorded frame.
///
/// > A visual language with two definitions has none.
///
/// So the geometry is that function's: a quadratic bow whose sign and size come
/// from `spread`, eight segments, solid while the worker is running and dashed
/// once it has finished — a **shape** difference, because PRD §11.4 says
/// peripheral vision cannot do the three-grey-levels one. The ink is that
/// module's constant faded to that module's tones (0.70 running, 0.34
/// finished), in the thread's own hue.
///
/// Who gets one is decided by the caller: [`ViewState::interrogates`], and the
/// reasoning is on `polis_render::live::draw_tether`.
fn draw_tether(
    painter: &egui::Painter,
    anchor: Pos2,
    worker: Pos2,
    spread: f32,
    running: bool,
    tint: u8,
) {
    let (width, tone) = if running { (1.6, 0.70) } else { (1.2, 0.34) };
    let ink = palette::thread_tether(tint).aged(tone);
    let d = worker - anchor;
    let len = d.length().max(1e-3);
    let bow = (len * 0.075).min(TETHER_BOW) * spread.clamp(-1.0, 1.0);
    let mid = anchor + d * 0.5;
    let ctrl = mid + Vec2::new(-d.y / len, d.x / len) * bow;
    let steps = 8;
    let pts: Vec<Pos2> = (0..=steps)
        .map(|i| {
            let t = i as f32 / steps as f32;
            let u = 1.0 - t;
            Pos2::new(
                (t * t).mul_add(worker.x, (u * u).mul_add(anchor.x, 2.0 * u * t * ctrl.x)),
                (t * t).mul_add(worker.y, (u * u).mul_add(anchor.y, 2.0 * u * t * ctrl.y)),
            )
        })
        .collect();
    if running {
        painter.add(egui::Shape::line(pts, Stroke::new(width, ink)));
    } else {
        // One segment on, one off: four dashes along the bow, the same period
        // the renderer uses.
        for pair in pts.as_chunks::<2>().0 {
            painter.line_segment([pair[0], pair[1]], Stroke::new(width, ink));
        }
    }
}

/// PRD §10.4's leading-edge mark — the redirect signal.
///
/// > A territory whose centre of mass is migrating out of `src/auth` toward
/// > `tests/` has a scope that is changing, and it is visible *while it is
/// > happening* — well before contention fires. […] This is the redirect signal,
/// > and it is the one thing here that no existing tool provides.
///
/// Two things about the drawing are decisions rather than taste:
///
/// * **The mark sits at the front of the field, not at the centre of mass.** The
///   centre of mass is where the thread has *been*; a mark there says "this
///   thread exists". The leading edge is where the scope is going, which is the
///   thing the operator would redirect.
/// * **Shape carries it, not colour.** PRD §11.4: *"Colour alone is never the
///   sole channel for any state."* The chevron is the channel; past the
///   threshold the mark gains weight and length rather than saturation, so a
///   drifting thread reads at a glance in the same ink as a still one.
///
/// The geometry is [`polis_world::territory::DriftMark`]'s, in city units.
/// Nothing is decided here.
fn draw_drift(
    painter: &egui::Painter,
    base: &BaseMap,
    camera: &Camera,
    mark: &polis_world::territory::DriftMark,
    ink: palette::Ink,
) {
    let tail = camera.to_screen(base.to_map(mark.tail));
    let tip = camera.to_screen(base.to_map(mark.tip));
    let along = tip - tail;
    let length = along.length();
    if length < 2.0 {
        return;
    }
    let unit = along / length;
    let normal = Vec2::new(-unit.y, unit.x);
    // At the threshold this is 1; a thread that has left is 3. Weight, not hue.
    let weight = (mark.ratio / polis_world::territory::DRIFT_THRESHOLD).clamp(1.0, 3.0);
    painter.line_segment([tail, tip], Stroke::new(1.0 * weight, ink.alpha(0.45)));
    let arm = 5.0f32.mul_add(weight, 4.0);
    for side in [1.0f32, -1.0] {
        painter.line_segment(
            [tip, tip - unit * arm + normal * (arm * 0.6 * side)],
            Stroke::new(1.4 * weight, ink.alpha(0.95)),
        );
    }
}

/// Layer 4's alarm: PRD §10.2's failure colour given the **area** PRD §11.4
/// needs (`polis_render::salience`).
///
/// # Why this is here and not only in the headless renderer
///
/// The window is the product. Until this call existed, the salience fix lived
/// entirely in `polis_render::live`, which is the path every measurement runs
/// through and *not* the path the operator looks at — so the map on the second
/// monitor still showed a failing session as two red glyph outlines while the
/// test suite reported the problem solved. The geometry is computed once, in
/// `polis_render::salience::strokes`, and drawn twice; neither rasteriser owns
/// the notation.
///
/// Drawn at **every** zoom tier, including `City`, where the operation glyphs
/// are not drawn at all. That is deliberate and it is the whole point: "which
/// building failed" is a question you walk to the screen to answer, and "that
/// district is in trouble" is one you must be able to answer from a chair on the
/// other side of the room.
fn draw_alarms(
    painter: &egui::Painter,
    base: &BaseMap,
    camera: &Camera,
    snapshot: &WorldSnapshot,
    rect: Rect,
) -> bool {
    let now = snapshot.at;
    let ttl = polis_world::TRAIL_TTL.as_secs_f32();
    let mut marks: Vec<live::Mark> = Vec::new();
    for thread in &snapshot.threads {
        for op in thread.ops.iter().rev().take(MARKS_SCANNED) {
            if op.outcome != polis_events::Outcome::Failed {
                continue;
            }
            let Some(path) = &op.path else { continue };
            let Some(map) = base.geometry.position_of(path) else {
                continue;
            };
            let age = now.saturating_duration_since(op.at).as_secs_f32() / ttl;
            if age >= 1.0 {
                continue;
            }
            let at = camera.to_screen(map);
            marks.push(live::Mark::single(
                [f64::from(at.x), f64::from(at.y)],
                op.glyph,
                op.outcome,
                f64::from(age),
                live::pulse_at(now.saturating_duration_since(op.at).as_secs_f64()),
            ));
        }
    }
    if marks.is_empty() {
        return false;
    }
    // The glyph radius the window is using, so the alarm clears the marks it is
    // about at every tier.
    let r = match camera.tier() {
        ZoomTier::Building => 6.0,
        ZoomTier::District => 3.6,
        ZoomTier::City => 3.0,
    };
    let mut animating = false;
    for alarm in salience::alarms(&marks, r, f64::from(rect.height())) {
        animating |= alarm.pulse > 0.0;
        for stroke in salience::strokes(alarm, r) {
            let ink = palette::Ink::agent(stroke.ink).alpha(1.0);
            let points: Vec<Pos2> = stroke
                .points
                .iter()
                .map(|p| Pos2::new(p[0] as f32, p[1] as f32))
                .collect();
            painter.add(egui::Shape::line(
                points,
                Stroke::new(stroke.width as f32, ink),
            ));
        }
    }
    animating
}

/// Layer 5 (PRD §11.2): the three attention states.
///
/// # The beacons are computed in `polis-render` and drawn here
///
/// `polis_render::salience::beacons` returns the region rings PRD §11.1's
/// ordering needs — one per pending decision, two per contention — in device
/// pixels, and this function paints them with `egui`. The geometry is not
/// re-derived: the window is the product and the canvas is the evidence, and a
/// notation the two rasterisers each own a copy of drifts in the direction of
/// the one nobody is measuring. That is exactly how the amber pin ended up
/// being a 20-pixel mast in the window while the headless renderer drew a plate,
/// a halo and an escalation ramp.
fn draw_attention(
    painter: &egui::Painter,
    base: &BaseMap,
    camera: &Camera,
    snapshot: &WorldSnapshot,
) -> bool {
    let mut animating = false;
    let now = snapshot.at;

    // One pass to place every mark in screen space, so the rings can be
    // clustered before anything is painted. `r` matches `draw_alarms`' own
    // glyph radius per tier, so an amber ring clears the glyphs it is about.
    let r = match camera.tier() {
        ZoomTier::Building => 6.0,
        ZoomTier::District => 3.6,
        ZoomTier::City => 3.0,
    };
    let map_height = f64::from(painter.clip_rect().height()).max(1.0);
    let mut placed: Vec<live::AttentionMark> = Vec::new();
    for mark in &snapshot.attention {
        let pulse = f64::from(mark.pulse(now));
        let weight = f64::from(mark.weight(now));
        let urgency = f64::from(mark.urgency(now));
        let (kind, at, other, severity, sited) = match &mark.kind {
            AttentionKind::NeedsDecision { thread, at, .. } => {
                let (p, sited) = decision_position(base, snapshot, thread, at.as_ref());
                let Some(p) = p else { continue };
                (live::MarkKind::NeedsDecision, p, None, None, sited)
            }
            AttentionKind::Done { thread, verified } => {
                let Some(p) = mark_position(base, snapshot, thread, None) else {
                    continue;
                };
                let kind = if *verified {
                    live::MarkKind::DoneVerified
                } else {
                    live::MarkKind::DoneUnverified
                };
                (kind, p, None, None, true)
            }
            AttentionKind::Contention(contention) => {
                let (a, b) = contention.threads();
                let (Some(pa), Some(pb)) = (
                    mark_position(base, snapshot, a, Some(contention.path())),
                    mark_position(base, snapshot, b, Some(contention.path())),
                ) else {
                    continue;
                };
                (
                    live::MarkKind::Contention,
                    pa,
                    Some(pb),
                    Some(contention.severity),
                    true,
                )
            }
        };
        let screen = camera.to_screen(at);
        placed.push(live::AttentionMark {
            kind,
            at: [f64::from(screen.x), f64::from(screen.y)],
            other: other.map(|p| {
                let s = camera.to_screen(p);
                [f64::from(s.x), f64::from(s.y)]
            }),
            severity,
            pulse,
            weight,
            urgency,
            sited,
        });
    }
    let (decisions, contended) = salience::beacons(&placed, r, map_height);
    // The primary state, region first: the ring reads from across the room and
    // the pin reads once the operator has walked over.
    animating |= paint_beacons(painter, &decisions, r);

    for mark in &snapshot.attention {
        let pulse = mark.pulse(now);
        if pulse > 0.0 {
            // Peripheral vision is poor at colour and good at motion onset
            // (PRD §11.4), so arrival is a brief pulse and the steady state is
            // shape and position.
            animating = true;
        }
        match &mark.kind {
            AttentionKind::NeedsDecision { thread, at, .. } => {
                let (Some(p), _) = decision_position(base, snapshot, thread, at.as_ref()) else {
                    continue;
                };
                pin(
                    painter,
                    camera.to_screen(p),
                    palette::needs_decision(),
                    pulse,
                );
            }
            AttentionKind::Done { thread, verified } => {
                let Some(p) = mark_position(base, snapshot, thread, None) else {
                    continue;
                };
                let ink = if *verified {
                    palette::done_verified()
                } else {
                    palette::done_unverified()
                };
                let at = camera.to_screen(p);
                let weight = mark.weight(now).clamp(0.15, 1.0);
                painter.circle_stroke(at, 9.0, Stroke::new(2.0, ink.alpha(weight)));
                if !*verified {
                    // "Done, unverified" is really "needs review", and it
                    // persists — so it gets a second ring rather than the same
                    // mark in a different colour (PRD §11.4).
                    painter.circle_stroke(at, 13.0, Stroke::new(1.2, ink.alpha(weight * 0.7)));
                }
                if pulse > 0.0 {
                    painter.circle_stroke(
                        at,
                        9.0 + pulse * 12.0,
                        Stroke::new(1.5, ink.alpha(pulse)),
                    );
                }
            }
            AttentionKind::Contention(contention) => {
                // A relation between two threads, so it is drawn as a link
                // joining them across the map, never a badge on a dot.
                let (a, b) = contention.threads();
                let (Some(pa), Some(pb)) = (
                    mark_position(base, snapshot, a, Some(contention.path())),
                    mark_position(base, snapshot, b, Some(contention.path())),
                ) else {
                    continue;
                };
                let ink = palette::contention();
                let (sa, sb) = (camera.to_screen(pa), camera.to_screen(pb));
                painter.line_segment([sa, sb], Stroke::new(2.4, ink.alpha(0.9)));
                for end in [sa, sb] {
                    painter.circle_stroke(end, 6.0, Stroke::new(2.0, ink.alpha(0.95)));
                }
                if pulse > 0.0 {
                    painter.circle_stroke(
                        (sa + sb.to_vec2()).to_vec2().to_pos2() * 0.5,
                        6.0 + pulse * 14.0,
                        Stroke::new(1.5, ink.alpha(pulse)),
                    );
                }
            }
        }
    }
    // …and contention's rings last, because PRD §11.1 puts it above the other
    // two and because a terminal hidden under a pin is a terminal the operator
    // does not walk to.
    animating |= paint_beacons(painter, &contended, r);
    animating
}

/// Paints one set of `polis_render::salience` rings. Returns whether any of
/// them is still animating an arrival.
fn paint_beacons(painter: &egui::Painter, beacons: &[salience::Alarm], r: f64) -> bool {
    let mut animating = false;
    for beacon in beacons {
        animating |= beacon.pulse > 0.0;
        for stroke in salience::strokes(*beacon, r) {
            let ink = palette::Ink::attention(stroke.ink).alpha(1.0);
            let points: Vec<Pos2> = stroke
                .points
                .iter()
                .map(|p| Pos2::new(p[0] as f32, p[1] as f32))
                .collect();
            painter.add(egui::Shape::line(
                points,
                Stroke::new(stroke.width as f32, ink),
            ));
        }
    }
    animating
}

/// Where an attention mark points: the file it names, else the thread's
/// territory, else nothing.
///
/// Public because the attention list jumps the camera to exactly where the map
/// drew the mark. Two answers to "where is this state" would be two maps.
pub fn mark_position(
    base: &BaseMap,
    snapshot: &WorldSnapshot,
    thread: &ThreadId,
    at: Option<&LogicalPath>,
) -> Option<Pos2> {
    if let Some(path) = at {
        if let Some(p) = base.geometry.position_of(path) {
            return Some(p);
        }
    }
    let thread = snapshot.thread(thread)?;
    // The anchor, with the mean behind it. Only the *first* rung changes here:
    // the ladder below is load-bearing and is not being replaced, and
    // `centre_of_mass` is kept as the immediate fallback so a territory built by
    // hand — kernels never pushed through `observe` — answers exactly as it did.
    if let Some(centre) = thread
        .territory
        .anchor()
        .or(thread.territory.centre_of_mass)
    {
        return Some(base.to_map(centre));
    }
    // PRD §6.4's lobes, and §6.2's claim. Reading the field's summary alone and
    // stopping was the window's half of the same bug the rail had: a caller
    // that tests one shape of a placed territory has quietly decided the other
    // shape is unplaced, which is how a thread with 483 calls came to have
    // nowhere on the map to put its pin.
    let scope = match thread.territory.placement() {
        polis_world::territory::Placement::Claim(path) => Some(path.clone()),
        polis_world::territory::Placement::Lobes(lobes) => lobes.first().map(|l| l.path.clone()),
        polis_world::territory::Placement::Nowhere => None,
    };
    if let Some(p) = scope.and_then(|path| base.geometry.position_of(&path)) {
        return Some(p);
    }
    thread
        .trail
        .iter()
        .rev()
        .find_map(|(path, _)| base.geometry.position_of(path))
}

/// Where a **pending decision** points, and whether that place is the real one.
///
/// The same chain as [`mark_position`], plus PRD §8's civic square as a last
/// resort — because an agent blocked on a human is the one state the map is not
/// allowed to lose, and in the operator's own screenshot all three
/// `WAITING ON YOU` rows belonged to threads with no territory at all. The
/// second element is `polis_render::live::AttentionMark::sited`: false means the
/// ring draws `salience::ALARM_UNSITED`'s accuracy circle and says *somewhere in
/// here* rather than claiming a district on no evidence.
fn decision_position(
    base: &BaseMap,
    snapshot: &WorldSnapshot,
    thread: &ThreadId,
    at: Option<&LogicalPath>,
) -> (Option<Pos2>, bool) {
    if let Some(p) = mark_position(base, snapshot, thread, at) {
        return (Some(p), true);
    }
    (base.geometry.position_of(&LogicalPath::root()), false)
}

/// The standing pin PRD §11.2 asks for: a mast and a head above the subject, so
/// the state reads by shape at any zoom and colour is never doing the work
/// alone.
fn pin(painter: &egui::Painter, at: Pos2, ink: palette::Ink, pulse: f32) {
    let head = at - Vec2::new(0.0, 20.0);
    painter.line_segment([at, head], Stroke::new(2.0, ink.alpha(0.9)));
    painter.circle_filled(head, 5.0, ink.alpha(0.95));
    if pulse > 0.0 {
        painter.circle_stroke(head, 5.0 + pulse * 14.0, Stroke::new(1.6, ink.alpha(pulse)));
    }
}

/// PRD §10.1's glyph set. Shape encodes what; the caller's colour encodes how it
/// went. They are never conflated.
fn draw_glyph(painter: &egui::Painter, at: Pos2, r: f32, glyph: Glyph, colour: Color32) {
    let stroke = Stroke::new(1.4, colour);
    match glyph {
        Glyph::HollowCircle => {
            painter.circle_stroke(at, r, stroke);
        }
        Glyph::BarredCircle => {
            painter.circle_stroke(at, r, stroke);
            painter.line_segment([at - Vec2::new(r, 0.0), at + Vec2::new(r, 0.0)], stroke);
        }
        Glyph::FilledSquare => {
            painter.rect_filled(
                Rect::from_center_size(at, Vec2::splat(r * 1.7)),
                0.0,
                colour,
            );
        }
        Glyph::FilledTriangle => {
            painter.add(egui::Shape::convex_polygon(
                vec![
                    at + Vec2::new(0.0, -r),
                    at + Vec2::new(r * 0.9, r * 0.75),
                    at + Vec2::new(-r * 0.9, r * 0.75),
                ],
                colour,
                Stroke::NONE,
            ));
        }
        Glyph::ConcentricCircles => {
            painter.circle_stroke(at, r, stroke);
            painter.circle_stroke(at, r * 0.45, stroke);
        }

        Glyph::Delegate => {
            painter.circle_filled(at, r * 0.35, colour);
            for i in 0..3 {
                let a = std::f32::consts::TAU * (i as f32 / 3.0) - std::f32::consts::FRAC_PI_2;
                painter.circle_filled(at + Vec2::angled(a) * r, r * 0.25, colour);
            }
        }
    }
}

/// A broken outline standing off the roof: PRD §8's scaffolding, which has to
/// read as impermanent.
fn scaffold(painter: &egui::Painter, ring: &[Pos2], ink: palette::Ink) {
    for (i, pair) in ring.windows(2).enumerate() {
        if i % 2 == 1 {
            continue;
        }
        painter.line_segment([pair[0], pair[1]], Stroke::new(2.0, ink.alpha(0.8)));
    }
}

/// The most monuments named at each tier, widest zoom first.
///
/// PRD §8 says monuments are *"always labelled at every zoom"* and it is right:
/// they are the orientation layer, and a map you cannot orient on is not a map.
/// It does **not** say all of them at once — `.take(24)` was reading it that
/// way, and twenty-four forced labels are a wall of type at the zoom where the
/// districts underneath them are three pixels wide. The list is already ranked
/// by inbound imports, so taking a prefix takes the best ones.
const MONUMENTS_PER_TIER: [usize; 3] = [8, 14, 18];

/// The deepest district named at each tier.
///
/// The wayfinding skeleton coarsens as you pull back: package names at city
/// zoom, one level in at district zoom, two at building zoom. Without this the
/// only zoom rule was the size gate below, which on a large repository lets
/// forty names through at once — `utils` and `lib` and `config` repeated in
/// every package, which is the definition of a name that does not locate you.
const DISTRICT_DEPTH_PER_TIER: [usize; 3] = [1, 2, 3];

/// The narrowest a building may be, on screen, and still carry its own name.
///
/// The district rule (44 px) one level down. A name wider than the thing it
/// names is not a label, it is a line of text lying across the map — and at
/// `ZoomTier::Building`'s wide end that is what several dozen of them were.
const NAMEABLE_BUILDING_PX: f32 = 30.0;

/// Every label on the map, in PRD §8's priority order.
///
/// # Order is the whole algorithm
///
/// [`LabelPlacer`] is greedy and now carries a per-frame budget, so **the order
/// these are offered in decides what the operator reads**. It is:
///
/// 1. monuments and threads waiting on a human — [`Priority::Anchor`], outside
///    the budget, because §8 makes one the orientation layer and §11.2 makes
///    the other the reason the product exists;
/// 2. districts, largest on screen first — the wayfinding skeleton, and the
///    largest are the ones that locate you;
/// 3. files, most recently touched first — *"what is being worked on"*, which
///    is the only reason a file name is on an ambient map at all.
///
/// Before this the file loop iterated `buildings` in path order and offered
/// **every file any thread had ever touched**, which on the operator's live
/// session was several hundred names competing alphabetically. What survived
/// was whatever started with `a`.
#[allow(clippy::too_many_arguments)]
fn draw_labels(
    painter: &egui::Painter,
    placer: &mut LabelPlacer,
    base: &BaseMap,
    camera: &Camera,
    snapshot: &WorldSnapshot,
    state: &ViewState,
    tier: ZoomTier,
    scale: f32,
) {
    let vis = camera.visible_map_rect();
    let rank = match tier {
        ZoomTier::City => 0,
        ZoomTier::District => 1,
        ZoomTier::Building => 2,
    };

    // 1. Monuments. PRD §8: "always labelled at every zoom". Priority::Anchor,
    //    so they are placed before anything else and never dropped. Filtered to
    //    the viewport *before* the count is taken — taking first meant a camera
    //    that had paned away from the top-ranked monuments got no anchors at
    //    all, which is the opposite of what the rule is for.
    for (path, at, _) in base
        .geometry
        .monuments
        .iter()
        .filter(|m| vis.contains(m.1))
        .take(MONUMENTS_PER_TIER[rank])
    {
        let name = path.file_name().unwrap_or_else(|| path.as_str());
        label(
            painter,
            placer,
            camera.to_screen(*at),
            name,
            FontId::proportional(if tier == ZoomTier::City { 10.0 } else { 12.0 }),
            palette::monument_label(),
            Priority::Anchor,
        );
    }

    // 2. Threads waiting on a human get a name on the map, because that is what
    //    the product is for (PRD §1, §11.2). Before the districts, not after:
    //    they are anchors, and an anchor that arrives late has to shove an
    //    already-placed label aside to take its place.
    for thread in snapshot.waiting() {
        // Substituted, not extended: `anchor()` is `Some` exactly when there are
        // kernels and `centre_of_mass` is `Some` exactly when they carry weight,
        // so the same set of waiting threads gets a name as before. What changes
        // is where the name goes — over the work rather than over the gap
        // between two lobes, which is where a name is least readable.
        let Some(centre) = thread
            .territory
            .anchor()
            .or(thread.territory.centre_of_mass)
        else {
            continue;
        };
        label(
            painter,
            placer,
            camera.to_screen(base.to_map(centre)) - Vec2::new(0.0, 26.0),
            &shorten(&thread_label(thread), MAP_TITLE_CHARS),
            FontId::proportional(12.0),
            palette::monument_label(),
            Priority::Anchor,
        );
    }

    // 3. Districts — the rest of the wayfinding skeleton, largest first.
    let mut districts: Vec<(f32, &LogicalPath, &MapShape)> = base
        .geometry
        .districts
        .iter()
        .filter(|(path, shape)| {
            vis.intersects(shape.bounds)
                && !path.is_root()
                && path.depth() <= DISTRICT_DEPTH_PER_TIER[rank]
                // A district smaller than its own label is noise at this zoom.
                && shape.diameter() * scale >= 44.0
        })
        .map(|(path, shape)| (shape.diameter(), path, shape))
        .collect();
    districts.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    for (_, path, shape) in districts {
        let name = path.file_name().unwrap_or_else(|| path.as_str());
        label(
            painter,
            placer,
            camera.to_screen(shape.centre),
            name,
            FontId::proportional(if tier == ZoomTier::City { 13.0 } else { 11.0 }),
            palette::district_label(),
            Priority::Structural,
        );
    }

    // 4. Files. Only at the tier where a building can carry a name, only where
    //    the building is wide enough to carry it, and **most recently touched
    //    first** — the map names what is happening now, and the budget stops it
    //    before it names the whole session's history.
    if tier != ZoomTier::Building {
        return;
    }
    let mut files: Vec<(Option<Instant>, &LogicalPath, &MapShape)> = base
        .geometry
        .buildings
        .iter()
        .filter(|(path, shape)| {
            vis.intersects(shape.bounds)
                && (shape.diameter() * scale >= NAMEABLE_BUILDING_PX
                    // The one the operator is pointing at or has selected is an
                    // answer to a question and is never too small to name.
                    || state.selected.as_ref() == Some(*path)
                    || state.hovered.as_ref() == Some(*path))
        })
        .filter_map(|(path, shape)| {
            let picked =
                state.selected.as_ref() == Some(path) || state.hovered.as_ref() == Some(path);
            let touched = snapshot.file(path).and_then(|f| f.last_touched);
            (picked || touched.is_some()).then(|| {
                // A picked building sorts above every touched one, whenever it
                // was last written to.
                (picked.then_some(snapshot.at).or(touched), path, shape)
            })
        })
        .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    for (_, path, shape) in files {
        if placer.remaining() == 0 {
            break;
        }
        let name = path.file_name().unwrap_or_else(|| path.as_str());
        label(
            painter,
            placer,
            camera.to_screen(shape.centre),
            name,
            FontId::monospace(11.0),
            palette::file_label(),
            Priority::Detail,
        );
    }
}

/// The most characters of a thread's title the **map** draws.
///
/// The rail has a column and can wrap; the map has a building. A generated
/// title runs to whatever the session was about — *"Ultracode CSS tokens and
/// manifest bug"* — and at 12 pt that is a 260-pixel bar of type lying across
/// four districts, forced into place because a waiting thread is
/// [`Priority::Anchor`]. Twenty-two characters is enough to tell four threads
/// apart, which is all the map is being asked for; the rail has the rest.
const MAP_TITLE_CHARS: usize = 22;

/// Truncates on a character boundary, with an ellipsis when it cut.
fn shorten(text: &str, chars: usize) -> String {
    if text.chars().count() <= chars {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// A short, stable name for a thread.
pub fn thread_label(thread: &Thread) -> String {
    thread.title.clone().unwrap_or_else(|| {
        let id = thread.id.as_str();
        format!("thread {}", &id[..id.len().min(8)])
    })
}

/// Lays out one label, asks the placer for room, and draws it if it got any.
#[allow(clippy::too_many_arguments)]
fn label(
    painter: &egui::Painter,
    placer: &mut LabelPlacer,
    anchor: Pos2,
    text: &str,
    font: FontId,
    ink: palette::Ink,
    priority: Priority,
) -> bool {
    let galley = painter.layout_no_wrap(text.to_owned(), font, ink.color());
    let Some(rect) = placer.place(anchor, galley.size(), priority) else {
        return false;
    };
    // A quiet plate under the type, so a label over a bright cloud stays
    // readable without the type itself leaving its band.
    painter.rect_filled(rect.expand(2.0), 2.0, palette::sea().alpha(0.72));
    painter.galley(rect.min, galley, ink.color());
    true
}

fn project_ring(shape: &MapShape, camera: &Camera) -> Vec<Pos2> {
    shape.ring.iter().map(|p| camera.to_screen(*p)).collect()
}

/// PRD §7.2's road hierarchy, as stroke widths.
///
/// An arterial survives decluttering at every zoom and an alley is the first
/// thing dropped, so the widths are ordered the way `RoadClass` documents them.
fn road_width(class: polis_layout::RoadClass) -> f32 {
    use polis_layout::RoadClass as R;
    match class {
        R::Arterial => 3.4,
        R::Street => 2.0,
        R::Alley => 1.1,
    }
}

/// Draws a legend for PRD §10.1's glyphs and §10.2's outcomes, so the notation
/// is learnable from the window rather than from the PRD.
pub fn legend(ui: &mut egui::Ui) {
    let font = FontId::proportional(11.0);
    for (glyph, name) in [
        (Glyph::HollowCircle, "read"),
        (Glyph::BarredCircle, "edit"),
        (Glyph::FilledSquare, "write"),
        (Glyph::FilledTriangle, "run"),
        (Glyph::ConcentricCircles, "verify"),
        (Glyph::Delegate, "delegate"),
    ] {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(96.0, 16.0), Sense::hover());
        let centre = Pos2::new(rect.min.x + 8.0, rect.center().y);
        draw_glyph(
            ui.painter(),
            centre,
            5.0,
            glyph,
            palette::outcome(polis_events::Outcome::Pending).color(),
        );
        ui.painter().text(
            Pos2::new(rect.min.x + 20.0, rect.center().y),
            Align2::LEFT_CENTER,
            name,
            font.clone(),
            palette::worker().color(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polis_events::WorkerId;

    /// PRD §12's *"shared selection and highlight state"*, in both directions.
    ///
    /// The rail is drawn before the map, so a single field would let the map
    /// light a rail row and never the other way round. The swap is what makes
    /// the two symmetrical, and this is the test that would have caught the
    /// half-working version.
    #[test]
    fn a_hover_written_by_either_view_is_read_by_the_other() {
        let a = ThreadId::of_session(polis_events::SessionId::new("a"));
        let b = ThreadId::of_session(polis_events::SessionId::new("b"));
        let mut state = ViewState::default();

        // Nothing pointed at, nothing highlighted.
        state.begin_frame();
        assert_eq!(state.hovered_thread(), None);
        assert!(!state.emphasises(&a));

        // The rail writes; the next frame is when every view reads it.
        state.hover_thread(&a);
        state.begin_frame();
        assert_eq!(state.hovered_thread(), Some(&a));
        assert!(state.emphasises(&a));
        assert!(!state.emphasises(&b));

        // The map writes; the rail reads the same value on the same terms.
        state.hover_thread(&b);
        state.begin_frame();
        assert_eq!(state.hovered_thread(), Some(&b));

        // The pointer leaves both views: nobody writes, and the highlight
        // clears itself rather than sticking to the last thing touched.
        state.begin_frame();
        assert_eq!(state.hovered_thread(), None);

        // Selection outlives the pointer, which is the difference between the
        // two and the reason they are separate fields.
        state.selected_thread = Some(a.clone());
        state.begin_frame();
        assert!(state.emphasises(&a));
        assert_eq!(state.hovered_thread(), None);
    }

    /// Following and selecting are different verbs (PRD §12): an operator can
    /// read about one thread while the camera is bound to another.
    #[test]
    fn selection_and_follow_are_not_the_same_field() {
        let a = ThreadId::of_session(polis_events::SessionId::new("a"));
        let b = ThreadId::of_session(polis_events::SessionId::new("b"));
        let state = ViewState {
            follow: Some(a.clone()),
            selected_thread: Some(b.clone()),
            ..ViewState::default()
        };
        assert!(state.emphasises(&b));
        assert!(!state.emphasises(&a));
        assert_eq!(state.follow.as_ref(), Some(&a));
    }

    #[test]
    fn a_worker_eases_toward_its_target_and_then_settles() {
        let mut motion = Motion::default();
        let id = WorkerId::new("w1");
        let (first, _) = motion.ease(&id, Pos2::new(100.0, 100.0));
        assert_eq!(first, Pos2::new(100.0, 100.0), "the first sight is a cut");

        let target = Pos2::new(400.0, 100.0);
        let mut moving = 0;
        let mut at = first;
        for _ in 0..200 {
            let (p, m) = motion.ease(&id, target);
            at = p;
            if m {
                moving += 1;
            } else {
                break;
            }
        }
        assert!(moving > 3, "it took {moving} frames, which is a teleport");
        assert_eq!(at, target, "and it did arrive");
        let (_, still_moving) = motion.ease(&id, target);
        assert!(
            !still_moving,
            "a settled worker must stop asking for frames"
        );
    }

    #[test]
    fn a_worker_that_left_the_world_is_forgotten() {
        let mut motion = Motion::default();
        let a = WorkerId::new("a");
        let b = WorkerId::new("b");
        motion.ease(&a, Pos2::ZERO);
        motion.ease(&b, Pos2::ZERO);
        motion.retain(&|id| id == &a);
        assert!(motion.workers.contains_key(&a));
        assert!(!motion.workers.contains_key(&b));
    }

    /// PRD §10.1 lists six glyphs and §10.2 three outcomes, and they are
    /// orthogonal channels: every pairing has to be drawable.
    #[test]
    fn every_glyph_and_outcome_pairing_is_drawable() {
        let ctx = egui::Context::default();
        // `FullOutput` carries the font-atlas delta and `epaint` panics if it is
        // dropped unapplied — there is no painter here to apply it to.
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            egui::Area::new("t".into()).show(ui, |ui| {
                for glyph in [
                    Glyph::HollowCircle,
                    Glyph::BarredCircle,
                    Glyph::FilledSquare,
                    Glyph::FilledTriangle,
                    Glyph::ConcentricCircles,
                    Glyph::Delegate,
                ] {
                    for outcome in [
                        polis_events::Outcome::Pending,
                        polis_events::Outcome::Done,
                        polis_events::Outcome::Failed,
                    ] {
                        draw_glyph(
                            ui.painter(),
                            Pos2::new(10.0, 10.0),
                            5.0,
                            glyph,
                            palette::outcome(outcome).color(),
                        );
                    }
                }
            });
        });
        let shapes = output.shapes.len();
        output.textures_delta.clear();
        assert!(shapes > 0, "the glyph pass emitted no geometry at all");
    }
}
