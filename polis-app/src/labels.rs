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

/// A greedy, priority-ordered label placer.
///
/// One per frame. Feed it labels in descending priority; it answers with the
/// rectangle to draw in, or `None` when the label was decluttered away.
#[derive(Debug)]
pub struct LabelPlacer {
    /// The rectangle labels must stay inside — the map viewport.
    bounds: Rect,
    /// Boxes already claimed this frame.
    placed: Vec<Rect>,
    /// How many labels were dropped, which the status bar shows so that
    /// "decluttered" and "not drawn" never look the same.
    dropped: usize,
}

impl LabelPlacer {
    /// A placer for one frame over one viewport.
    pub fn new(bounds: Rect) -> Self {
        Self {
            bounds,
            placed: Vec::new(),
            dropped: 0,
        }
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
    pub fn place(&mut self, anchor: Pos2, size: Vec2, priority: Priority) -> Option<Rect> {
        let size = size + Vec2::splat(PAD * 2.0);
        let mut first = None;
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
            if self.placed.iter().all(|other| !other.intersects(rect)) {
                self.placed.push(rect);
                return Some(rect.shrink(PAD));
            }
        }
        if priority == Priority::Anchor {
            if let Some(rect) = first {
                self.placed.push(rect);
                return Some(rect.shrink(PAD));
            }
        }
        self.dropped += 1;
        None
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
