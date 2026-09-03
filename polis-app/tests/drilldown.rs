//! The drill-down layer, drawn headlessly (PRD §12).
//!
//! > **Fuzzy above, exact below.** […] Hovering a building must give a definite
//! > list of which threads touched it and when.
//!
//! Every assertion here reads the **text that was actually painted**, walked out
//! of `egui`'s shape list, rather than a return value. That is deliberate: the
//! failures this layer can have are all of one shape — a panel that computes the
//! right answer and draws a different one, a row that is present but inside a
//! collapsed directory, a card that lists one of the two threads on a file — and
//! none of them are visible to a test of the computation alone.
//!
//! # What a `Context` with no renderer needs
//!
//! `FullOutput` carries the font-atlas and texture deltas, and `epaint` panics
//! if they are dropped unapplied. Every pass therefore ends in
//! `output.textures_delta.clear()`.

// The fixture is one long, ordered construction of a world, and the clock
// subtractions in it are against instants the test just created.
#![allow(clippy::too_many_lines, clippy::unchecked_time_subtraction)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Pos2, Rect, Vec2};
use polis_app::drill;
use polis_app::mapview::ViewState;
use polis_app::treeview::{TreeOut, TreeView};
use polis_app::ui::{self, Overlay};
use polis_events::{LogicalPath, SessionId, ThreadId, WorktreeId};
use polis_world::attention::{Attention, AttentionKind, DecisionSource};
use polis_world::contention::{Claim, Contention};
use polis_world::snapshot::WorldSnapshot;
use polis_world::{FileState, Thread, ThreadStatus, VisitStats};

const CONTESTED: &str = "src/auth/mod.rs";
const WAITING_AT: &str = "src/main.rs";
const OFF_MAP: &str = "scratch/new.rs";

fn lp(s: &str) -> LogicalPath {
    LogicalPath::new(s).expect("test path")
}

/// A layout carrying exactly the buildings this test talks about, cut from a
/// real generated city so the geometry is a city's and not a fixture's.
fn layout() -> polis_layout::CityLayout {
    let tree = polis_repo::synthetic::repository(60, 7);
    let mut layout =
        polis_layout::city::generate_with(&tree, &polis_layout::city::LayoutInputs::default())
            .layout;
    let template = layout
        .buildings
        .values()
        .next()
        .cloned()
        .expect("the synthetic city has buildings");
    layout.buildings.clear();
    for path in [CONTESTED, WAITING_AT, "docs/readme.md"] {
        layout.buildings.insert(
            lp(path),
            polis_layout::Building {
                path: lp(path),
                ..template.clone()
            },
        );
    }
    layout
}

/// Two agents on one file, one of them waiting on a human, and one file the map
/// cannot draw — the situation the whole product exists for, at its smallest.
fn snapshot() -> WorldSnapshot {
    let now = Instant::now();
    let mut snapshot = WorldSnapshot::empty(Arc::new(layout()));
    snapshot.at = now;

    let alpha_session = SessionId::new("s-alpha");
    let beta_session = SessionId::new("s-beta");
    let alpha = ThreadId::of_session(alpha_session.clone());
    let beta = ThreadId::of_session(beta_session.clone());

    let mut first = Thread::new(alpha.clone(), alpha_session, now - Duration::from_secs(600));
    first.title = Some("alpha".to_owned());
    first.status = ThreadStatus::Waiting;
    first.visits.insert(
        lp(CONTESTED),
        VisitStats {
            count: 6,
            writes: 4,
            first: now - Duration::from_secs(500),
            last: now - Duration::from_secs(12),
        },
    );
    first.trail.push_back((lp(CONTESTED), now));
    first.ops.push_back(op(
        &lp(CONTESTED),
        now - Duration::from_secs(12),
        polis_events::ToolKind::Edit,
        polis_events::Outcome::Failed,
    ));

    let mut second = Thread::new(beta.clone(), beta_session, now - Duration::from_secs(300));
    second.title = Some("beta".to_owned());
    second.status = ThreadStatus::Done;
    second.ops.push_back(op(
        &lp(CONTESTED),
        now - Duration::from_secs(7),
        polis_events::ToolKind::Write,
        polis_events::Outcome::Done,
    ));
    second.visits.insert(
        lp(CONTESTED),
        VisitStats {
            count: 2,
            writes: 2,
            first: now - Duration::from_secs(40),
            last: now - Duration::from_secs(7),
        },
    );
    second.visits.insert(
        lp(OFF_MAP),
        VisitStats {
            count: 1,
            writes: 1,
            first: now - Duration::from_secs(5),
            last: now - Duration::from_secs(5),
        },
    );
    second.trail.push_back((lp(OFF_MAP), now));

    let mut state = FileState {
        lines_added: 31,
        lines_removed: 4,
        diff_lines: 35,
        reads: 3,
        writes: 2,
        last_touched: Some(now - Duration::from_secs(7)),
        ..FileState::default()
    };
    state.touched_by.push(alpha.clone());
    state.touched_by.push(beta.clone());
    let mut files = BTreeMap::new();
    files.insert(lp(CONTESTED), state);
    files.insert(lp(OFF_MAP), FileState::default());

    // Overlapping line ranges on one branch: PRD §11.3's Critical tier, the one
    // where work is being clobbered now.
    let contention = Contention::of(
        Claim::write(alpha.clone(), lp(CONTESTED), now - Duration::from_secs(12))
            .in_checkout(WorktreeId::PRIMARY, Some("main"))
            .with_lines(10, 40),
        Claim::write(beta.clone(), lp(CONTESTED), now - Duration::from_secs(7))
            .in_checkout(WorktreeId::PRIMARY, Some("main"))
            .with_lines(20, 30),
    );
    // Older than PRD §11.4's ≤400 ms arrival pulse, so a fixture whose clock
    // never advances is not permanently mid-pulse.
    let arrived = now - Duration::from_secs(30);
    // In PRD §11.1's order, which is the order `polis-world` publishes and the
    // order the list must present without re-sorting.
    snapshot.attention = vec![
        Attention::new(AttentionKind::Contention(Box::new(contention)), arrived),
        Attention::new(
            AttentionKind::NeedsDecision {
                thread: alpha.clone(),
                at: Some(lp(WAITING_AT)),
                source: DecisionSource::PermissionRequest,
            },
            arrived,
        ),
        // Names no file, so the list falls back to beta's last step — which is
        // the off-map one, and the only reason that row is reachable at all.
        Attention::new(
            AttentionKind::Done {
                thread: beta.clone(),
                verified: false,
            },
            arrived,
        ),
    ];
    snapshot.threads = vec![first, second];
    snapshot.files = Arc::new(files);
    snapshot.generation = 1;
    snapshot
}

fn op(
    path: &LogicalPath,
    at: Instant,
    tool: polis_events::ToolKind,
    outcome: polis_events::Outcome,
) -> polis_world::Operation {
    polis_world::Operation {
        path: Some(path.clone()),
        placement: polis_world::place::OpPlacement::Path(path.clone()),
        glyph: tool.glyph(),
        tool,
        outcome,
        worker: None,
        at,
        tool_use: None,
    }
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

/// Runs one pass and returns every string that was actually painted.
fn painted(
    ctx: &egui::Context,
    input: egui::RawInput,
    body: impl FnMut(&mut egui::Ui),
) -> Vec<String> {
    let mut full = ctx.run_ui(input, body);
    let mut out = Vec::new();
    for clipped in &full.shapes {
        collect(&clipped.shape, &mut out);
    }
    full.textures_delta.clear();
    out
}

/// Two passes, returning what the second one painted.
fn twice(ctx: &egui::Context, size: Vec2, mut body: impl FnMut(&mut egui::Ui)) -> Vec<String> {
    let _ = painted(ctx, raw_input(size), &mut body);
    painted(ctx, raw_input(size), &mut body)
}

fn collect(shape: &egui::Shape, out: &mut Vec<String>) {
    match shape {
        egui::Shape::Text(text) => out.push(text.galley.text().to_owned()),
        egui::Shape::Vec(shapes) => {
            for shape in shapes {
                collect(shape, out);
            }
        }
        _ => {}
    }
}

fn says(painted: &[String], needle: &str) -> bool {
    painted.iter().any(|t| t.contains(needle))
}

/// PRD §12's third semantic-zoom tier, all four of the things it names:
/// *recent operations, which threads touched it, diff size, verification
/// status* — and, above them, the state PRD §11.1 ranks first.
#[test]
fn the_detail_panel_names_both_agents_the_diff_and_the_collision() {
    let ctx = egui::Context::default();
    let snapshot = snapshot();
    let text = painted(&ctx, raw_input(Vec2::new(420.0, 900.0)), |ui| {
        ui::building_panel(ui, &snapshot, &lp(CONTESTED));
    });

    assert!(says(&text, CONTESTED), "the path: {text:?}");
    assert!(
        says(&text, "CONTENTION"),
        "the worst state, first: {text:?}"
    );
    assert!(says(&text, "clobbering now"), "its tier: {text:?}");
    assert!(says(&text, "+31 −4"), "diff size: {text:?}");
    assert!(says(&text, "needs review"), "verification status: {text:?}");
    // "A definite list of which threads touched it": both of them, by name.
    assert!(says(&text, "alpha"), "{text:?}");
    assert!(says(&text, "beta"), "{text:?}");
    // And when, exactly, rather than "recently".
    assert!(says(&text, "last 12s ago"), "alpha's last touch: {text:?}");
    assert!(says(&text, "last 7s ago"), "beta's last touch: {text:?}");

    // Recent operations, merged across the two threads and in time order — the
    // defect that made a two-agent panel wrong: walking thread by thread put
    // alpha's older Edit above beta's newer Write.
    let ops: Vec<&String> = text
        .iter()
        .skip_while(|t| t.as_str() != "RECENT OPERATIONS")
        .collect();
    assert!(ops.len() >= 3, "the operations were not drawn: {text:?}");
    assert!(ops[1].contains("Write"), "newest first: {ops:?}");
    assert!(ops[2].contains("Edit"), "then the older one: {ops:?}");
    assert!(
        ops[2].contains("FAILED"),
        "and how it went, in words as well as colour: {ops:?}"
    );
}

/// The hover card is the same facts, shorter. It may drop rows; it may not
/// disagree with the panel about the rows it keeps.
#[test]
fn the_hover_card_and_the_panel_agree_about_who_touched_the_file() {
    let ctx = egui::Context::default();
    let snapshot = snapshot();
    // An `egui::Area` is laid out against the size it had last frame, so the
    // first pass registers it and the second is the one that paints. That is
    // also exactly what the window does: the card follows the pointer, and the
    // pointer was somewhere on the previous frame too.
    let card = twice(&ctx, Vec2::new(1200.0, 800.0), |ui| {
        ui::hover_card(ui.ctx(), Pos2::new(300.0, 300.0), &snapshot, &lp(CONTESTED));
    });
    assert!(says(&card, CONTESTED), "{card:?}");
    assert!(says(&card, "alpha") && says(&card, "beta"), "{card:?}");
    assert!(says(&card, "CONTENTION"), "{card:?}");
    assert!(says(&card, "+31 −4"), "{card:?}");
    assert!(
        says(&card, "click to open in your editor"),
        "PRD §12's one action, said: {card:?}"
    );

    // A file nothing has reported on says so rather than showing an empty card.
    let quiet = twice(&ctx, Vec2::new(1200.0, 800.0), |ui| {
        ui::hover_card(
            ui.ctx(),
            Pos2::new(300.0, 300.0),
            &snapshot,
            &lp("docs/readme.md"),
        );
    });
    assert!(says(&quiet, "no channel has reported"), "{quiet:?}");
}

/// PRD §1's primary decision, in the order PRD §11.1 fixes.
#[test]
fn the_attention_list_is_worst_first_and_says_where_each_one_is() {
    let ctx = egui::Context::default();
    let snapshot = snapshot();
    let text = painted(&ctx, raw_input(Vec2::new(420.0, 900.0)), |ui| {
        ui::attention_list(ui, &snapshot, None);
    });
    let index = |needle: &str| {
        text.iter()
            .position(|t| t.contains(needle))
            .unwrap_or_else(|| panic!("{needle:?} was never painted: {text:?}"))
    };
    assert!(
        index("CONTENTION") < index("WAITING ON YOU"),
        "contention is the only state where work is being destroyed: {text:?}"
    );
    assert!(
        index("WAITING ON YOU") < index("needs review"),
        "a pending decision costs wall clock; done costs nothing: {text:?}"
    );
    // Every row says which file, so a click has somewhere to go.
    assert!(says(&text, CONTESTED), "{text:?}");
    assert!(says(&text, WAITING_AT), "{text:?}");
    assert!(
        says(&text, OFF_MAP),
        "the `done` row falls back to the thread's last step: {text:?}"
    );
    assert!(says(&text, "(a) jumps to the next"), "{text:?}");
}

/// The `a` key is the whole product in one keystroke, and it has to walk the
/// queue rather than land on the same row for ever.
#[test]
fn the_attention_key_walks_the_list_worst_first_and_wraps() {
    let snapshot = snapshot();
    let mut overlay = Overlay::default();
    let first = overlay.next_attention(&snapshot).expect("a first jump");
    assert_eq!(first.path, Some(lp(CONTESTED)));
    assert_eq!(overlay.attention_cursor, Some(0));

    let second = overlay.next_attention(&snapshot).expect("a second");
    assert_eq!(second.path, Some(lp(WAITING_AT)));

    let third = overlay.next_attention(&snapshot).expect("a third");
    assert_eq!(third.path, Some(lp(OFF_MAP)));

    let wrapped = overlay.next_attention(&snapshot).expect("wrapping");
    assert_eq!(wrapped.path, first.path, "the queue is a cycle");

    // And a quiet map offers nothing rather than a jump to nowhere.
    let empty = WorldSnapshot::empty(Arc::new(layout()));
    assert!(overlay.next_attention(&empty).is_none());
    assert_eq!(overlay.attention_cursor, None);
}

/// A click has to land on the row it is over. The first version gave every row
/// a hit rectangle running back to the top of the rail — `ui.min_rect()` grows
/// with the panel — so a click anywhere in the list jumped to whichever row
/// registered last, which is a control that looks like it works.
#[test]
fn clicking_the_second_row_jumps_to_the_second_row() {
    let ctx = egui::Context::default();
    let snapshot = snapshot();
    let size = Vec2::new(420.0, 900.0);

    // Pass one lays the rows out; pass two puts the pointer on one and clicks.
    let mut jump = None;
    let mut full = ctx.run_ui(raw_input(size), |ui| {
        jump = ui::attention_list(ui, &snapshot, None);
    });
    let mut at = None;
    for clipped in &full.shapes {
        find_text(&clipped.shape, "WAITING ON YOU", &mut at);
    }
    full.textures_delta.clear();
    let at = at.expect("the second row was drawn");

    let mut input = raw_input(size);
    input.events.push(egui::Event::PointerMoved(at));
    input.events.push(egui::Event::PointerButton {
        pos: at,
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: egui::Modifiers::NONE,
    });
    input.events.push(egui::Event::PointerButton {
        pos: at,
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: egui::Modifiers::NONE,
    });
    let mut full = ctx.run_ui(input, |ui| {
        jump = ui::attention_list(ui, &snapshot, None);
    });
    full.textures_delta.clear();

    let jump = jump.expect("the click produced a jump");
    assert_eq!(
        jump.path,
        Some(lp(WAITING_AT)),
        "the row under the pointer, not the last row registered"
    );
}

/// Measured on the operator's own live session: the list opened with 33 rows,
/// every one of them the same contention, and the whole thread rail was pushed
/// off the bottom. PRD §17: *"does it change a decision? If not, cut it."*
/// Thirty-three of one kind is one decision, not thirty-three — but the other
/// kinds must survive the cut, which is the part a plain "first twelve rows"
/// would get wrong.
#[test]
fn a_flood_of_one_kind_never_buries_the_others() {
    let ctx = egui::Context::default();
    let mut snapshot = snapshot();
    let contention = snapshot.attention[0].clone();
    for _ in 0..32 {
        snapshot.attention.insert(0, contention.clone());
    }
    assert_eq!(snapshot.attention.len(), 35);

    let text = painted(&ctx, raw_input(Vec2::new(420.0, 1400.0)), |ui| {
        ui::attention_list(ui, &snapshot, None);
    });
    let rows = text.iter().filter(|t| t.as_str() == "CONTENTION").count();
    assert_eq!(rows, ui::ROWS_PER_KIND, "the flood is capped: {text:?}");
    assert!(
        says(
            &text,
            &format!("+{} more contention", 33 - ui::ROWS_PER_KIND)
        ),
        "and counted rather than dropped: {text:?}"
    );
    assert!(
        says(&text, "WAITING ON YOU"),
        "the primary state survives the flood: {text:?}"
    );
    assert!(says(&text, "needs review"), "and so does done: {text:?}");
}

/// PRD §12: *"a plain tree, **co-equal with the map, not a fallback**"* — which
/// means a click on a row does what a click on a building does.
#[test]
fn a_tree_row_click_selects_the_file_and_opens_it_exactly_as_the_map_does() {
    let ctx = egui::Context::default();
    let snapshot = snapshot();
    let mut tree = TreeView::default();
    let mut state = ViewState::default();
    let size = Vec2::new(900.0, 700.0);

    // Pass one lays the rows out; only then is a row's screen position known.
    // Pass two puts the pointer on one and clicks. That is the order the window
    // sees when the mouse enters.
    let mut out = TreeOut::default();
    let mut full = ctx.run_ui(raw_input(size), |ui| {
        out = tree.draw(ui, &snapshot, &mut state);
    });
    full.textures_delta.clear();
    assert_eq!(out.clicked, None, "nothing was clicked yet");

    // `docs` is a top-level directory and `sync` opens the first level, so its
    // one file has a row on the second pass.
    let target = row_of(&ctx, &mut tree, &snapshot, &mut state, size, "readme.md")
        .expect("the tree drew a row for docs/readme.md");
    let mut input = raw_input(size);
    input.events.push(egui::Event::PointerMoved(target));
    input.events.push(egui::Event::PointerButton {
        pos: target,
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: egui::Modifiers::NONE,
    });
    input.events.push(egui::Event::PointerButton {
        pos: target,
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: egui::Modifiers::NONE,
    });
    let mut full = ctx.run_ui(input, |ui| {
        out = tree.draw(ui, &snapshot, &mut state);
    });
    full.textures_delta.clear();

    assert_eq!(
        out.clicked,
        Some(lp("docs/readme.md")),
        "the row must ask for the editor, as a building does"
    );
    assert_eq!(
        state.selected,
        Some(lp("docs/readme.md")),
        "and the selection is the one both views share"
    );
    assert_eq!(out.centre_on, Some(lp("docs/readme.md")));
}

/// Finds the screen position of a row by its painted text.
fn row_of(
    ctx: &egui::Context,
    tree: &mut TreeView,
    snapshot: &WorldSnapshot,
    state: &mut ViewState,
    size: Vec2,
    name: &str,
) -> Option<Pos2> {
    let mut found = None;
    let mut full = ctx.run_ui(raw_input(size), |ui| {
        tree.draw(ui, snapshot, state);
    });
    for clipped in &full.shapes {
        find_text(&clipped.shape, name, &mut found);
    }
    full.textures_delta.clear();
    found
}

fn find_text(shape: &egui::Shape, name: &str, out: &mut Option<Pos2>) {
    match shape {
        egui::Shape::Text(text) if text.galley.text() == name => {
            *out = Some(text.pos + Vec2::new(4.0, 6.0));
        }
        egui::Shape::Vec(shapes) => {
            for shape in shapes {
                find_text(shape, name, out);
            }
        }
        _ => {}
    }
}

/// The roll-up, which is what makes the tree co-equal rather than co-located: a
/// mark on a file inside a shut directory has to be visible on the directory,
/// or the tree becomes the one view where contention can be entirely absent.
#[test]
fn contention_inside_a_shut_directory_is_still_visible_on_the_tree() {
    let ctx = egui::Context::default();
    let snapshot = snapshot();
    let mut tree = TreeView::default();
    let mut state = ViewState::default();
    let text = painted(&ctx, raw_input(Vec2::new(900.0, 700.0)), |ui| {
        tree.draw(ui, &snapshot, &mut state);
    });

    assert!(
        says(&text, "src"),
        "the top level is open after a sync: {text:?}"
    );
    assert!(
        !says(&text, "mod.rs"),
        "src/auth is shut, so the contended file has no row of its own: {text:?}"
    );
    assert!(
        says(&text, "><"),
        "and the directory above it carries the mark: {text:?}"
    );
    // The tree also has to be able to disagree with the map.
    assert!(says(&text, "off-map"), "{text:?}");
    // And it says what the map would be saying, so swapping loses nothing.
    assert!(says(&text, "3 attention, worst CONTENTION"), "{text:?}");
}

/// PRD §12's follow is a cycle with an off step, so an operator watching every
/// agent in a repository can visit all of them and then stop.
#[test]
fn follow_walks_every_thread_and_then_lets_go() {
    let snapshot = snapshot();
    let threads: Vec<ThreadId> = snapshot.threads.iter().map(|t| t.id.clone()).collect();
    assert_eq!(threads.len(), 2);

    let first = drill::next_follow(None, &threads, None).expect("the first thread");
    assert_eq!(first, threads[0]);
    let second = drill::next_follow(Some(&first), &threads, None).expect("the second");
    assert_eq!(second, threads[1]);
    assert_eq!(
        drill::next_follow(Some(&second), &threads, None),
        None,
        "off the end is off, not wrap-around"
    );

    // With a file in hand, the cycle starts at the agent working on that file.
    let prefer = drill::facts(&snapshot, &lp(CONTESTED))
        .touches
        .first()
        .map(|t| t.thread.clone())
        .expect("a toucher");
    assert_eq!(
        drill::next_follow(None, &threads, Some(&prefer)),
        Some(prefer.clone())
    );
    // And a thread that ended while it was being followed does not wedge it.
    let ghost = ThreadId::of_session(SessionId::new("gone"));
    assert_eq!(
        drill::next_follow(Some(&ghost), &threads, None),
        Some(threads[0].clone())
    );
}
