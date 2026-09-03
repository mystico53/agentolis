//! Exact below (PRD §12).
//!
//! > **Fuzzy above, exact below.** Soft cloud edges are honest for the ambient
//! > layer and useless when acting. Hovering a building must give a definite
//! > list of which threads touched it and when.
//!
//! The ambient layer — clouds, trails, the skyline — is deliberately soft. The
//! moment the operator points at something, softness stops. This module is the
//! one place that turns a [`WorldSnapshot`] into that definite answer, so the
//! three surfaces that give it — the hover card, the detail panel and the tree's
//! badges — are three renderings of **one** computation and cannot disagree
//! about who touched a file.
//!
//! # Why the merge matters
//!
//! The first version of the detail panel walked `snapshot.threads` and, for
//! each, its own `ops` deque. With one thread that is the right answer. With
//! two — which is the whole point of *"all agents active in a repository"* — it
//! is the wrong one twice over: the list came out grouped by thread rather than
//! ordered in time, and the twelve-row cap was spent on the first thread before
//! the second was reached, so the most recent operation on the file could be
//! absent from a list headed "recent operations". [`Facts::ops`] merges across
//! threads and sorts newest-first before anything is capped.
//!
//! # The attention list
//!
//! [`Ranked`] is the same idea applied to PRD §11: an ordered, clickable list of
//! every live attention state, which is the shortest path between PRD §1's
//! *"get to the thread that is waiting on a human"* and the operator's hands.
//! The order is `snapshot.attention`'s own — `polis-world` publishes it in
//! PRD §11.1 order and re-sorting it here would be a second opinion about
//! severity.

use std::time::Instant;

use eframe::egui::Pos2;
use polis_events::{LogicalPath, Outcome, ThreadId, ToolKind, WorkerId};
use polis_world::attention::{Attention, AttentionKind};
use polis_world::snapshot::WorldSnapshot;
use polis_world::{FileState, ThreadStatus};

use crate::camera::{Camera, BUILDING_TIER_BUILDING_PX};

/// How many operations a drill-down lists before it stops.
///
/// The panel is a list of the last few things that happened to one file, not a
/// log; past a dozen the operator is reading history rather than making a
/// decision (PRD §17).
pub const OPS_SHOWN: usize = 12;

/// One thread's relationship with one file: who, how often, and when.
///
/// The *"definite list of which threads touched it and when"* of PRD §12, one
/// row of it.
#[derive(Debug, Clone)]
pub struct Touch {
    /// Which thread.
    pub thread: ThreadId,
    /// What to call it — its title when it has one.
    pub label: String,
    /// Working / Waiting / Idle / Done, so the list says whether the toucher is
    /// still around.
    pub status: Option<ThreadStatus>,
    /// Total touches. The thrashing signal of PRD §12.
    pub visits: u32,
    /// Of which mutating.
    pub writes: u32,
    /// First touch, when the thread is still in the world.
    pub first: Option<Instant>,
    /// Most recent touch.
    pub last: Option<Instant>,
}

/// One operation on one file, with its thread already resolved.
#[derive(Debug, Clone)]
pub struct Op {
    /// When it happened.
    pub at: Instant,
    /// What to call the thread that ran it.
    pub thread: String,
    /// The worker inside it, or `None` for the main agent.
    pub worker: Option<WorkerId>,
    /// Which tool. The panel names it rather than drawing PRD §10.1's glyph:
    /// shape is the map's channel, and a word is what a list column can hold.
    pub tool: ToolKind,
    /// PRD §10.2's colour channel — printed as a word as well, because colour
    /// is never the sole channel for anything (PRD §11.4).
    pub outcome: Outcome,
}

/// Everything the window knows about one path, gathered once.
///
/// Borrows the snapshot: a frame draws the hover card and the panel from the
/// same `Arc<WorldSnapshot>` it is already holding, so nothing here is cloned
/// that was not already going to be formatted into a string.
#[derive(Debug)]
pub struct Facts<'a> {
    /// The path asked about.
    pub path: &'a LogicalPath,
    /// Its live state, when any channel has reported on it.
    pub state: Option<&'a FileState>,
    /// Whether the layout gave it a building. `false` is the map/tree
    /// disagreement PRD §12 wants the tree to be able to voice.
    pub on_map: bool,
    /// Whether the path is a **district** rather than a file.
    ///
    /// A shell command names its working directory, so a drill-down subject is
    /// sometimes a directory — and the first version of the panel called the
    /// repository root a file with no name and stamped it `off-map`, which is a
    /// panel that looks broken reporting something that is fine.
    pub district: bool,
    /// Who touched it and when, newest first.
    pub touches: Vec<Touch>,
    /// Recent operations, **merged across threads** and newest first.
    pub ops: Vec<Op>,
    /// How many operations there were before [`OPS_SHOWN`] truncated the list.
    pub ops_total: usize,
    /// Live attention marks that name this file — contention, or a decision
    /// waiting on a human at this building.
    pub marks: Vec<&'a Attention>,
}

impl Facts<'_> {
    /// Whether a test ran against this file after the last change to it.
    pub fn verified(&self) -> bool {
        self.state.is_some_and(FileState::is_verified)
    }

    /// Lines added and removed, as the channels reported them.
    pub fn diff(&self) -> (u32, u32) {
        self.state
            .map_or((0, 0), |f| (f.lines_added, f.lines_removed))
    }

    /// True when no channel has said anything about this file at all.
    pub fn untouched(&self) -> bool {
        self.state.is_none() && self.marks.is_empty()
    }
}

/// Gathers everything known about one path (PRD §12, *exact below*).
pub fn facts<'a>(snapshot: &'a WorldSnapshot, path: &'a LogicalPath) -> Facts<'a> {
    let state = snapshot.file(path);
    let on_map = snapshot.layout.buildings.contains_key(path);
    let district = !on_map && (path.is_root() || snapshot.layout.districts.contains_key(path));

    // Every thread that has ever visited the path, not only those the file
    // table remembers: `touched_by` is the file's own list and `visits` is the
    // thread's, and a read that never wrote leaves a visit without a touch.
    let mut touches: Vec<Touch> = Vec::new();
    for thread in &snapshot.threads {
        let visit = thread.visits.get(path);
        let listed = state.is_some_and(|f| f.touched_by.contains(&thread.id));
        if visit.is_none() && !listed {
            continue;
        }
        touches.push(Touch {
            thread: thread.id.clone(),
            label: crate::mapview::thread_label(thread),
            status: Some(thread.status),
            visits: visit.map_or(0, |v| v.count),
            writes: visit.map_or(0, |v| v.writes),
            first: visit.map(|v| v.first),
            last: visit.map(|v| v.last),
        });
    }
    // A thread that has ended is gone from `snapshot.threads` while its name is
    // still on the file. Saying nothing there would be the map quietly losing
    // the one fact the drill-down exists to supply, so the id is listed with
    // what is left of it.
    if let Some(file) = state {
        for id in &file.touched_by {
            if touches.iter().any(|t| t.thread == *id) {
                continue;
            }
            touches.push(Touch {
                thread: id.clone(),
                label: short_id(id.as_str()),
                status: None,
                visits: 0,
                writes: 0,
                first: None,
                last: None,
            });
        }
    }
    touches.sort_by(|a, b| b.last.cmp(&a.last).then_with(|| a.label.cmp(&b.label)));

    let mut ops: Vec<Op> = Vec::new();
    for thread in &snapshot.threads {
        let label = crate::mapview::thread_label(thread);
        for op in &thread.ops {
            if op.path.as_ref() != Some(path) {
                continue;
            }
            ops.push(Op {
                at: op.at,
                thread: label.clone(),
                worker: op.worker.clone(),
                tool: op.tool.clone(),
                outcome: op.outcome,
            });
        }
    }
    ops.sort_by_key(|op| std::cmp::Reverse(op.at));
    let ops_total = ops.len();
    ops.truncate(OPS_SHOWN);

    let marks: Vec<&Attention> = snapshot
        .attention
        .iter()
        .filter(|mark| match &mark.kind {
            AttentionKind::NeedsDecision { at, .. } => at.as_ref() == Some(path),
            AttentionKind::Contention(c) => c.path() == path,
            AttentionKind::Done { .. } => false,
        })
        .collect();

    Facts {
        path,
        state,
        on_map,
        district,
        touches,
        ops,
        ops_total,
        marks,
    }
}

/// What to print for a path, including the two cases that have no name of their
/// own: the repository root, and a directory.
pub fn label_of(path: &LogicalPath) -> String {
    if path.is_root() {
        "· the repository root".to_owned()
    } else {
        path.as_str().to_owned()
    }
}

/// What the window was asked to go and look at.
///
/// Produced by a click on an attention row, by the `a` key, and by the detail
/// panel; applied once per frame by [`crate::app`], which is the only place that
/// holds the camera, the tree and the selection at the same time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Jump {
    /// The file to select and reveal, when the subject is one.
    pub path: Option<LogicalPath>,
    /// The thread the subject belongs to, so the camera has somewhere to cut to
    /// even when no file is named.
    pub thread: ThreadId,
}

/// One row of the attention list: a mark, ranked, with somewhere to jump to.
#[derive(Debug, Clone)]
pub struct Ranked<'a> {
    /// The mark itself. Its `kind` carries everything the row prints.
    pub mark: &'a Attention,
    /// The file the row is about: the one the mark names, else the last file
    /// the thread touched.
    ///
    /// The fallback is what makes the list usable at all: the most common
    /// source of a *needs decision* on real sessions is `TurnEnded`, which names
    /// no file, and a row with nowhere to go is a row the operator cannot act
    /// on.
    pub at: Option<LogicalPath>,
    /// What to call the thread.
    pub thread: String,
    /// How long the state has been up.
    pub waiting: std::time::Duration,
}

/// The live attention states, in the order `polis-world` published them —
/// PRD §11.1's `contention > needs-decision > done`.
///
/// > **Primary decision it accelerates:** *unblock* — get to the thread that is
/// > waiting on a human. (PRD §1)
pub fn ranked(snapshot: &WorldSnapshot) -> Vec<Ranked<'_>> {
    snapshot
        .attention
        .iter()
        .map(|mark| Ranked {
            mark,
            at: subject(snapshot, mark),
            thread: snapshot.thread(mark.thread()).map_or_else(
                || short_id(mark.thread().as_str()),
                crate::mapview::thread_label,
            ),
            waiting: snapshot.at.saturating_duration_since(mark.since),
        })
        .collect()
}

/// The file a mark is about: the one it names, else the last file its thread
/// touched.
///
/// The map answers the same question in geometry and falls back to the
/// territory's centre of mass, which has no row in a filesystem tree. This is
/// the tree-native answer, and it is also what the attention list selects when
/// the operator clicks a row.
pub fn subject(snapshot: &WorldSnapshot, mark: &Attention) -> Option<LogicalPath> {
    let named = match &mark.kind {
        AttentionKind::NeedsDecision { at, .. } => at.clone(),
        AttentionKind::Contention(c) => Some(c.path().clone()),
        AttentionKind::Done { .. } => None,
    };
    if let Some(path) = named {
        if !path.is_root() {
            return Some(path);
        }
    }
    let thread = snapshot.thread(mark.thread())?;
    // The last step with a **building**, not simply the last step. A shell call
    // is placed on its working directory, so the newest step on a busy thread is
    // very often the repository root — and a row that points at the root points
    // at everything, which is the same as pointing at nothing. Measured on the
    // operator's own live session: the top attention row landed on the root and
    // the panel came out titled `FILE` with no name under it.
    let steps = thread.trail.iter().rev().map(|(path, _)| path);
    steps
        .clone()
        .find(|path| snapshot.layout.buildings.contains_key(*path))
        .or_else(|| steps.clone().find(|path| !path.is_root()))
        .cloned()
}

/// A short, stable word for a mark, so colour is never the only channel
/// (PRD §11.4).
pub fn word(kind: &AttentionKind) -> &'static str {
    match kind {
        AttentionKind::Contention(_) => "CONTENTION",
        AttentionKind::NeedsDecision { .. } => "WAITING ON YOU",
        AttentionKind::Done {
            verified: false, ..
        } => "needs review",
        AttentionKind::Done { verified: true, .. } => "done",
    }
}

/// The ink a mark owns in PRD §10.3's top band.
pub fn ink(kind: &AttentionKind) -> crate::palette::Ink {
    match kind {
        AttentionKind::Contention(_) => crate::palette::contention(),
        AttentionKind::NeedsDecision { .. } => crate::palette::needs_decision(),
        AttentionKind::Done { verified: true, .. } => crate::palette::done_verified(),
        AttentionKind::Done {
            verified: false, ..
        } => crate::palette::done_unverified(),
    }
}

/// A short mark for a tree row, so the tree shows the same three states the map
/// does without borrowing the map's geometry.
///
/// PRD §12 asks the two views to be co-equal, and a tree that cannot show
/// contention is a tree the operator has to leave in order to act.
///
/// **ASCII on purpose.** `egui`'s default face has no glyph for most of the
/// dingbats an interface reaches for first, and a missing glyph paints a tofu
/// box — which in a column of attention marks reads as a fourth state. Measured
/// on the live window: `◂`, `◆` and `✓` all came out as boxes.
pub fn tree_glyph(kind: &AttentionKind) -> &'static str {
    match kind {
        AttentionKind::Contention(_) => "><",
        AttentionKind::NeedsDecision { .. } => "!",
        AttentionKind::Done { verified: true, .. } => "ok",
        AttentionKind::Done {
            verified: false, ..
        } => "?",
    }
}

/// PRD §11.1's order as a number, worst first, for the tree's roll-up.
///
/// The list itself is never re-sorted with this — `polis-world` already
/// publishes in this order — but a directory carrying three marks has to show
/// the worst one, and that needs a comparison.
pub fn rank(kind: &AttentionKind) -> u8 {
    match kind {
        AttentionKind::Contention(_) => 0,
        AttentionKind::NeedsDecision { .. } => 1,
        AttentionKind::Done {
            verified: false, ..
        } => 2,
        AttentionKind::Done { verified: true, .. } => 3,
    }
}

/// The next thread to bind the camera to (PRD §12, *follow a thread*).
///
/// A cycle with an explicit off step: nothing → the first thread → … → the last
/// → nothing. An operator watching *"all agents active in a repository"* is
/// visiting a queue, and a key that always jumped to the same thread would only
/// ever show them one of them.
///
/// `prefer` is where the cycle starts when nothing is bound yet — the thread
/// that most recently touched whatever is selected — so pressing follow with a
/// file in hand goes to the agent working on that file rather than to thread
/// number one.
pub fn next_follow(
    current: Option<&ThreadId>,
    threads: &[ThreadId],
    prefer: Option<&ThreadId>,
) -> Option<ThreadId> {
    if threads.is_empty() {
        return None;
    }
    match current {
        None => prefer
            .filter(|id| threads.contains(id))
            .cloned()
            .or_else(|| threads.first().cloned()),
        Some(id) => match threads.iter().position(|t| t == id) {
            // Off the end of the list is *off*, not wrap-around: the operator
            // must be able to stop following without hunting for another key.
            Some(i) => threads.get(i + 1).cloned(),
            // The followed thread has ended. Start again rather than stick.
            None => threads.first().cloned(),
        },
    }
}

/// Cuts the camera to a point and zooms in far enough that PRD §12's *Building*
/// representation is the one drawn.
///
/// > **Follow a thread**: binds the camera to a thread. **Cut, do not pan.**
///
/// A jump that lands at the City tier answers "where" and not "what", which is
/// half a drill-down. The cut comes first so the zoom, which is about the
/// viewport centre, keeps the subject under it.
pub fn drill_to(camera: &mut Camera, at: Pos2) {
    camera.cut_to(at);
    let px = camera.building_screen_px();
    if px < BUILDING_TIER_BUILDING_PX && px > 0.0 {
        camera.set_zoom(camera.zoom() * (BUILDING_TIER_BUILDING_PX / px));
        camera.cut_to(at);
    }
}

/// The first eight characters of an id — enough to tell two sessions apart and
/// short enough to sit in a column.
pub fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::time::Duration;

    use polis_events::SessionId;
    use polis_world::{Operation, Thread, VisitStats};

    fn lp(s: &str) -> LogicalPath {
        LogicalPath::new(s).expect("test path")
    }

    fn thread_of(name: &str, at: Instant) -> Thread {
        let session = SessionId::new(name);
        let mut thread = Thread::new(ThreadId::of_session(session.clone()), session, at);
        thread.title = Some(name.to_owned());
        thread
    }

    fn op(path: &LogicalPath, at: Instant, tool: ToolKind) -> Operation {
        Operation {
            path: Some(path.clone()),
            placement: polis_world::place::OpPlacement::Path(path.clone()),
            glyph: tool.glyph(),
            tool,
            outcome: Outcome::Done,
            worker: None,
            at,
            tool_use: None,
        }
    }

    /// The defect this module exists to remove: with two threads on one file,
    /// walking thread by thread returns the list grouped by thread, and the cap
    /// is spent before the second thread is reached. A list headed "recent
    /// operations" must be in time order and must contain the most recent one.
    #[test]
    fn operations_are_merged_across_threads_and_newest_first() {
        let t0 = Instant::now();
        let path = lp("src/a.rs");
        let mut snapshot = WorldSnapshot::empty(Arc::new(polis_layout::CityLayout::default()));
        snapshot.at = t0 + Duration::from_secs(100);

        let mut alpha = thread_of("alpha", t0);
        // Thread alpha alone would fill the whole cap.
        for i in 0..(OPS_SHOWN + 4) {
            alpha.ops.push_back(op(
                &path,
                t0 + Duration::from_secs(i as u64),
                ToolKind::Read,
            ));
        }
        let mut beta = thread_of("beta", t0);
        beta.ops
            .push_back(op(&path, t0 + Duration::from_secs(90), ToolKind::Write));
        snapshot.threads = vec![alpha, beta];

        let facts = facts(&snapshot, &path);
        assert_eq!(facts.ops.len(), OPS_SHOWN, "the list is capped");
        assert_eq!(facts.ops_total, OPS_SHOWN + 5, "and says how much it cut");
        assert_eq!(
            facts.ops[0].thread, "beta",
            "the newest operation is beta's, whatever order the threads are in"
        );
        assert!(
            facts.ops.windows(2).all(|w| w[0].at >= w[1].at),
            "newest first"
        );
    }

    /// PRD §12: *"a definite list of which threads touched it and when"* —
    /// definite means every toucher, including one whose thread has since gone.
    #[test]
    fn every_toucher_is_listed_even_when_its_thread_has_ended() {
        let t0 = Instant::now();
        let path = lp("src/a.rs");
        let mut snapshot = WorldSnapshot::empty(Arc::new(polis_layout::CityLayout::default()));
        snapshot.at = t0 + Duration::from_secs(10);

        let mut alpha = thread_of("alpha", t0);
        alpha.visits.insert(
            path.clone(),
            VisitStats {
                count: 6,
                writes: 2,
                first: t0,
                last: t0 + Duration::from_secs(5),
            },
        );
        let gone = ThreadId::of_session(SessionId::new("ghost-session"));
        let mut state = FileState {
            last_touched: Some(t0),
            ..FileState::default()
        };
        state.touched_by.push(alpha.id.clone());
        state.touched_by.push(gone.clone());
        let mut files = BTreeMap::new();
        files.insert(path.clone(), state);
        snapshot.files = Arc::new(files);
        snapshot.threads = vec![alpha];

        let facts = facts(&snapshot, &path);
        assert_eq!(facts.touches.len(), 2, "{:?}", facts.touches);
        assert_eq!(facts.touches[0].label, "alpha");
        assert_eq!(facts.touches[0].visits, 6);
        assert!(
            facts.touches.iter().any(|t| t.thread == gone),
            "the ended thread is still named on the file"
        );
        assert!(facts.touches[1].status.is_none(), "and says it is gone");
    }

    /// A mark that names no file still has to be reachable, because the most
    /// common real one — a main agent that ended its turn — never names one.
    #[test]
    fn a_mark_that_names_no_file_falls_back_to_the_threads_last_step() {
        let t0 = Instant::now();
        let mut snapshot = WorldSnapshot::empty(Arc::new(polis_layout::CityLayout::default()));
        snapshot.at = t0 + Duration::from_secs(30);
        let mut alpha = thread_of("alpha", t0);
        alpha.trail.push_back((lp("src/first.rs"), t0));
        alpha.trail.push_back((lp("src/last.rs"), t0));
        let id = alpha.id.clone();
        snapshot.threads = vec![alpha];
        snapshot.attention = vec![Attention::new(
            AttentionKind::NeedsDecision {
                thread: id,
                at: None,
                source: polis_world::attention::DecisionSource::TurnEnded,
            },
            t0,
        )];

        let rows = ranked(&snapshot);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].at, Some(lp("src/last.rs")));
        assert_eq!(rows[0].thread, "alpha");
        assert!(rows[0].waiting >= Duration::from_secs(30));
    }

    /// Seen on the operator's live session: the top attention row pointed at
    /// the repository root, because the thread's newest step was a shell command
    /// placed on its working directory. A row that points at everything points
    /// at nothing, and the panel it opened was titled `FILE` with no name under
    /// it and an `off-map` badge on the repository itself.
    #[test]
    fn a_mark_falls_back_past_the_repository_root_to_a_real_building() {
        let t0 = Instant::now();
        let file = lp("polis-app/src/ui.rs");
        let mut layout = polis_layout::CityLayout::default();
        let tree = polis_repo::synthetic::repository(40, 3);
        let generated =
            polis_layout::city::generate_with(&tree, &polis_layout::city::LayoutInputs::default());
        let template = generated
            .layout
            .buildings
            .values()
            .next()
            .cloned()
            .expect("a building");
        layout.buildings.insert(
            file.clone(),
            polis_layout::Building {
                path: file.clone(),
                ..template
            },
        );
        let mut snapshot = WorldSnapshot::empty(std::sync::Arc::new(layout));
        snapshot.at = t0 + std::time::Duration::from_secs(5);

        let mut thread = thread_of("alpha", t0);
        thread.trail.push_back((file.clone(), t0));
        thread.trail.push_back((LogicalPath::root(), t0));
        let id = thread.id.clone();
        snapshot.threads = vec![thread];
        snapshot.attention = vec![Attention::new(
            AttentionKind::Done {
                thread: id,
                verified: false,
            },
            t0,
        )];

        assert_eq!(
            ranked(&snapshot)[0].at,
            Some(file),
            "the newest step is the root; the newest *building* is the answer"
        );
        assert_eq!(label_of(&LogicalPath::root()), "· the repository root");
    }

    /// The root and a directory are drawn as such rather than as a nameless
    /// file the map has lost.
    #[test]
    fn a_district_subject_is_not_reported_as_off_the_map() {
        let tree = polis_repo::synthetic::repository(40, 3);
        let layout =
            polis_layout::city::generate_with(&tree, &polis_layout::city::LayoutInputs::default())
                .layout;
        let district = layout
            .districts
            .keys()
            .find(|path| !path.is_root())
            .cloned()
            .expect("the synthetic city has districts");
        let snapshot = WorldSnapshot::empty(std::sync::Arc::new(layout));

        let here = facts(&snapshot, &district);
        assert!(!here.on_map, "a district has no building");
        assert!(here.district, "but it is a district, not a lost file");
        assert!(facts(&snapshot, &LogicalPath::root()).district);

        let nowhere = lp("scratch/nowhere.rs");
        let lost = facts(&snapshot, &nowhere);
        assert!(!lost.on_map && !lost.district, "this one really is off-map");
    }

    /// PRD §11.1 is an ordering and the tree's roll-up depends on it.
    #[test]
    fn contention_outranks_a_decision_which_outranks_done() {
        let thread = ThreadId::of_session(SessionId::new("s"));
        let decision = AttentionKind::NeedsDecision {
            thread: thread.clone(),
            at: None,
            source: polis_world::attention::DecisionSource::TurnEnded,
        };
        let unverified = AttentionKind::Done {
            thread: thread.clone(),
            verified: false,
        };
        let verified = AttentionKind::Done {
            thread,
            verified: true,
        };
        assert!(rank(&decision) < rank(&unverified));
        assert!(rank(&unverified) < rank(&verified));
        assert_ne!(word(&decision), word(&unverified));
        assert_ne!(tree_glyph(&decision), tree_glyph(&unverified));
    }
}
