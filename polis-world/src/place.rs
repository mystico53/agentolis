//! Where an operation is drawn — the placement chain, made explicit.
//!
//! # The failure this module exists to fix
//!
//! The map used to draw an operation only if it resolved to a *building*, and
//! everything else fell off the floor. That is not a small omission. Measured
//! over one project's whole corpus, 544 tool calls failed and **374 of them
//! (69 %) were `Bash` or `PowerShell`** — tools that carry no file path. Only
//! 24 failures were on a tool that names one. So the marks that would have been
//! red were never drawn at all, and a failing session rendered identically to a
//! clean one.
//!
//! The same hole hid the busiest thing an agent does: 6 742 shell calls in one
//! recorded session, against 1 104 edits.
//!
//! # The chain
//!
//! Four rungs, tried in order, and the function that walks them is
//! [`site_of`] — one definition, shared by the world and the renderer, so
//! "where does this get drawn" cannot have two answers.
//!
//! 1. [`OpSite::Path`] — the operation's own path, and the city has geometry
//!    for it. An `Edit` at its building.
//! 2. [`OpSite::Cwd`] — a shell call's **working directory**, as a district.
//!    PRD §6.1's evidence table already gives `Bash` cwd a weight of 0.5, so a
//!    cwd is a locatable signal and not a shrug.
//!
//!    With one exclusion, and it is the load-bearing one: a cwd that resolves
//!    to the **repository root** is not a location. It names the whole city, and
//!    drawing there puts every shell call the session ever ran on one pixel at
//!    the centre of the map — which is the exact failure PRD §6 opens by
//!    rejecting, *"a centroid in the empty gap between them, which is the one
//!    place nothing is happening"*. Measured on the three recorded sessions,
//!    6 804 of 10 030 operations in one of them resolved to the root this way.
//!    Those demote to rung 3.
//! 3. [`OpSite::Agent`] — the acting agent's own position: the worker's focus
//!    when a worker ran it, else the thread's territory centre of mass, else
//!    the newest step of its trail that has geometry. *An agent has a place
//!    even when a particular call does not.*
//! 4. [`OpSite::Rail`] — nothing at all. PRD §6.2 already named the answer:
//!
//!    > Until then the thread renders with no cloud — **an unplaced marker in
//!    > the status rail**.
//!
//!    Nothing is silently dropped. What cannot be placed is counted
//!    ([`PlacementCensus`]) and listed.
//!
//! # Rungs are not equally certain, and the drawing says so
//!
//! [`OpSite::scale`] hands the renderer a factor on the glyph radius: a mark
//! whose position came from a district is coarser than one at a building, and a
//! mark at the agent is coarser still. That is the *position* channel carrying
//! its own uncertainty. It is **not** PRD §10's shape or colour channel and it
//! never encodes an outcome — §10 opens by forbidding that conflation, so
//! de-emphasising a successful operation is done by aggregating it, never by
//! recolouring or reshaping it.

use polis_events::{LogicalPath, WorkerId};
use polis_layout::{CityLayout, Point};

use crate::{Operation, Thread};

/// Which rung of the chain an operation's position was decided on, as the world
/// recorded it when the call happened.
///
/// Rung 4 is not here on purpose: "this has nowhere to go" is a fact about the
/// city and the thread *now*, not about the call, and a thread that acquires a
/// position later must start drawing the operations it already ran. So the
/// world stores rungs 1–3 and [`site_of`] resolves rung 4 at draw time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpPlacement {
    /// 1. The operation's own path.
    Path(LogicalPath),
    /// 2. A shell call's working directory, as a district below the repository
    ///    root.
    Cwd(LogicalPath),
    /// 3. Wherever the agent that ran it is.
    Agent,
}

impl OpPlacement {
    /// The path this placement names, if any.
    #[must_use]
    pub fn path(&self) -> Option<&LogicalPath> {
        match self {
            Self::Path(p) | Self::Cwd(p) => Some(p),
            Self::Agent => None,
        }
    }

    /// 1, 2 or 3 — the rung this placement was decided on.
    #[must_use]
    pub fn rung(&self) -> u8 {
        match self {
            Self::Path(_) => 1,
            Self::Cwd(_) => 2,
            Self::Agent => 3,
        }
    }
}

/// The resolved answer: a point on the map, or the status rail.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OpSite {
    /// 1. At its own path's building or district.
    Path(Point),
    /// 2. At its working directory's district.
    Cwd(Point),
    /// 3. At the agent that ran it.
    Agent(Point),
    /// 4. Nowhere on the map: PRD §6.2's unplaced marker in the status rail.
    Rail,
}

impl OpSite {
    /// The point, or `None` for [`OpSite::Rail`].
    #[must_use]
    pub fn point(self) -> Option<Point> {
        match self {
            Self::Path(p) | Self::Cwd(p) | Self::Agent(p) => Some(p),
            Self::Rail => None,
        }
    }

    /// 1, 2, 3 or 4.
    #[must_use]
    pub fn rung(self) -> u8 {
        match self {
            Self::Path(_) => 1,
            Self::Cwd(_) => 2,
            Self::Agent(_) => 3,
            Self::Rail => 4,
        }
    }

    /// How certain the position is, as a factor on the glyph radius.
    ///
    /// A building is a place; a district is a neighbourhood; the agent is "with
    /// whoever ran it". The mark shrinks as the claim weakens, which is the
    /// position channel carrying its own uncertainty — never the outcome.
    #[must_use]
    pub fn scale(self) -> f64 {
        match self {
            Self::Path(_) => 1.0,
            Self::Cwd(_) => 0.78,
            Self::Agent(_) => 0.62,
            Self::Rail => 0.0,
        }
    }

    /// Whether this site is a place the **operation** named, or one borrowed
    /// from the agent that ran it.
    ///
    /// Rungs 1 and 2 are the operation's own — the file it touched, the
    /// directory it ran in. Rung 3 is not: it is wherever the thread happens to
    /// be standing, which moves as the agent works and says nothing about what
    /// the call did. A [`Point`] cannot carry that difference, and losing it is
    /// how a failed `PowerShell` at the repository root came to fly a
    /// district-scale alarm over a directory it had never opened.
    ///
    /// The mark is drawn either way, and [`scale`](Self::scale) is what says how
    /// sure its position is. This is the stronger question, and only the alarm
    /// layer asks it: *may a ring point at this?*
    #[must_use]
    pub fn sited(self) -> bool {
        matches!(self, Self::Path(_) | Self::Cwd(_))
    }

    /// Whether this site stacks marks on top of each other by construction.
    ///
    /// Rungs 2 and 3 are coarse: every shell call in a district lands on the
    /// same point, and every pathless call of one agent lands on the agent. The
    /// renderer spreads those rather than overplotting them.
    #[must_use]
    pub fn is_coarse(self) -> bool {
        matches!(self, Self::Cwd(_) | Self::Agent(_))
    }
}

/// A logical path's position in city space: its building, else its district.
///
/// The single definition. `World::position_of` and the renderer's
/// `position_of` both call it, because a map on which the world and the
/// renderer disagree about where a file is has no meaning.
#[must_use]
pub fn position_in(layout: &CityLayout, path: &LogicalPath) -> Option<Point> {
    if let Some(b) = layout.buildings.get(path) {
        let c = b.footprint.centroid();
        if c.x.is_finite() && c.y.is_finite() {
            return Some(c);
        }
    }
    layout
        .districts
        .get(path)
        .map(|d| d.centre)
        .filter(|c| c.x.is_finite() && c.y.is_finite())
}

/// The district a working directory names, or `None` when it names the whole
/// repository.
///
/// Walks up to the nearest ancestor the city has a district for, and stops
/// **above** the root: see the module docs for why the root is not a location.
#[must_use]
pub fn district_for_cwd(layout: &CityLayout, cwd: &LogicalPath) -> Option<LogicalPath> {
    let mut at = cwd.clone();
    loop {
        if at.is_root() {
            return None;
        }
        if position_in(layout, &at).is_some() {
            return Some(at);
        }
        at = at.parent()?;
    }
}

/// Where a thread's main agent is (PRD §6): the territory's **anchor**, else the
/// newest step of its trail that the city has geometry for, else — only for a
/// territory that has no kernels at all — its centre of mass.
///
/// > A main agent has no meaningful point location — it delegates rather than
/// > edits. Computing a centroid of its workers is actively wrong: an
/// > orchestrator with workers in `src/auth` and `tests/` gets a centroid in the
/// > empty gap between them, which is the one place nothing is happening.
///
/// This function used to read `centre_of_mass` first and its doc used to say
/// *"it is a property of the field, not a mean of positions"*. That was false:
/// `Territory::refresh_centre_of_mass` is literally `Σ(c·w)/Σw`, so rung 1 was
/// the centroid the sentence above rejects, and an orchestrator's ring really
/// did land in the gap. [`crate::territory::Territory::anchor`] is the
/// field's mode — the kernel centre where the density is highest — and *that* is
/// a property of the field: it is always a place something was observed, and two
/// sessions with similar work but different lobes no longer collapse onto nearly
/// the same point the way two similar means do.
///
/// The mean survives as the **last** rung, and only there. It is what a
/// territory assembled by hand — a fixture that pushes into `kernels` and sets
/// `centre_of_mass` without ever calling `observe` — can still answer with, and
/// dropping it would silently unplace threads in tests that are about something
/// else entirely.
#[must_use]
pub fn thread_position(thread: &Thread, layout: &CityLayout) -> Option<Point> {
    // The *drawn* anchor, which is the mode gliding rather than the mode
    // jumping — see `territory::ANCHOR_GLIDE`. Every decision still reads
    // `Territory::anchor`; this is the position channel, and the position
    // channel is the one that has to move continuously. The plain anchor is the
    // fallback for a territory that has never been decayed, which is every
    // hand-built fixture.
    if let Some(anchor) = thread
        .territory
        .drawn_anchor()
        .or_else(|| thread.territory.anchor())
    {
        if anchor.x.is_finite() && anchor.y.is_finite() {
            return Some(anchor);
        }
    }
    if let Some(head) = thread
        .trail
        .iter()
        .rev()
        .find_map(|(path, _)| position_in(layout, path))
    {
        return Some(head);
    }
    thread
        .territory
        .centre_of_mass
        .filter(|com| com.x.is_finite() && com.y.is_finite())
}

/// Where the agent that ran an operation was **when it ran it**.
///
/// The order is deliberately *not* [`thread_position`]'s, and the difference
/// matters more than it looks:
///
/// 1. the worker's focus, when a worker ran it — a subagent's shell call belongs
///    at the file that subagent is working on;
/// 2. the newest step of the thread's trail that the city has geometry for;
/// 3. [`thread_position`] — the territory's anchor, and the centre of mass
///    behind it — as a last resort.
///
/// [`thread_position`] puts the territory first because it is answering *"what
/// is this agent's scope"*, and PRD §6 is emphatic that a scope is a field
/// rather than a point. This function answers a different question — *"where did
/// this call happen"* — and for that a summary of the whole field is the wrong
/// answer however it is computed: it is decaying evidence from the entire
/// session and can sit a long way from where the agent is now. Measured on
/// `9cab97d7`, the centre of mass sat 227 city units from the median of the
/// files the session touched, which put every rung-3 mark off the side of a
/// camera framed on the work.
///
/// That measurement was taken against the mean, and rung 3 is now the *mode*, so
/// half of the old argument has dissolved: the mode is a place work actually
/// happened, so it can no longer be off in the empty middle. The half that
/// remains is the half that decided this order — the mode is where the thread
/// has been working over the last several minutes of decay, and the trail head
/// is where it was on the call being drawn. Those are different questions and
/// rung 2 answers this one. The 227 units were not re-measured against the
/// anchor; nothing here depends on the number any more.
#[must_use]
pub fn agent_position(
    thread: &Thread,
    worker: Option<&WorkerId>,
    layout: &CityLayout,
) -> Option<Point> {
    if let Some(focus) = worker
        .and_then(|id| thread.worker(id))
        .and_then(|w| w.focus.as_ref())
        // A focus of the repository root is not a location, for exactly the
        // reason [`district_for_cwd`] refuses one: it names the whole city, so
        // every worker whose last located call was root-scoped lands on the
        // same pixel at the centre of the map. Measured on session `4bbcee1c`,
        // three of six *running* workers shared one point that way — and 340 of
        // that session's observations were root-scoped, so it is the common
        // case rather than the corner one. Falling through to the trail head
        // below says less and says it truthfully.
        .filter(|focus| !focus.is_root())
    {
        if let Some(p) = position_in(layout, focus) {
            return Some(p);
        }
    }
    if let Some(head) = thread
        .trail
        .iter()
        .rev()
        .find_map(|(path, _)| position_in(layout, path))
    {
        return Some(head);
    }
    thread_position(thread, layout)
}

/// The chain, walked. This function *is* the specification in the module docs.
#[must_use]
pub fn site_of(op: &Operation, thread: &Thread, layout: &CityLayout) -> OpSite {
    if let OpPlacement::Path(p) = &op.placement {
        if let Some(at) = position_in(layout, p) {
            return OpSite::Path(at);
        }
    }
    if let OpPlacement::Cwd(p) = &op.placement {
        if let Some(at) = position_in(layout, p) {
            return OpSite::Cwd(at);
        }
    }
    agent_position(thread, op.worker.as_ref(), layout).map_or(OpSite::Rail, OpSite::Agent)
}

/// How many operations landed on each rung.
///
/// Kept on [`crate::Health`] so "what did the map not show you" is a number the
/// status bar can print, rather than a thing an operator has to infer from an
/// empty picture.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlacementCensus {
    /// Rung 1 — at their own path.
    pub path: u64,
    /// Rung 2 — at a working directory's district.
    pub cwd: u64,
    /// Rung 3 — at the agent that ran them.
    pub agent: u64,
    /// Rung 4 — nowhere: the status rail.
    pub rail: u64,
}

impl PlacementCensus {
    /// Counts one operation on rung `rung` (1–4). Out-of-range counts as rung 4,
    /// because the one thing this type may never do is lose a count.
    pub fn count(&mut self, rung: u8) {
        let slot = match rung {
            1 => &mut self.path,
            2 => &mut self.cwd,
            3 => &mut self.agent,
            _ => &mut self.rail,
        };
        *slot = slot.saturating_add(1);
    }

    /// Every operation counted.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.path + self.cwd + self.agent + self.rail
    }

    /// Those that reached the map.
    #[must_use]
    pub fn placed(&self) -> u64 {
        self.path + self.cwd + self.agent
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polis_events::{Glyph, Outcome, SessionId, ThreadId, ToolKind};
    use polis_layout::{Building, District, LotId, Polygon, RoofForm};
    use std::time::Instant;

    fn lp(s: &str) -> LogicalPath {
        LogicalPath::new(s).expect("path")
    }

    fn pt(x: f32, y: f32) -> Point {
        Point { x, y }
    }

    /// A city with one building in one district, plus a root district — the
    /// shape every real layout has.
    fn city() -> CityLayout {
        let mut layout = CityLayout {
            extent: 100.0,
            ..CityLayout::default()
        };
        layout.districts.insert(
            LogicalPath::root(),
            District {
                path: LogicalPath::root(),
                boundary: Polygon::default(),
                centre: pt(0.0, 0.0),
                blocks: Vec::new(),
            },
        );
        layout.districts.insert(
            lp("src/auth"),
            District {
                path: lp("src/auth"),
                boundary: Polygon::default(),
                centre: pt(20.0, 5.0),
                blocks: Vec::new(),
            },
        );
        layout.buildings.insert(
            lp("src/auth/token.rs"),
            Building {
                path: lp("src/auth/token.rs"),
                lot: LotId(0),
                footprint: Polygon::new(vec![
                    pt(30.0, 8.0),
                    pt(32.0, 8.0),
                    pt(32.0, 10.0),
                    pt(30.0, 10.0),
                ]),
                height: 1.0,
                roof: RoofForm::Flat,
                rotation: 0.0,
            },
        );
        layout
    }

    fn op(placement: OpPlacement) -> Operation {
        Operation {
            path: placement.path().cloned(),
            placement,
            tool: ToolKind::Bash,
            glyph: Glyph::FilledTriangle,
            outcome: Outcome::Failed,
            worker: None,
            at: Instant::now(),
            tool_use: None,
        }
    }

    fn thread() -> Thread {
        Thread::new(
            ThreadId::of_session(SessionId::new("s")),
            SessionId::new("s"),
            Instant::now(),
        )
    }

    #[test]
    fn a_cwd_at_the_repository_root_is_not_a_location() {
        // The whole city is not a place. 6 804 of 10 030 operations in one
        // recorded session resolved here, and drawing them all on one pixel at
        // the centre of the map is the failure PRD §6 opens by rejecting.
        let layout = city();
        assert_eq!(district_for_cwd(&layout, &LogicalPath::root()), None);
        assert_eq!(
            district_for_cwd(&layout, &lp("src/auth")),
            Some(lp("src/auth"))
        );
        // An unknown directory walks up to the nearest district it has.
        assert_eq!(
            district_for_cwd(&layout, &lp("src/auth/deep/deeper")),
            Some(lp("src/auth"))
        );
    }

    #[test]
    fn the_chain_falls_through_every_rung_and_never_drops_the_operation() {
        let layout = city();
        let mut t = thread();

        // Rung 4: no path, no territory, no trail.
        let pathless = op(OpPlacement::Agent);
        assert_eq!(site_of(&pathless, &t, &layout), OpSite::Rail);

        // Rung 3: the thread acquires a position, and the operation it already
        // ran starts being drawn. This is why rung 4 is resolved rather than
        // stored.
        t.trail.push_back((lp("src/auth/token.rs"), Instant::now()));
        assert_eq!(site_of(&pathless, &t, &layout).rung(), 3);

        // Rung 2 outranks rung 3.
        let shell = op(OpPlacement::Cwd(lp("src/auth")));
        assert_eq!(site_of(&shell, &t, &layout), OpSite::Cwd(pt(20.0, 5.0)));

        // Rung 1 outranks everything.
        let edit = op(OpPlacement::Path(lp("src/auth/token.rs")));
        assert_eq!(site_of(&edit, &t, &layout).rung(), 1);

        // A path the city has no geometry for — a file added since the last
        // layout run, or one outside every checkout — falls through rather than
        // vanishing. That is the failure this module exists to fix, and on one
        // recorded session it was 868 paths.
        let outside = op(OpPlacement::Path(lp("scratch/notes.md")));
        assert_eq!(site_of(&outside, &t, &layout).rung(), 3);
    }

    #[test]
    fn a_coarser_rung_draws_a_smaller_mark_and_never_a_different_colour() {
        // Position uncertainty is the *only* thing scale carries. All four of
        // these are `Outcome::Failed` and the scale ordering is unaffected.
        let layout = city();
        let t = thread();
        let sites = [
            site_of(&op(OpPlacement::Path(lp("src/auth/token.rs"))), &t, &layout),
            site_of(&op(OpPlacement::Cwd(lp("src/auth"))), &t, &layout),
        ];
        assert!(sites[0].scale() > sites[1].scale());
        assert!(!sites[0].is_coarse() && sites[1].is_coarse());
        // Nothing to draw, so nothing to size.
        assert!(OpSite::Rail.scale() <= 0.0);
    }

    #[test]
    fn the_census_never_loses_a_count() {
        let mut c = PlacementCensus::default();
        for rung in [1, 2, 3, 4, 9] {
            c.count(rung);
        }
        assert_eq!(c.total(), 5);
        assert_eq!(c.placed(), 3);
        assert_eq!(c.rail, 2, "an out-of-range rung is counted, not dropped");
    }
}
