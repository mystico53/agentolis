//! What the map does when the operator asks about one thread, drawn headlessly
//! through the real window pass.
//!
//! # What this is protecting, and the three rules it has had
//!
//! The operator ran `polis watch` on a repository with nine live threads and
//! said *"super messy"*. The largest single object in that frame was the
//! delegation fan: `polis_app::mapview` drew **one line across the whole map
//! per worker**, uncapped, for every thread at once, and one session on this
//! machine has a hundred workers.
//!
//! So the lines were put behind a gate — the trail and the tethers of the one
//! thread being pointed at, selected or followed — and the map still came back
//! *"still lines everywhere!!"*, because pointing at a card is something an
//! operator does while reading the list, and it sprouted a capped fan plus up
//! to 192 trail steps every time the pointer crossed a row.
//!
//! The third rule is the operator's own: *"instead of these lines when hovering
//! over a thread card, highlight the cloud, make it brighter"*. The card and the
//! cloud already share one hue (PRD §11.4), so the question a hover asks —
//! *which shape on the map is this row?* — is answered by lighting that shape
//! up, and nothing is drawn that was not already there.
//!
//! Two claims, and no unit test of any one piece can see either, because both
//! are about the whole composed frame:
//!
//! 1. asking about a thread adds **no lines at all**: an interrogated frame
//!    emits the shapes an ambient one does, where rule one drew a line per
//!    worker of every thread and rule two drew a capped fan plus a trail;
//! 2. the answer moved into the cloud layer instead — the textures that pass
//!    uploads carry **more light** than the ambient pass's, and by a margin that
//!    is one cloud brightening rather than the whole sky.
//!
//! Both are measured the same way: the shapes and the texture uploads of one
//! pass over one snapshot, differenced against a pass that asked nothing.

// The fixture is one long ordered construction of a world, and its clock
// arithmetic is against instants this test just made.
#![allow(clippy::too_many_lines, clippy::unchecked_time_subtraction)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Pos2, Rect, Vec2};
use polis_app::basemap::BaseMap;
use polis_app::camera::Camera;
use polis_app::clouds::Clouds;
use polis_app::mapview::{self, ViewState};
use polis_events::{Glyph, LogicalPath, Outcome, SessionId, ThreadId, ToolKind, WorkerId};
use polis_layout::city::{self, City, LayoutInputs};
use polis_world::snapshot::WorldSnapshot;
use polis_world::territory::{Kernel, Territory};
use polis_world::{Operation, Thread, ThreadStatus, Worker};

/// How many workers the orchestrator in the fixture has.
///
/// Chosen to be well past [`TETHERS_PER_THREAD`] and in the range the operator
/// actually runs: their live sessions on this machine carry 17, 32 and 101.
const WORKERS: usize = 40;

fn city() -> City {
    let tree = polis_repo::synthetic::repository(220, 0x5EED);
    city::generate_with(&tree, &LayoutInputs::default())
}

/// One orchestrator with [`WORKERS`] running workers on distinct buildings, a
/// converged territory to anchor them to, and a second thread that has workers
/// of its own — because "only the asked-about thread" is a claim about the
/// threads that were *not* asked about.
fn fleet(city: &City) -> (WorldSnapshot, ThreadId, ThreadId) {
    let now = Instant::now();
    let layout = Arc::new(city.layout.clone());
    let mut snapshot = WorldSnapshot::empty(Arc::clone(&layout));
    snapshot.at = now;

    let paths: Vec<LogicalPath> = layout.buildings.keys().take(WORKERS * 2).cloned().collect();
    assert!(
        paths.len() >= WORKERS + 8,
        "the fixture city needs {} buildings, it has {}",
        WORKERS + 8,
        paths.len()
    );

    let alpha = ThreadId::of_session(SessionId::new("s-alpha"));
    let beta = ThreadId::of_session(SessionId::new("s-beta"));
    let mut threads = Vec::new();

    for (which, (id, session, workers, slot)) in [
        (alpha.clone(), "s-alpha", WORKERS, 0u8),
        (beta.clone(), "s-beta", 6, 1u8),
    ]
    .into_iter()
    .enumerate()
    {
        let mut thread = Thread::new(id, SessionId::new(session), now);
        // Two threads that land on one hue slot share a field **layer**, and a
        // shared layer is one cloud — `polis_world::Thread::layer`. Inside a
        // `World` the layer ring makes them exclusive; a fixture has no set to
        // be exclusive against, so it says which is which itself. This test
        // needs two clouds: one to light, one to leave alone.
        thread.tint = slot;
        thread.layer = u16::from(slot);
        thread.title = Some(format!("thread {which}"));
        thread.status = ThreadStatus::Working;
        let base = which * WORKERS;
        for i in 0..workers {
            let path = paths[base + i].clone();
            let mut worker = Worker::new(WorkerId::new(format!("w{which}-{i}")), now);
            worker.focus = Some(path.clone());
            // Every worker running, so every tether is one `Shape::line` and
            // the shape difference between two passes is the tether *count*.
            worker.running = true;
            thread.workers.push(worker);
        }
        // A little history, so the frame is not tethers on an empty map.
        for i in 0..6 {
            let path = paths[base + i].clone();
            let at = now - Duration::from_secs(i as u64 * 3);
            thread.trail.push_back((path.clone(), at));
            thread.ops.push_back(Operation {
                path: Some(path.clone()),
                placement: polis_world::OpPlacement::Path(path.clone()),
                tool: ToolKind::Edit,
                glyph: Glyph::BarredCircle,
                outcome: Outcome::Done,
                worker: None,
                at,
                tool_use: None,
            });
        }
        let mut territory = Territory::for_extent(layout.extent);
        territory.claim = Some(paths[base].parent().unwrap_or_else(LogicalPath::root));
        for path in &paths[base..base + 6] {
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
        threads.push(thread);
    }

    snapshot.threads = threads;
    snapshot.generation = 1;
    (snapshot, alpha, beta)
}

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

/// What one pass put on the screen: its shapes, and the light in its textures.
#[derive(Debug, Clone, Copy)]
struct Pass {
    shapes: usize,
    light: f64,
}

/// Every channel of every texel a pass uploaded, summed.
///
/// The cloud layer is two **premultiplied** images, so a transparent texel
/// counts zero and a brighter mark counts more — which is exactly the claim
/// being made: the cloud of the asked-about thread puts more light on the map.
/// The base map is uploaded once, on the first pass; every number here is taken
/// from the last one.
fn light(delta: &egui::TexturesDelta) -> f64 {
    delta
        .set
        .values()
        .flatten()
        .map(|delta| match &delta.image {
            egui::epaint::ImageData::Color(image) => image
                .pixels
                .iter()
                .map(|p| f64::from(p.r()) + f64::from(p.g()) + f64::from(p.b()))
                .sum::<f64>(),
        })
        .sum()
}

/// `frames` passes over one snapshot with one cloud layer, and the last of them.
///
/// More than one pass because the clouds are a tween: a brand-new field starts
/// at zero and rises (`polis_render::live::CloudTween::advance`), so a single
/// pass measures a sky that has barely arrived. A second of presentation time is
/// well past `CLOUD_TWEEN_RATE`, and it also puts the base map's own upload
/// behind us.
fn run(
    ctx: &egui::Context,
    base: &BaseMap,
    snapshot: &WorldSnapshot,
    state: &mut ViewState,
    viewport: Rect,
    frames: usize,
) -> Pass {
    let mut camera = Camera::fit(base.edge(), base.geometry.median_building_px, viewport);
    let mut clouds = Clouds::default();
    let mut last = Pass {
        shapes: 0,
        light: 0.0,
    };
    for _ in 0..frames {
        state.begin_frame();
        let mut full = ctx.run_ui(raw_input(viewport.size()), |ui| {
            let rect = ui.max_rect();
            camera.set_viewport(rect);
            let _ = mapview::draw(
                ui,
                rect,
                base,
                &mut camera,
                &mut clouds,
                snapshot,
                state,
                12,
                polis_app::config::Look::default(),
                1.0 / 60.0,
            );
        });
        last.shapes = full.shapes.len();
        // The **last** upload, not the last pass. Once the tween arrives the
        // layer stops repainting — PRD §13.1's idle budget, *"< 2 % of one
        // core"* — so the final passes upload nothing at all, and the frame
        // worth measuring is the settled cloud that the window then leaves
        // alone.
        if !full.textures_delta.set.is_empty() {
            last.light = light(&full.textures_delta);
        }
        full.textures_delta.clear();
    }
    last
}

/// One second of presentation time, which is long past the cloud tween.
const FRAMES: usize = 60;

/// The measurement, and the two claims that came out of it.
///
/// Printed as well as asserted: `cargo test -p polis-app --test asked_about --
/// --nocapture` is how the numbers below get re-taken after a change to the
/// notation.
#[test]
fn asking_about_a_thread_draws_no_lines_and_lights_its_cloud() {
    let city = city();
    let ctx = egui::Context::default();
    let layout = Arc::new(city.layout.clone());
    let base = BaseMap::render(&ctx, &city, &layout, false);
    let (snapshot, alpha, beta) = fleet(&city);
    let viewport = Rect::from_min_size(Pos2::ZERO, Vec2::new(1200.0, 800.0));

    let mut state = ViewState::default();
    let ambient = run(&ctx, &base, &snapshot, &mut state, viewport, FRAMES);

    // Following is one of PRD §12's three ways of saying "this one", and the
    // only one that does not also lift the anchor's highlight ring — so a
    // difference in the shape count would be a line and nothing else.
    let mut asked = ViewState::default();
    asked.follow = Some(alpha.clone());
    let followed = run(&ctx, &base, &snapshot, &mut asked, viewport, FRAMES);

    let legacy: usize = snapshot
        .threads
        .iter()
        .map(|t| t.workers.iter().filter(|w| w.focus.is_some()).count())
        .sum();
    println!(
        "shapes: ambient {} - asked-about {}; the first rule drew {legacy} lines (every worker \
         of every thread) and the second a capped fan plus a trail",
        ambient.shapes, followed.shapes
    );
    println!(
        "light: ambient {:.0} - asked-about {:.0} = {:+.1} %",
        ambient.light,
        followed.light,
        (followed.light / ambient.light - 1.0) * 100.0
    );

    assert_eq!(
        followed.shapes, ambient.shapes,
        "asking about a thread with {WORKERS} workers changed the map's shape count — the \
         answer is the cloud, not a line"
    );

    // And the cloud carries it. One of the two territories is drawn at
    // `clouds::LIT_GAIN`, so the sky as a whole gains a fraction of that gain —
    // not all of it, which is the half of the claim that says only the
    // asked-about thread is lit.
    assert!(
        followed.light > ambient.light * 1.05,
        "asking about a thread did not brighten its cloud: {:.0} against {:.0}",
        followed.light,
        ambient.light
    );
    // The ceiling is the gain itself: a highlight can at most take the sky to
    // what it would be if the one cloud it lights were the whole of it, and the
    // texel-for-texel claim that the *other* layers are untouched is made where
    // it can be made exactly — `clouds::tests`.
    assert!(
        followed.light < ambient.light * 1.81,
        "the sky brightened past LIT_GAIN, so more than one cloud was lit: {:.0} against {:.0}",
        followed.light,
        ambient.light
    );

    // The other thread is the control: it has a territory of its own, and
    // asking about it has to light that one instead — same rule, same cost.
    let mut other = ViewState::default();
    other.follow = Some(beta);
    let beta_lit = run(&ctx, &base, &snapshot, &mut other, viewport, FRAMES);
    println!(
        "light: the second thread's cloud is the smaller one — {:.0}, {:+.1} %",
        beta_lit.light,
        (beta_lit.light / ambient.light - 1.0) * 100.0
    );
    assert!(
        beta_lit.light > ambient.light * 1.05,
        "asking about the second thread did not brighten its cloud: {:.0} against {:.0}",
        beta_lit.light,
        ambient.light
    );
    assert_eq!(
        beta_lit.shapes, ambient.shapes,
        "the second thread drew lines the first one did not"
    );

    // Selecting is the second verb and has to reach the same rule. It also
    // lifts the anchor's highlight ring, which is a *shape* — the one shape an
    // interrogation is still allowed to add, and it stands off the anchor
    // rather than crossing the map.
    let mut selected = ViewState::default();
    selected.selected_thread = Some(alpha);
    let with_selection = run(&ctx, &base, &snapshot, &mut selected, viewport, FRAMES);
    assert_eq!(
        with_selection.shapes,
        ambient.shapes + 1,
        "selecting must reach the same rule as following, plus the one highlight ring only \
         selection lifts"
    );
    assert!(
        with_selection.light > ambient.light * 1.05,
        "selecting a thread did not brighten its cloud: {:.0} against {:.0}",
        with_selection.light,
        ambient.light
    );
}
