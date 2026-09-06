//! One frame of the window, drawn headlessly (PRD §12, §13).
//!
//! Everything else in `polis-app` is unit-tested against its own inputs. This
//! is the test that runs the thing: it builds a real city, a real snapshot with
//! threads, trails, operations, a territory and all three attention states, and
//! puts them through [`polis_app::mapview::draw`] inside a real `egui::Context`.
//!
//! It exists because the failures this task actually hit were all of one kind —
//! a layer that compiles and draws nothing, a tier that never changes, a
//! hit-test that never hits — and none of them are visible to a unit test of the
//! part in isolation. `egui::Context::run_ui` gives a full pass with no window,
//! no adapter and no surface, so the whole composition is checkable in CI.
//!
//! # What a `Context` with no renderer needs
//!
//! `FullOutput` carries the font-atlas and texture deltas, and `epaint` panics
//! if they are dropped unapplied — there is no painter here to apply them to.
//! Every pass therefore ends in `output.textures_delta.clear()`.

// The fixture is one long, ordered construction of a world and the two clock
// subtractions in it are against instants this test just created.
#![allow(clippy::too_many_lines, clippy::unchecked_time_subtraction)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Pos2, Rect, Vec2};
use polis_app::basemap::BaseMap;
use polis_app::camera::{Camera, ZoomTier};
use polis_app::clouds::Clouds;
use polis_app::mapview::{self, MapFrame, ViewState};
use polis_events::{
    Glyph, LogicalPath, Outcome, SessionId, ThreadId, ToolKind, WorkerId, WorktreeId,
};

use polis_layout::city::{self, City, LayoutInputs};
use polis_world::attention::{Attention, AttentionKind, DecisionSource};
use polis_world::contention::{Claim, Contention};
use polis_world::snapshot::WorldSnapshot;
use polis_world::territory::{Kernel, Territory};
use polis_world::{FileState, Operation, Thread, ThreadStatus, VisitStats, Worker};

fn city() -> City {
    let tree = polis_repo::synthetic::repository(220, 0x5EED);
    city::generate_with(&tree, &LayoutInputs::default())
}

/// A snapshot with something in every live layer: two threads, one with a
/// converged territory and workers, trails, operations of every glyph, and one
/// of each attention state.
fn populated(city: &City) -> WorldSnapshot {
    let now = Instant::now();
    let layout = Arc::new(city.layout.clone());
    let mut snapshot = WorldSnapshot::empty(Arc::clone(&layout));
    snapshot.at = now;

    let paths: Vec<LogicalPath> = layout.buildings.keys().take(12).cloned().collect();
    assert!(paths.len() >= 8, "the fixture city needs buildings");

    let mut files = BTreeMap::new();
    let session = SessionId::new("s-1");
    let alpha = ThreadId::of_session(session.clone());
    let beta = ThreadId::of_session(SessionId::new("s-2"));

    let mut thread = Thread::new(alpha.clone(), session.clone(), now);
    thread.title = Some("alpha".to_owned());
    thread.status = ThreadStatus::Working;
    thread.tool_calls = 40;
    thread.failures = 2;

    let glyphs = [
        Glyph::HollowCircle,
        Glyph::BarredCircle,
        Glyph::FilledSquare,
        Glyph::FilledTriangle,
        Glyph::ConcentricCircles,
        Glyph::Delegate,
    ];
    for (i, path) in paths.iter().enumerate() {
        let at = now - Duration::from_secs(i as u64 * 3);
        thread.trail.push_back((path.clone(), at));
        thread.visits.insert(
            path.clone(),
            VisitStats {
                count: u32::try_from(i).unwrap_or(0) + 1,
                writes: 1,
                first: at,
                last: at,
            },
        );
        thread.ops.push_back(Operation {
            path: Some(path.clone()),
            placement: polis_world::OpPlacement::Path(path.clone()),
            tool: ToolKind::Edit,
            glyph: glyphs[i % glyphs.len()],
            outcome: match i % 3 {
                0 => Outcome::Pending,
                1 => Outcome::Done,
                _ => Outcome::Failed,
            },
            worker: None,
            at,
            tool_use: None,
        });

        let mut file = FileState {
            reads: 3,
            writes: 1,
            lines_added: 12,
            lines_removed: 4,
            diff_lines: 16,
            ..FileState::default()
        };
        file.last_touched = Some(at);
        file.touched_by.push(alpha.clone());
        files.insert(path.clone(), file);
    }

    // Two workers, so tethers and the eased worker marks are drawn.
    for (i, path) in paths.iter().take(2).enumerate() {
        let mut worker = Worker::new(WorkerId::new(format!("w{i}")), now);
        worker.focus = Some(path.clone());
        worker.running = true;
        thread.workers.push(worker);
    }

    // A converged territory with real kernels, so the cloud layer has a field.
    let mut territory = Territory::for_extent(layout.extent);
    territory.claim = Some(paths[0].parent().unwrap_or_else(LogicalPath::root));
    for path in paths.iter().take(6) {
        let centre = layout
            .buildings
            .get(path)
            .map(|b| b.footprint.centroid())
            .expect("a building");
        territory.centre_of_mass = Some(centre);
        territory.kernels.push(Kernel {
            centre,
            weight: 1.4,
            radius: layout.extent * 0.06,
            at: now,
        });
    }
    thread.territory = territory;

    let mut idle = Thread::new(beta.clone(), SessionId::new("s-2"), now);
    idle.title = Some("beta".to_owned());
    idle.status = ThreadStatus::Waiting;
    idle.territory = Territory::for_extent(layout.extent);
    idle.territory.centre_of_mass = layout
        .buildings
        .get(&paths[7])
        .map(|b| b.footprint.centroid());

    let contention = Contention::of(
        Claim::write(alpha.clone(), paths[1].clone(), now).in_checkout(WorktreeId::PRIMARY, None),
        Claim::write(beta.clone(), paths[1].clone(), now).in_checkout(WorktreeId::PRIMARY, None),
    );
    // Two seconds old, so PRD §11.4's ≤400 ms arrival pulse has already run its
    // course: in a fixture whose clock never advances a fresh mark would pulse
    // for ever and the window would never go idle.
    let arrived = now - Duration::from_secs(2);
    snapshot.attention = vec![
        Attention::new(
            AttentionKind::NeedsDecision {
                thread: beta.clone(),
                at: Some(paths[3].clone()),
                source: DecisionSource::PermissionRequest,
            },
            arrived,
        ),
        Attention::new(
            AttentionKind::Done {
                thread: alpha.clone(),
                verified: false,
            },
            arrived,
        ),
        Attention::new(AttentionKind::Contention(Box::new(contention)), arrived),
    ];

    snapshot.threads = vec![thread, idle];
    snapshot.files = Arc::new(files);
    snapshot.generation = 1;
    snapshot
}

/// Runs one full `egui` pass and hands back the map frame and the shape count.
///
/// `zoom` is applied **inside** the pass, against the rectangle `egui` actually
/// hands out. Setting it outside would set it against a rectangle the pass then
/// replaces, and `Camera::set_viewport` preserves the zoom *factor* rather than
/// the absolute scale — so the tier would be decided by a viewport that never
/// existed.
fn frame(
    ctx: &egui::Context,
    base: &BaseMap,
    camera: &mut Camera,
    clouds: &mut Clouds,
    snapshot: &WorldSnapshot,
    state: &mut ViewState,
    zoom: Option<f32>,
    input: egui::RawInput,
) -> (MapFrame, usize) {
    let mut out = None;
    let expected = input.screen_rect.expect("a sized viewport");
    let mut full = ctx.run_ui(input, |ui| {
        let rect = ui.max_rect();
        assert!(
            (rect.width() - expected.width()).abs() < 1.0,
            "the pass ran over {rect:?}, not {expected:?}"
        );
        camera.set_viewport(rect);
        if let Some(zoom) = zoom {
            camera.set_zoom(zoom);
        }
        out = Some(mapview::draw(
            ui,
            rect,
            base,
            camera,
            clouds,
            snapshot,
            state,
            12,
            1.0 / 60.0,
        ));
    });
    let shapes = full.shapes.len();
    full.textures_delta.clear();
    (out.expect("the map was drawn"), shapes)
}

/// A pass over a viewport of a known size.
///
/// `RawInput::screen_rect` is the field that decides it, and its default is
/// **10 000 x 10 000** — a rect big enough that every zoom lands in
/// [`ZoomTier::Building`], which is exactly how the first version of the tier
/// test came out green-looking and wrong.
fn raw_input(size: Vec2) -> egui::RawInput {
    let rect = Rect::from_min_size(Pos2::ZERO, size);
    let mut input = egui::RawInput {
        screen_rect: Some(rect),
        ..Default::default()
    };
    input
        .viewports
        .entry(input.viewport_id)
        .or_default()
        .inner_rect = Some(rect);
    input
}

#[test]
fn a_frame_of_the_map_draws_every_live_layer() {
    let city = city();
    let ctx = egui::Context::default();
    let layout = Arc::new(city.layout.clone());
    let base = BaseMap::render(&ctx, &city, &layout, false);
    let snapshot = populated(&city);

    let viewport = Rect::from_min_size(Pos2::ZERO, Vec2::new(1200.0, 800.0));
    let mut camera = Camera::fit(base.edge(), base.geometry.median_building_px, viewport);
    let mut clouds = Clouds::default();
    let mut state = ViewState::default();

    let (map, shapes) = frame(
        &ctx,
        &base,
        &mut camera,
        &mut clouds,
        &snapshot,
        &mut state,
        None,
        raw_input(viewport.size()),
    );

    assert!(shapes > 200, "the frame emitted only {shapes} shapes");
    assert!(
        map.labels_placed > 0,
        "no label survived the declutterer at all"
    );
    assert!(
        map.buildings_drawn > 0 || map.tier == ZoomTier::City,
        "buildings must be drawn at every tier above City"
    );
    // Through the census rather than through two loose fields, because the
    // census is what the status bar prints and a green `shown >= 1` beside a bar
    // that says `0 CLOUDS` would be the two disagreeing again.
    let census = clouds.census(camera.scale());
    assert!(
        census.shown >= 1,
        "the converged territory got no cloud: {}",
        census.reason()
    );
    assert!(
        census.kernels >= 6,
        "every kernel should reach the field: {}",
        census.kernels
    );
    assert!(
        !census.sub_pixel(),
        "and they are wide enough to draw at the fitted camera: {}",
        census.reason()
    );
    assert!(map.draw_ms < 200.0, "one frame took {} ms", map.draw_ms);
}

/// PRD §12's three tiers, exercised through the real draw path rather than
/// through `Camera::tier` alone: each one has to produce a frame, and the City
/// tier has to draw **no** individual buildings.
#[test]
fn every_zoom_tier_draws_and_the_city_tier_draws_no_buildings() {
    let city = city();
    let ctx = egui::Context::default();
    let layout = Arc::new(city.layout.clone());
    let base = BaseMap::render(&ctx, &city, &layout, false);
    let snapshot = populated(&city);
    let viewport = Rect::from_min_size(Pos2::ZERO, Vec2::new(1200.0, 800.0));

    let mut seen: Vec<(ZoomTier, usize)> = Vec::new();
    for zoom in [0.15f32, 1.0, 20.0] {
        let mut camera = Camera::fit(base.edge(), base.geometry.median_building_px, viewport);
        let mut clouds = Clouds::default();
        let mut state = ViewState::default();
        let (map, shapes) = frame(
            &ctx,
            &base,
            &mut camera,
            &mut clouds,
            &snapshot,
            &mut state,
            Some(zoom),
            raw_input(viewport.size()),
        );
        assert!(shapes > 20, "tier {:?} drew {shapes} shapes", map.tier);
        seen.push((map.tier, map.buildings_drawn));
    }

    assert_eq!(seen[0].0, ZoomTier::City, "{seen:?}");
    assert_eq!(
        seen[0].1, 0,
        "PRD §12: at City tier buildings are sub-pixel and are not drawn individually"
    );
    assert_eq!(seen[1].0, ZoomTier::District, "{seen:?}");
    assert!(seen[1].1 > 0, "{seen:?}");
    assert_eq!(seen[2].0, ZoomTier::Building, "{seen:?}");
    assert!(seen[2].1 > 0, "{seen:?}");
}

/// The hit-test, through the same pass the window uses: put the pointer on a
/// building's centre and the frame has to report that building.
#[test]
fn the_pointer_over_a_building_reports_that_building() {
    let city = city();
    let ctx = egui::Context::default();
    let layout = Arc::new(city.layout.clone());
    let base = BaseMap::render(&ctx, &city, &layout, false);
    let snapshot = WorldSnapshot::empty(Arc::clone(&layout));
    let viewport = Rect::from_min_size(Pos2::ZERO, Vec2::new(1200.0, 800.0));

    let mut hits = 0;
    let mut tried = 0;
    for (path, shape) in base.geometry.buildings.iter().take(24) {
        let mut camera = Camera::fit(base.edge(), base.geometry.median_building_px, viewport);
        let mut clouds = Clouds::default();
        let mut state = ViewState::default();

        // Pass one settles the camera against the rectangle `egui` hands out and
        // registers the widget rect; only then is the building's screen position
        // knowable. Pass two puts the pointer there. That is exactly the order
        // the window sees when the mouse enters.
        let (_, _) = frame(
            &ctx,
            &base,
            &mut camera,
            &mut clouds,
            &snapshot,
            &mut state,
            Some(6.0),
            raw_input(viewport.size()),
        );
        camera.cut_to(shape.centre);
        let mut input = raw_input(viewport.size());
        input
            .events
            .push(egui::Event::PointerMoved(camera.to_screen(shape.centre)));
        let (map, _) = frame(
            &ctx,
            &base,
            &mut camera,
            &mut clouds,
            &snapshot,
            &mut state,
            None,
            input,
        );

        tried += 1;
        if map.hovered.as_ref() == Some(path) {
            hits += 1;
        }
    }
    assert!(tried > 10);
    assert!(
        hits * 4 >= tried * 3,
        "{hits} of {tried} buildings were found under their own centre"
    );
}

/// PRD §13.1's idle budget is a repaint discipline, and the map layer's half of
/// it is this: a still world with no tween in flight must not ask for another
/// frame.
#[test]
fn a_still_world_asks_for_no_further_frames() {
    let city = city();
    let ctx = egui::Context::default();
    let layout = Arc::new(city.layout.clone());
    let base = BaseMap::render(&ctx, &city, &layout, false);
    let snapshot = populated(&city);
    let viewport = Rect::from_min_size(Pos2::ZERO, Vec2::new(1200.0, 800.0));
    let mut camera = Camera::fit(base.edge(), base.geometry.median_building_px, viewport);
    let mut clouds = Clouds::default();
    let mut state = ViewState::default();

    // The first frames ease the worker marks into place from nothing.
    let mut animating = 0;
    let mut map = None;
    for _ in 0..64 {
        let (f, _) = frame(
            &ctx,
            &base,
            &mut camera,
            &mut clouds,
            &snapshot,
            &mut state,
            None,
            raw_input(viewport.size()),
        );
        if f.animating {
            animating += 1;
        }
        map = Some(f);
    }
    assert!(
        !map.expect("frames were drawn").animating,
        "the map is still asking for frames after {animating} of 64 settled"
    );
}

/// The base map is cached to a texture and redrawn only on layout change
/// (PRD §13). Identity, not content: a republished snapshot that reuses the
/// same `Arc` must not cost a rasterisation.
#[test]
fn the_base_map_is_reused_until_the_layout_changes() {
    let city = city();
    let ctx = egui::Context::default();
    let layout = Arc::new(city.layout.clone());
    let base = BaseMap::render(&ctx, &city, &layout, false);
    assert!(base.matches(&layout, false));
    assert!(
        !base.matches(&layout, true),
        "the street layer is part of the key"
    );

    let republished = Arc::clone(&layout);
    assert!(
        base.matches(&republished, false),
        "a snapshot that reuses the layout must reuse the texture"
    );

    let regenerated = Arc::new(city.layout.clone());
    assert!(
        !base.matches(&regenerated, false),
        "a new layout is a new base map"
    );
}

/// An empty sky has to say why (PRD §6.2, §10.4).
///
/// `polis-render`'s headless path has asserted
/// `frame.cloud.unplaced > 0 && frame.cloud.shown == 0` since `CloudCensus` was
/// written; the window had no equivalent, because it called
/// `territory::visible_clouds` — a wrapper that returns the chosen territories
/// and drops the three counts saying what happened to everybody else. So the
/// status bar could only ever print `0 clouds (0 kernels)`, which states the
/// symptom twice and the cause not at all, and the operator's `i dont see any
/// clouds` had nothing on screen to pull on.
///
/// The snapshot here is the shape that produces it: two threads that have made
/// tool calls and whose evidence has not agreed on an ancestor.
#[test]
fn a_window_with_no_clouds_says_which_gate_took_them() {
    let city = city();
    let ctx = egui::Context::default();
    let layout = Arc::new(city.layout.clone());
    let base = BaseMap::render(&ctx, &city, &layout, false);

    let now = Instant::now();
    let mut snapshot = WorldSnapshot::empty(Arc::clone(&layout));
    snapshot.at = now;
    snapshot.generation = 1;
    for n in 0..2 {
        let session = SessionId::new(format!("s-{n}"));
        let mut thread = Thread::new(ThreadId::of_session(session.clone()), session, now);
        thread.status = ThreadStatus::Working;
        thread.tool_calls = 417;
        // A territory with no claim, no lobes and no kernels: PRD §6.2's
        // *"unplaced marker in the status rail"*.
        thread.territory = Territory::for_extent(layout.extent);
        snapshot.threads.push(thread);
    }

    let viewport = Rect::from_min_size(Pos2::ZERO, Vec2::new(1200.0, 800.0));
    let mut camera = Camera::fit(base.edge(), base.geometry.median_building_px, viewport);
    let mut clouds = Clouds::default();
    let mut state = ViewState::default();
    let _ = frame(
        &ctx,
        &base,
        &mut camera,
        &mut clouds,
        &snapshot,
        &mut state,
        None,
        raw_input(viewport.size()),
    );

    let census = clouds.census(camera.scale());
    assert_eq!(census.shown, 0, "nothing converged, so nothing is drawn");
    assert_eq!(
        census.unplaced, 2,
        "and both threads are accounted for rather than lost"
    );
    assert_eq!(
        census.threads(),
        snapshot.threads.len(),
        "every thread lands in exactly one bucket"
    );
    assert!(
        census.withheld(),
        "which is what makes the status bar print a reason at all"
    );
    let reason = census.reason();
    assert!(
        reason.contains("UNCONVERGED"),
        "the bar has to name the gate that fired, not restate the count: {reason}"
    );
}

/// Rain is the only channel that fires on *every* call, so it is also the only
/// one whose absence is invisible: a map with no rain and a map whose rain is
/// broken look identical. This is the test that tells them apart.
///
/// It drives one frame with every operation fresh and one with every operation
/// long stale, and asserts the fresh pass emits strictly more shapes and reports
/// itself as animating. "No calls, no rain" is the intended behaviour, so the
/// stale pass must be quiet — both halves are the claim.
#[test]
fn rain_falls_while_calls_are_arriving_and_stops_when_they_are_not() {
    let city = city();
    let ctx = egui::Context::default();
    let layout = Arc::new(city.layout.clone());
    let base = BaseMap::render(&ctx, &city, &layout, false);

    let viewport = Rect::from_min_size(Pos2::ZERO, Vec2::new(1200.0, 800.0));

    // One pass at a given operation age, everything else held identical.
    let pass = |age: Duration| {
        let mut snapshot = populated(&city);
        let at = snapshot.at - age;
        for thread in &mut snapshot.threads {
            for op in &mut thread.ops {
                op.at = at;
            }
        }
        let mut camera = Camera::fit(base.edge(), base.geometry.median_building_px, viewport);
        let mut clouds = Clouds::default();
        let mut state = ViewState::default();
        frame(
            &ctx,
            &base,
            &mut camera,
            &mut clouds,
            &snapshot,
            &mut state,
            None,
            raw_input(viewport.size()),
        )
    };

    let (wet, wet_shapes) = pass(Duration::from_millis(200));
    let (_dry, dry_shapes) = pass(Duration::from_secs(600));

    assert!(
        wet_shapes > dry_shapes,
        "a frame with calls arriving drew {wet_shapes} shapes and one with none drew \
         {dry_shapes} - rain is not reaching the canvas"
    );
    assert!(
        wet.animating,
        "rain is on screen but the frame did not ask for another, so it will freeze mid-ripple"
    );
}
