//! Label placement and decluttering (PRD §13, §8).
//!
//! > It also gives you label collision and decluttering, which is the one
//! > genuinely hard thing a map engine like `MapLibre` would have bought you.
//!
//! So it is implemented rather than assumed. The algorithm is the standard
//! greedy one and the interesting part is the priority order, which comes
//! straight out of PRD §8:
//!
//! > **District-level skeleton must stay readable at all zooms even while the
//! > street level tangles.** District boundaries, monuments, and the skyline
//! > profile are the wayfinding layer and should survive when everything else is
//! > decluttered away.
//!
//! Monuments are therefore [`Priority::Anchor`] and are *never* dropped — they
//! are placed first and everything else yields to them. That is what
//! "always labelled at every zoom" means when the screen is full.
//!
//! # Why greedy is the right algorithm here
//!
//! Label placement is NP-hard in general and every practical map renderer solves
//! it greedily in priority order with a small set of candidate positions per
//! label. The thing that makes it feel stable rather than flickery is that the
//! priority order is a total order that does not depend on the frame: rank
//! first, then distance from the viewport centre, then the path itself. Two
//! frames with the same camera place the same labels.

use eframe::egui::{Align2, Pos2, Rect, Vec2};

/// How hard a label fights for its place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    /// Never dropped. Monuments and live attention marks — the wayfinding and
    /// the alerting layers, which PRD §8 and §10.3 both put above the map.
    Anchor,
    /// Dropped only when it collides with an anchor: district names.
    Structural,
    /// Dropped freely: file names, worker labels.
    Detail,
}

/// Padding around each label's box, so two labels never touch.
const PAD: f32 = 3.0;

/// Screen area, in square pixels, that buys one non-anchor label.
///
/// # Why a budget and not only a collision test
///
/// Collision alone answers *"do these two labels overlap"* and never answers
/// *"is this many labels readable"* — and on the operator's own repository the
/// second question is the one that was failing. Nine live threads over
/// `qurio-toolset` had touched several hundred files; every one of them was a
/// candidate, none of them collided after the placer was done with them, and
/// the result was a map with sixty names on it that PRD §17's test cuts on
/// sight: a name that is one of sixty does not change a decision.
///
/// So the placer also has a *count*. Cartography has always had one — a road
/// atlas, a transit diagram and a weather chart all carry on the order of
/// twenty to forty place names per view at every zoom, and they thin the set as
/// you zoom out rather than shrinking the type. `44 000` px² per label puts a
/// 1280 x 900 pane at 26 and the operator's 2077 x 1217 pane at 28 (the
/// ceiling), which is that range.
///
/// The budget is spent in PRD §8's priority order, so what survives is the
/// wayfinding skeleton and the work in progress, not whatever the map happened
/// to iterate first.
const AREA_PER_LABEL: f32 = 44_000.0;

/// The most, and fewest, non-anchor labels one frame may place.
///
/// The ceiling is what a glance can carry; the floor keeps a small pane — a
/// half-width window, a thumbnail — from losing its district names entirely.
const BUDGET_RANGE: (usize, usize) = (8, 28);

/// A greedy, priority-ordered label placer.
///
/// One per frame. Feed it labels in descending priority; it answers with the
/// rectangle to draw in, or `None` when the label was decluttered away — either
/// because it collided with something already placed, or because the frame's
/// budget (`AREA_PER_LABEL`) is spent.
#[derive(Debug)]
pub struct LabelPlacer {
    /// The rectangle labels must stay inside — the map viewport.
    bounds: Rect,
    /// Boxes already claimed this frame.
    placed: Vec<Rect>,
    /// The subset of [`Self::placed`] that belongs to [`Priority::Anchor`].
    ///
    /// Kept apart because an anchor is the one label allowed to take an
    /// occupied box, and the box it must never take is **another anchor's**.
    /// Two forced labels on one point is the only way this placer can produce
    /// unreadable overlapping type, and on the operator's live map it did: a
    /// monument's name and a blocked thread's title landed on the same building
    /// and neither could be read.
    anchors: Vec<Rect>,
    /// How many labels were dropped, which the status bar shows so that
    /// "decluttered" and "not drawn" never look the same.
    dropped: usize,
    /// How many non-anchor labels this frame may still place.
    ///
    /// [`Priority::Anchor`] does not draw on it: PRD §8 makes monuments the
    /// orientation layer and says they are labelled *at every zoom*, so they are
    /// the frame's fixed cost and everything else competes for what is left.
    budget: usize,
    /// Non-anchor labels placed so far.
    spent: usize,
}

impl LabelPlacer {
    /// A placer for one frame over one viewport, with the budget the viewport's
    /// area buys.
    pub fn new(bounds: Rect) -> Self {
        let area = bounds.width().max(0.0) * bounds.height().max(0.0);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let budget = (area / AREA_PER_LABEL) as usize;
        Self::with_budget(bounds, budget.clamp(BUDGET_RANGE.0, BUDGET_RANGE.1))
    }

    /// A placer with an explicit budget. The tests use it; the window does not.
    pub fn with_budget(bounds: Rect, budget: usize) -> Self {
        Self {
            bounds,
            placed: Vec::new(),
            anchors: Vec::new(),
            dropped: 0,
            budget,
            spent: 0,
        }
    }

    /// How many non-anchor labels this frame may still place.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.budget.saturating_sub(self.spent)
    }

    /// How many labels found a place.
    pub fn placed(&self) -> usize {
        self.placed.len()
    }

    /// How many were decluttered away.
    pub fn dropped(&self) -> usize {
        self.dropped
    }

    /// Tries to place a label of `size` near `anchor`.
    ///
    /// Candidates are tried in order: above the anchor, below, right, left, then
    /// the four diagonals. An [`Priority::Anchor`] label that finds no free
    /// candidate takes its first one anyway and pushes everything later out of
    /// the way, because PRD §8 says monuments are labelled at every zoom.
    ///
    /// Two things can decluttter a label away, and they answer different
    /// questions: a collision means *"there is no room here"*, and a spent
    /// budget (`AREA_PER_LABEL`) means *"there are already enough names on this
    /// map"*. Both count as [`Self::dropped`], because from the operator's side
    /// they are the same event — a name that is not on screen.
    pub fn place(&mut self, anchor: Pos2, size: Vec2, priority: Priority) -> Option<Rect> {
        if priority != Priority::Anchor && self.spent >= self.budget {
            self.dropped += 1;
            return None;
        }
        let size = size + Vec2::splat(PAD * 2.0);
        let mut first = None;
        let mut clear_of_anchors = None;
        for (align, offset) in CANDIDATES {
            let rect = align
                .align_size_within_rect(size, Rect::from_center_size(anchor + *offset, Vec2::ZERO));
            if !self.bounds.intersects(rect) {
                continue;
            }
            let rect = clamp_into(rect, self.bounds);
            if first.is_none() {
                first = Some(rect);
            }
            if clear_of_anchors.is_none() && !self.anchors.iter().any(|a| a.intersects(rect)) {
                clear_of_anchors = Some(rect);
            }
            if self.placed.iter().all(|other| !other.intersects(rect)) {
                return Some(self.claim(rect, priority));
            }
        }
        if priority == Priority::Anchor {
            // Forced, because PRD §8 says a monument is labelled at every zoom
            // — but forced onto a box that no *other* anchor is using, so the
            // guarantee costs a district name and never another guarantee.
            if let Some(rect) = clear_of_anchors.or(first) {
                return Some(self.claim(rect, priority));
            }
        }
        self.dropped += 1;
        None
    }

    /// Records a placed box and charges it to the frame's budget.
    fn claim(&mut self, rect: Rect, priority: Priority) -> Rect {
        self.placed.push(rect);
        if priority == Priority::Anchor {
            self.anchors.push(rect);
        } else {
            self.spent += 1;
        }
        rect.shrink(PAD)
    }

    /// Reserves a rectangle that is not a label — a panel, a tooltip, the
    /// transport bar — so labels do not slide under it.
    pub fn reserve(&mut self, rect: Rect) {
        self.placed.push(rect);
    }
}

/// Candidate positions, best first. `Align2` here says where the *anchor* sits
/// relative to the label box, so `CENTER_BOTTOM` puts the label above the point.
const CANDIDATES: &[(Align2, Vec2)] = &[
    (Align2::CENTER_BOTTOM, Vec2::new(0.0, -4.0)),
    (Align2::CENTER_TOP, Vec2::new(0.0, 4.0)),
    (Align2::LEFT_CENTER, Vec2::new(6.0, 0.0)),
    (Align2::RIGHT_CENTER, Vec2::new(-6.0, 0.0)),
    (Align2::LEFT_BOTTOM, Vec2::new(5.0, -5.0)),
    (Align2::RIGHT_BOTTOM, Vec2::new(-5.0, -5.0)),
    (Align2::LEFT_TOP, Vec2::new(5.0, 5.0)),
    (Align2::RIGHT_TOP, Vec2::new(-5.0, 5.0)),
];

/// Slides a rectangle fully inside `bounds` when it can fit.
fn clamp_into(rect: Rect, bounds: Rect) -> Rect {
    let mut min = rect.min;
    if rect.width() <= bounds.width() {
        min.x = min.x.clamp(bounds.min.x, bounds.max.x - rect.width());
    }
    if rect.height() <= bounds.height() {
        min.y = min.y.clamp(bounds.min.y, bounds.max.y - rect.height());
    }
    Rect::from_min_size(min, rect.size())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placer() -> LabelPlacer {
        LabelPlacer::new(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0)))
    }

    #[test]
    fn two_labels_at_the_same_point_do_not_overlap() {
        let mut p = placer();
        let size = Vec2::new(80.0, 14.0);
        let a = p
            .place(Pos2::new(400.0, 300.0), size, Priority::Detail)
            .expect("first");
        let b = p
            .place(Pos2::new(400.0, 300.0), size, Priority::Detail)
            .expect("second");
        assert!(!a.intersects(b), "{a:?} {b:?}");
    }

    #[test]
    fn a_crowded_point_eventually_declutters() {
        let mut p = placer();
        let size = Vec2::new(120.0, 14.0);
        let mut placed = 0;
        for _ in 0..40 {
            if p.place(Pos2::new(400.0, 300.0), size, Priority::Detail)
                .is_some()
            {
                placed += 1;
            }
        }
        assert!(placed < 40, "everything was placed, so nothing decluttered");
        assert_eq!(placed + p.dropped(), 40);
    }

    /// A forced label may sit on a district name and must not sit on another
    /// forced one. A monument's name and a blocked thread's title landing on
    /// the same building was the one way this placer could still emit
    /// unreadable type, and on the operator's live map it did — twice in one
    /// frame.
    ///
    /// The guarantee is *best effort by construction*: with eight candidate
    /// boxes around one point there are only about two that do not overlap each
    /// other, so a third forced label at the same pixel still has to take an
    /// occupied box. Two is the case that occurs and the case this fixes.
    #[test]
    fn a_forced_label_yields_to_another_forced_label_before_taking_its_box() {
        let mut p = placer();
        let size = Vec2::new(60.0, 14.0);
        let at = Pos2::new(400.0, 300.0);
        let monument = p.place(at, size, Priority::Anchor).expect("monument");
        // Every remaining box is now taken by district names, so the thread
        // title has to force — and the box it forces onto must not be the
        // monument's.
        for _ in 0..40 {
            let _ = p.place(at, size, Priority::Structural);
        }
        let title = p.place(at, size, Priority::Anchor).expect("thread title");
        assert!(
            !monument.intersects(title),
            "two forced labels overlap: {monument:?} {title:?}"
        );
    }

    /// PRD §8: monuments are "always labelled at every zoom". A monument label
    /// is placed even when every candidate box is already taken.
    #[test]
    fn an_anchor_label_is_never_dropped() {
        let mut p = placer();
        let size = Vec2::new(200.0, 16.0);
        for _ in 0..60 {
            let _ = p.place(Pos2::new(400.0, 300.0), size, Priority::Detail);
        }
        let dropped_before = p.dropped();
        assert!(dropped_before > 0, "the point should be saturated by now");
        assert!(
            p.place(Pos2::new(400.0, 300.0), size, Priority::Anchor)
                .is_some(),
            "a monument yielded its label"
        );
        assert_eq!(
            p.dropped(),
            dropped_before,
            "an anchor never counts as dropped"
        );
    }

    #[test]
    fn a_label_off_the_viewport_is_dropped_not_clamped_into_view() {
        let mut p = placer();
        assert!(p
            .place(
                Pos2::new(-900.0, -900.0),
                Vec2::new(60.0, 14.0),
                Priority::Detail
            )
            .is_none());
        assert_eq!(p.dropped(), 1);
    }

    #[test]
    fn a_label_near_the_edge_is_slid_fully_into_view() {
        let mut p = placer();
        let rect = p
            .place(
                Pos2::new(795.0, 300.0),
                Vec2::new(120.0, 14.0),
                Priority::Detail,
            )
            .expect("placed");
        assert!(rect.max.x <= 800.0 + 0.01, "{rect:?}");
    }

    #[test]
    fn a_reserved_rectangle_pushes_labels_out_of_it() {
        let mut p = placer();
        let panel = Rect::from_min_size(Pos2::new(300.0, 200.0), Vec2::new(300.0, 200.0));
        p.reserve(panel);
        let rect = p.place(
            Pos2::new(450.0, 300.0),
            Vec2::new(60.0, 14.0),
            Priority::Detail,
        );
        assert!(rect.is_none_or(|r| !r.intersects(panel)));
    }

    /// The soup, as a test. Two hundred well-separated file names all fit
    /// without colliding, and a map with two hundred names on it is exactly
    /// what PRD §17 cuts — so the count, not the collision, has to stop them.
    #[test]
    fn a_map_full_of_room_still_stops_at_the_budget() {
        let mut p = placer();
        let mut placed = 0;
        for i in 0..200i16 {
            #[allow(clippy::cast_precision_loss)] // a loop index, not a measurement
            let anchor = Pos2::new(
                20.0 + f32::from(i % 20) * 38.0,
                20.0 + f32::from(i / 20) * 55.0,
            );
            if p.place(anchor, Vec2::new(30.0, 12.0), Priority::Detail)
                .is_some()
            {
                placed += 1;
            }
        }
        assert_eq!(placed, 10, "800x600 buys ten non-anchor labels, not 200");
        assert_eq!(p.dropped(), 190);
        assert_eq!(p.remaining(), 0);
    }

    /// The budget thins with the viewport, which is what "thin out with zoom"
    /// means for a pane that is not resized: a wider map earns more names, and
    /// neither end runs away.
    #[test]
    fn the_budget_follows_the_viewport_between_a_floor_and_a_ceiling() {
        let for_size = |w: f32, h: f32| {
            LabelPlacer::new(Rect::from_min_size(Pos2::ZERO, Vec2::new(w, h))).remaining()
        };
        assert_eq!(for_size(320.0, 240.0), BUDGET_RANGE.0, "a thumbnail");
        assert_eq!(for_size(800.0, 600.0), 10, "a half window");
        assert_eq!(for_size(1280.0, 900.0), 26, "a window");
        assert_eq!(for_size(2077.0, 1217.0), BUDGET_RANGE.1, "the operator's");
        assert_eq!(for_size(6000.0, 4000.0), BUDGET_RANGE.1, "a wall display");
    }

    /// PRD §8's monuments are the frame's fixed cost. A budget that they drew
    /// on could be spent by the wayfinding layer before the work in progress
    /// ever got a name — or, worse, spent by file names before the monuments.
    #[test]
    fn monuments_do_not_draw_on_the_budget() {
        let mut p =
            LabelPlacer::with_budget(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0)), 3);
        for i in 0..6i16 {
            assert!(
                p.place(
                    Pos2::new(60.0 + f32::from(i) * 110.0, 80.0),
                    Vec2::new(70.0, 14.0),
                    Priority::Anchor,
                )
                .is_some(),
                "monument {i} was decluttered"
            );
        }
        assert_eq!(p.remaining(), 3, "monuments spent the budget");
        for i in 0..3i16 {
            assert!(p
                .place(
                    Pos2::new(60.0 + f32::from(i) * 110.0, 300.0),
                    Vec2::new(70.0, 14.0),
                    Priority::Structural,
                )
                .is_some());
        }
        assert_eq!(p.remaining(), 0);
        assert!(p
            .place(
                Pos2::new(600.0, 500.0),
                Vec2::new(70.0, 14.0),
                Priority::Structural,
            )
            .is_none());
    }

    /// Stability: the same camera twice must place the same labels, or the map
    /// flickers. Guaranteed by the placer being a pure function of its inputs.
    #[test]
    fn the_same_input_places_the_same_labels() {
        let run = || {
            let mut p = placer();
            let mut out = Vec::new();
            for i in 0..30i16 {
                #[allow(clippy::cast_precision_loss)] // a loop index, not a measurement
                let anchor = Pos2::new(
                    100.0 + f32::from(i % 6) * 12.0,
                    100.0 + f32::from(i / 6) * 9.0,
                );
                out.push(p.place(anchor, Vec2::new(70.0, 14.0), Priority::Detail));
            }
            out
        };
        assert_eq!(run(), run());
    }
}
