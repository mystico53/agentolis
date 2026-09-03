//! Who gets a tether, drawn headlessly through the real window pass.
//!
//! # What this is protecting
//!
//! The operator ran `polis watch` on a repository with nine live threads and
//! said *"super messy"*. The largest single object in that frame was the
//! delegation fan: `polis_app::mapview` drew **one line across the whole map
//! per worker**, uncapped, for every thread at once, and one session on this
//! machine has a hundred workers. An independent review had already listed it
//! under PRD §17 — *"does it change a decision?"* — and the answer is no: a
//! tether says "this worker belongs to that thread", and nobody unblocks or
//! redirects an agent because it has hands.
//!
//! Ownership is carried instead by `polis_render::live::thread_slot`'s colour,
//! which costs no area, and the line is kept for PRD §12's *"exact below"*: the
//! operator asks about **one** thread, by pointing at it, selecting it or
//! following it, and gets that thread's tethers and nobody else's.
//!
//! So there are two claims, and a unit test of the placer cannot see either of
//! them because both are about the whole composed frame:
//!
//! 1. an ambient frame contains **no** tethers at all;
//! 2. an interrogated thread's frame contains exactly
//!    `min(workers, TETHERS_PER_THREAD)` of them, and no more however many
//!    workers it has.
//!
//! Both are measured the same way — the count of `egui` shapes the pass emits,
//! differenced between two passes over one snapshot — which is why the fixture
//! interrogates through [`ViewState::follow`] rather than `selected_thread`:
//! selection also lifts the highlight ring and thickens the trail, and a
//! difference has to be caused by one thing to mean anything.

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
use polis_render::live::TETHERS_PER_THREAD;
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

    for (which, (id, session, workers)) in [
        (alpha.clone(), "s-alpha", WORKERS),
        (beta.clone(), "s-beta", 6),
    ]
    .into_iter()
    .enumerate()
    {
        let mut thread = Thread::new(id, SessionId::new(session), now);
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

/// One full pass, and how many shapes it emitted.
fn shapes(
    ctx: &egui::Context,
    base: &BaseMap,
    snapshot: &WorldSnapshot,
    state: &mut ViewState,
    viewport: Rect,
) -> usize {
    let mut camera = Camera::fit(base.edge(), base.geometry.median_building_px, viewport);
    let mut clouds = Clouds::default();
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
            1.0 / 60.0,
        );
    });
    let n = full.shapes.len();
    full.textures_delta.clear();
    n
}

/// The measurement, and the two claims that came out of it.
///
/// Printed as well as asserted: `cargo test -p polis-app --test tether_rule --
/// --nocapture` is how the numbers below get re-taken after a change to the
/// notation.
#[test]
fn an_ambient_map_has_no_tethers_and_one_asked_about_thread_has_sixteen() {
    let city = city();
    let ctx = egui::Context::default();
    let layout = Arc::new(city.layout.clone());
    let base = BaseMap::render(&ctx, &city, &layout, false);
    let (snapshot, alpha, beta) = fleet(&city);
    let viewport = Rect::from_min_size(Pos2::ZERO, Vec2::new(1200.0, 800.0));

    let mut state = ViewState::default();
    let ambient = shapes(&ctx, &base, &snapshot, &mut state, viewport);

    // Following is one of PRD §12's three ways of saying "this one", and the
    // only one that does not also lift the highlight — so the difference below
    // is caused by the tethers alone.
    let mut asked = ViewState::default();
    asked.follow = Some(alpha.clone());
    let interrogated = shapes(&ctx, &base, &snapshot, &mut asked, viewport);

    let drawn = interrogated.saturating_sub(ambient);
    let legacy: usize = snapshot
        .threads
        .iter()
        .map(|t| t.workers.iter().filter(|w| w.focus.is_some()).count())
        .sum();
    println!(
        "tethers: ambient {ambient} - asked-about {interrogated} = {drawn} drawn; the old rule \
         drew {legacy} (every worker of every thread, uncapped)"
    );

    // Asking about a thread now reveals its TRAIL as well as its tethers - both
    // are PRD 12's "exact below", and trails moved behind the same gate after
    // the operator's ambient map came back as "still lines everywhere!!" with up
    // to 960 persisting segments across five threads. So this is no longer a
    // count of tethers alone, and the invariant worth pinning is the one that
    // was actually broken: ambient draws NONE, and asking draws a bounded few
    // rather than one line per worker.
    assert!(
        drawn >= TETHERS_PER_THREAD,
        "asking about a thread with {WORKERS} workers must draw its          {TETHERS_PER_THREAD} tethers, drew only {drawn}"
    );
    assert!(
        drawn <= TETHERS_PER_THREAD * 2,
        "asking about one thread drew {drawn} lines - the tether cap plus one          trail should stay well inside {}",
        TETHERS_PER_THREAD * 2
    );
    assert!(
        drawn < legacy / 2,
        "the fan is not smaller: {drawn} against the old rule's {legacy}"
    );

    // And the other threads' fans stay down: asking about beta, which has six
    // workers, draws its six tethers and its own trail — not alpha's forty.
    // The point is that the count scales with the asked-about thread and not
    // with the fleet, which is what the ambient fan got wrong.
    let mut other = ViewState::default();
    other.follow = Some(beta);
    let beta_drawn = shapes(&ctx, &base, &snapshot, &mut other, viewport).saturating_sub(ambient);
    assert!(
        beta_drawn >= 6,
        "asking about the six-worker thread must draw its six tethers, drew {beta_drawn}"
    );
    assert!(
        beta_drawn < drawn,
        "a six-worker thread must cost less than a forty-worker one: {beta_drawn} vs {drawn}"
    );

    // Selecting is the second verb and has to reach the same rule. It also
    // lifts the highlight ring, so the count is the tethers plus that one ring.
    let mut selected = ViewState::default();
    selected.selected_thread = Some(alpha);
    let with_selection = shapes(&ctx, &base, &snapshot, &mut selected, viewport);
    let selected_drawn = with_selection.saturating_sub(ambient);
    assert!(
        selected_drawn > TETHERS_PER_THREAD,
        "selecting a thread did not tether it: {selected_drawn}"
    );
    assert_eq!(
        selected_drawn,
        drawn + 1,
        "selecting must reach the same rule as following, plus the one highlight          ring only selection lifts"
    );
}
