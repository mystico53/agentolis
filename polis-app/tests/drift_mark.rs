//! PRD §10.4's leading-edge mark, through the real draw path.
//!
//! > Compute a drift vector from the centre of mass over the last 60s and render
//! > a leading-edge mark when its magnitude exceeds a threshold. **This is the
//! > redirect signal, and it is the one thing here that no existing tool
//! > provides.**
//!
//! `polis-world` decides *whether* and *where*; this asserts that the decision
//! reaches pixels. It exists because the failure mode this task actually hit
//! twice is a layer that compiles, computes the right answer, and draws nothing:
//! `egui::Context::run_ui` gives a full pass with no window and no adapter, so
//! the whole composition is checkable in CI.
//!
//! # What it is worth
//!
//! Read [`polis_world::territory::DRIFT_VERDICT`] before treating this mark as
//! load-bearing. Measured over sixteen of the operator's real sessions it fires
//! 7 % of the time and predicts a scope change with a lift of 1.14 over a 30.4 %
//! base rate — which is to say it does not predict one. This test asserts that
//! the notation works, not that the signal is worth drawing.

use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Pos2, Rect, Vec2};
use polis_app::basemap::BaseMap;
use polis_app::camera::Camera;
use polis_app::clouds::Clouds;
use polis_app::mapview::{self, ViewState};
use polis_events::{LogicalPath, SessionId, ThreadId};
use polis_layout::city::{self, City, LayoutInputs};
use polis_world::snapshot::WorldSnapshot;
use polis_world::territory::{Territory, DRIFT_WINDOW};
use polis_world::{Observation, PathScope, Thread, ThreadStatus};

fn city() -> City {
    let tree = polis_repo::synthetic::repository(220, 0x5EED);
    city::generate_with(&tree, &LayoutInputs::default())
}

/// A snapshot with one thread whose territory is established at `home` and whose
/// last minute of work is at `recent`.
///
/// `recent == home` is the control: the same thread, the same amount of
/// evidence, no migration.
fn drifting(city: &City, home: &[LogicalPath], recent: &[LogicalPath]) -> WorldSnapshot {
    let now = Instant::now();
    let layout = Arc::new(city.layout.clone());
    let mut snapshot = WorldSnapshot::empty(Arc::clone(&layout));
    snapshot.at = now;

    let session = SessionId::new("drifter");
    let id = ThreadId::of_session(session.clone());
    let mut thread = Thread::new(id.clone(), session, now);
    thread.status = ThreadStatus::Working;
    thread.last_activity = now;

    let mut territory = Territory::for_extent(layout.extent);
    // The established scope, older than the window — otherwise there is nothing
    // for the last minute's work to be a departure *from*.
    let established = now
        .checked_sub(DRIFT_WINDOW + Duration::from_secs(10))
        .expect("the process has been up for a minute");
    let observe = |t: &mut Territory, paths: &[LogicalPath], at: Instant| {
        for path in paths {
            let centre = layout
                .buildings
                .get(path)
                .map(|b| b.footprint.centroid())
                .expect("a building");
            t.observe(
                &Observation {
                    thread: ThreadId::of_session(SessionId::new("drifter")),
                    worker: None,
                    path: path.clone(),
                    scope: PathScope::File,
                    tool: polis_events::ToolKind::Edit,
                    at,
                    weight: None,
                },
                1.0,
                Some(centre),
            );
        }
    };
    observe(&mut territory, home, established);
    for step in 0..4u32 {
        observe(
            &mut territory,
            recent,
            now.checked_sub(Duration::from_secs(u64::from(20 - step * 5)))
                .expect("within the last twenty seconds"),
        );
    }
    territory.decay(now);
    thread.territory = territory;
    snapshot.threads.push(thread);
    snapshot
}

fn draw_once(city: &City, snapshot: &WorldSnapshot) -> usize {
    let ctx = egui::Context::default();
    let layout = Arc::new(city.layout.clone());
    let base = BaseMap::render(&ctx, city, &layout, false);
    let viewport = Rect::from_min_size(Pos2::ZERO, Vec2::new(1200.0, 800.0));
    let mut camera = Camera::fit(base.edge(), base.geometry.median_building_px, viewport);
    let mut clouds = Clouds::default();
    let mut state = ViewState::default();

    let rect = Rect::from_min_size(Pos2::ZERO, viewport.size());
    let mut input = egui::RawInput {
        screen_rect: Some(rect),
        ..Default::default()
    };
    input
        .viewports
        .entry(input.viewport_id)
        .or_default()
        .inner_rect = Some(rect);

    let mut full = ctx.run_ui(input, |ui| {
        let rect = ui.max_rect();
        camera.set_viewport(rect);
        mapview::draw(
            ui,
            rect,
            &base,
            &mut camera,
            &mut clouds,
            snapshot,
            &mut state,
            polis_world::territory::CLOUD_CAP,
            1.0 / 60.0,
        );
    });
    let shapes = full.shapes.len();
    // `epaint` panics if a texture delta is dropped unapplied, and there is no
    // painter here to apply it to.
    full.textures_delta.clear();
    shapes
}

#[test]
fn a_migrating_territory_draws_a_leading_edge_and_a_settled_one_does_not() {
    let city = city();
    let paths: Vec<LogicalPath> = city.layout.buildings.keys().cloned().collect();
    assert!(paths.len() >= 40, "the fixture city needs buildings");

    // Two groups far enough apart in the layout to be a migration rather than a
    // wobble: the first eight buildings and the last eight.
    let home: Vec<LogicalPath> = paths.iter().take(8).cloned().collect();
    let away: Vec<LogicalPath> = paths.iter().rev().take(8).cloned().collect();

    let settled = drifting(&city, &home, &home);
    let migrating = drifting(&city, &home, &away);

    assert!(
        settled.threads[0].territory.drift_mark().is_none(),
        "a thread still working where it always has is not drifting"
    );
    let mark = migrating.threads[0]
        .territory
        .drift_mark()
        .expect("the last minute of work is somewhere else entirely");
    let moved = (mark.tip.x - mark.tail.x).hypot(mark.tip.y - mark.tail.y);
    assert!(
        moved > migrating.threads[0].territory.bandwidth(),
        "the leading edge is past the field's own radius: {moved}"
    );

    // The mark is three strokes — a spine and two chevron arms — over whatever
    // the settled frame already drew. Comparing the two frames rather than
    // counting absolute shapes keeps this from breaking every time another
    // layer gains a stroke.
    let with = draw_once(&city, &migrating);
    let without = draw_once(&city, &settled);
    assert!(
        with >= without + 3,
        "the drift mark reached no pixels: {with} shapes with it, {without} without"
    );
}
