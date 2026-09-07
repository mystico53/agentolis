//! Cloud callouts: a leader out of a territory, and the caption at the end of
//! it (PRD §6.4, §10.3, §13).
//!
//! # Why a cloud needed a caption at all
//!
//! A cloud says *where* a thread is working and *whose* it is — a hue and a
//! silhouette — and it has never said anything else. Everything the operator
//! could read about a thread lived in the rail: the name, the state, the call
//! count. So the map answered "something is happening over there" and the
//! operator then had to find the matching swatch in a list to learn which
//! thread it was, which is the lookup the map exists to remove.
//!
//! The caption is therefore attached to the cloud rather than filed beside it.
//!
//! # The shape, and why it is that shape
//!
//! A leader leaves the silhouette **horizontally**, at the height the field
//! peaks. It runs out, bends once, and runs horizontally into the caption. When
//! nothing pushed the caption off its cloud's height — the common case, because
//! PRD §10.4 caps the sky at a handful of clouds — all four points are
//! collinear and it reads as one straight line.
//!
//! The bend is where the honesty is. Two clouds at the same height cannot both
//! have a straight leader, and the alternatives are to overlap the captions
//! (unreadable) or to drop one (the map stops naming a live thread). A knee
//! costs a few pixels of ink and keeps both.
//!
//! # The math, in the order it runs
//!
//! 1. **Side.** Each cloud takes the side of the viewport it is already on, so
//!    a leader points away from the city rather than back across it. A side
//!    that cannot hold the caption flips; one that fits neither takes whichever
//!    has more room, because a caption over its own cloud's fringe still names
//!    the thread and a dropped one does not.
//! 2. **Gutter.** Every caption on a side shares one near edge, placed clear of
//!    the widest cloud on that side. Parallel leaders of differing length read
//!    as one system; captions at nine different depths read as scatter. It also
//!    makes it impossible for a caption to land on a cloud that is on its own
//!    side, which no amount of per-caption collision testing would guarantee.
//! 3. **Stack.** Captions are ordered by the height of their own cloud and then
//!    de-overlapped along y. Same order in, same order out, so leaders never
//!    cross.
//! 4. **Budget.** A side whose captions cannot all fit between the top and
//!    bottom margins drops from the back, and the caller hands them over in
//!    priority order so what it drops is the least important.
//!
//! # Why the de-overlap is two algorithms and not one
//!
//! [`stack`] runs a cluster merge and *then* two clamping sweeps. The merge is
//! what makes the result look considered: a group of captions that collide is
//! placed as a block centred on where its members wanted to be, so a pair
//! pushes symmetrically apart instead of the lower one sliding down and the
//! upper one keeping its place. It is the standard
//! minimum-total-displacement solution for intervals on a line, and on its own
//! it is *nearly* right — its output can still leave the viewport.
//!
//! The sweeps are the guarantee. A forward pass enforces the top margin and the
//! separation, a backward pass enforces the bottom margin, and because the
//! caller has already dropped captions until the stack fits, the second pass
//! cannot undo the first. Optimality is a preference; not overlapping is the
//! contract, and they are separated here so that the contract does not depend
//! on the preference being correct.

// The counts here are label counts — `polis_world::territory::CLOUD_CAP` of
// them, plus a handful — and they are divided into pixel sums. There is no
// `usize` this module can hold that `f32` cannot represent exactly.
#![allow(clippy::cast_precision_loss)]

use eframe::egui::{Pos2, Rect, Vec2};

/// How far the leader runs horizontally out of the silhouette before it may
/// bend.
///
/// Long enough that the bend is clearly outside the cloud rather than sitting
/// on its fringe, where the eye would read it as part of the contour.
const STUB: f32 = 16.0;

/// How far the leader runs horizontally into the caption after the bend.
///
/// This is what makes the line meet the caption square, so the arrival reads as
/// deliberate at any displacement.
const RUN: f32 = 14.0;

/// Clear vertical space between two captions.
const LANE: f32 = 7.0;

/// How close to the edge of the map a caption may sit.
const MARGIN: f32 = 10.0;

/// Which way a callout leaves its cloud.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// Caption to the left of the cloud; the leader runs `-x`.
    Left,
    /// Caption to the right of the cloud; the leader runs `+x`.
    Right,
}

/// One cloud asking to be named. Given in priority order, highest first.
#[derive(Debug, Clone, Copy)]
pub struct Request {
    /// The silhouette the leader must clear, in screen pixels.
    ///
    /// The kernels' bounding box, which contains the drawn cloud exactly:
    /// `polis_render::live`'s kernel has compact support and is zero at its own
    /// radius, so no contour can reach outside it.
    pub cloud: Rect,
    /// The height the leader leaves at — where the field peaks, which is not
    /// the middle of the box when a territory has lobes of unequal weight.
    pub at: f32,
    /// How wide the silhouette is at [`Self::at`], as `(left, right)` screen x.
    ///
    /// Where the leader **attaches**, as against [`Self::cloud`], which is what
    /// the caption has to stay clear of. They are the same edge for a round
    /// cloud and a long way apart for a lobed one, and using the box for both
    /// is what leaves a leader starting in open map with a gap behind its dot.
    pub span: (f32, f32),
    /// The caption's size.
    pub size: Vec2,
}

/// Where one caption ended up.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Callout {
    /// Which way it left.
    pub side: Side,
    /// The caption's box.
    pub label: Rect,
    /// The leader: on the silhouette, out, the bend, into the caption. Points
    /// 0–1 and 2–3 are horizontal by construction; 1–2 carries whatever
    /// displacement [`stack`] gave the caption, and is zero-length when it gave
    /// it none.
    pub leader: [Pos2; 4],
}

/// Lays out every callout for one frame.
///
/// Returns one entry per request, in the order they were given. `None` is a
/// caption that did not fit — the caller draws nothing for it, and the cloud
/// keeps its hue and its place in the rail.
///
/// A pure function of its arguments, so two frames with the same camera lay out
/// identically and the captions do not shimmer.
#[must_use]
pub fn lay_out(requests: &[Request], bounds: Rect) -> Vec<Option<Callout>> {
    let mut out = vec![None; requests.len()];
    if bounds.width() <= 0.0 || bounds.height() <= 0.0 {
        return out;
    }

    // 1. Side, and the requests that cannot be drawn at any width.
    let mut sides: Vec<(usize, Side)> = Vec::with_capacity(requests.len());
    for (i, req) in requests.iter().enumerate() {
        if req.size.x + MARGIN * 2.0 > bounds.width() || req.size.y + MARGIN * 2.0 > bounds.height()
        {
            continue;
        }
        sides.push((i, side_for(req, bounds)));
    }

    for side in [Side::Left, Side::Right] {
        // Priority order is the request order, and it is what the budget spends
        // and what ties in the stack fall back on.
        let group: Vec<usize> = sides
            .iter()
            .filter(|(_, s)| *s == side)
            .map(|(i, _)| *i)
            .collect();
        place_side(requests, bounds, side, &group, &mut out);
    }
    out
}

/// The side a cloud is already on, unless that side cannot hold the caption.
///
/// Room is measured from the silhouette, because the leader has to clear it: a
/// cloud hard against the right edge has no room on the right however much
/// empty map is on its left.
fn side_for(req: &Request, bounds: Rect) -> Side {
    let right = bounds.max.x - (req.cloud.max.x + STUB + RUN) - MARGIN;
    let left = (req.cloud.min.x - STUB - RUN) - bounds.min.x - MARGIN;
    let prefers_right = req.cloud.center().x >= bounds.center().x;
    let fits = |room: f32| room >= req.size.x;
    match (prefers_right, fits(right), fits(left)) {
        (true, true, _) | (false, true, false) => Side::Right,
        (false, _, true) | (true, false, true) => Side::Left,
        // Neither side can hold it clear of the cloud, which is a cloud wider
        // than the map. Take the roomier one and let the gutter clamp: a
        // caption over its own fringe still names the thread.
        _ => {
            if right >= left {
                Side::Right
            } else {
                Side::Left
            }
        }
    }
}

/// Gutter, budget and stack for the captions on one side.
fn place_side(
    requests: &[Request],
    bounds: Rect,
    side: Side,
    group: &[usize],
    out: &mut [Option<Callout>],
) {
    if group.is_empty() {
        return;
    }

    // 2. One near edge for the whole side, clear of every cloud on it.
    let widest = group
        .iter()
        .map(|i| requests[*i].size.x)
        .fold(0.0f32, f32::max);
    let gutter = match side {
        Side::Right => {
            let want = group
                .iter()
                .map(|i| requests[*i].cloud.max.x + STUB + RUN)
                .fold(f32::NEG_INFINITY, f32::max);
            want.min(bounds.max.x - MARGIN - widest)
                .max(bounds.min.x + MARGIN)
        }
        Side::Left => {
            let want = group
                .iter()
                .map(|i| requests[*i].cloud.min.x - STUB - RUN)
                .fold(f32::INFINITY, f32::min);
            want.max(bounds.min.x + MARGIN + widest)
                .min(bounds.max.x - MARGIN)
        }
    };

    // 3. The budget. Captions come in priority order, so the tail is what goes.
    let room = bounds.height() - MARGIN * 2.0;
    let mut kept: Vec<usize> = group.to_vec();
    while stack_height(requests, &kept) > room && !kept.is_empty() {
        kept.pop();
    }
    if kept.is_empty() {
        return;
    }

    // 4. The stack, in cloud order so leaders cannot cross. Ties keep priority
    //    order, which is what makes the layout stable frame to frame.
    let mut order = kept.clone();
    order.sort_by(|a, b| {
        requests[*a]
            .at
            .total_cmp(&requests[*b].at)
            .then_with(|| a.cmp(b))
    });
    let tops = stack(
        &order.iter().map(|i| requests[*i].at).collect::<Vec<_>>(),
        &order
            .iter()
            .map(|i| requests[*i].size.y)
            .collect::<Vec<_>>(),
        bounds.min.y + MARGIN,
        bounds.max.y - MARGIN,
    );

    for (slot, i) in order.iter().enumerate() {
        let req = &requests[*i];
        let label = match side {
            Side::Right => Rect::from_min_size(Pos2::new(gutter, tops[slot]), req.size),
            Side::Left => Rect::from_min_size(Pos2::new(gutter - req.size.x, tops[slot]), req.size),
        };
        out[*i] = Some(Callout {
            side,
            label,
            leader: leader(req, label, side),
        });
    }
}

/// The height a set of captions needs, lanes included.
fn stack_height(requests: &[Request], group: &[usize]) -> f32 {
    if group.is_empty() {
        return 0.0;
    }
    let heights: f32 = group.iter().map(|i| requests[*i].size.y).sum();
    heights + LANE * (group.len() - 1) as f32
}

/// De-overlaps a column of boxes along y, given where each one wants its centre.
///
/// `wanted` and `heights` are parallel and already sorted by `wanted`. Returns
/// the top of each box, in the same order.
///
/// The clusters do the aesthetics and the sweeps do the guarantee — see the
/// module docs. Public because the guarantee is worth testing on its own.
#[must_use]
pub fn stack(wanted: &[f32], heights: &[f32], top: f32, bottom: f32) -> Vec<f32> {
    // One run of boxes that will be placed touching, at the position that
    // minimises the total squared displacement of its members.
    struct Cluster {
        /// Index of the first member in `wanted`.
        first: usize,
        /// How many members.
        n: usize,
        /// Their heights and lanes, summed.
        height: f32,
        /// Σ over members of (where it wanted its top − its offset in the run).
        /// The cluster's own top is this over `n`.
        pull: f32,
    }

    let mut clusters: Vec<Cluster> = Vec::with_capacity(wanted.len());
    for (i, (want, h)) in wanted.iter().zip(heights).enumerate() {
        clusters.push(Cluster {
            first: i,
            n: 1,
            height: *h,
            pull: want - h / 2.0,
        });
        // Merge backwards for as long as this cluster's arrival made the one
        // before it overlap. Each merge re-centres the whole run, which can
        // push it into the run before that, so this is a loop and not a test.
        while clusters.len() >= 2 {
            let b = clusters.pop().expect("len >= 2");
            let a = clusters.pop().expect("len >= 2");
            let a_top = a.pull / a.n as f32;
            let b_top = b.pull / b.n as f32;
            if a_top + a.height + LANE <= b_top {
                clusters.push(a);
                clusters.push(b);
                break;
            }
            // `b`'s members now sit `a.height + LANE` further down the run, so
            // each of them wants the run's top that much higher.
            let shift = a.height + LANE;
            clusters.push(Cluster {
                first: a.first,
                n: a.n + b.n,
                height: a.height + LANE + b.height,
                pull: a.pull + (b.pull - b.n as f32 * shift),
            });
        }
    }

    let mut tops = vec![0.0f32; wanted.len()];
    for c in &clusters {
        let mut y = c.pull / c.n as f32;
        for k in c.first..c.first + c.n {
            tops[k] = y;
            y += heights[k] + LANE;
        }
    }

    // The guarantee, independent of everything above: forward for the top edge
    // and the lanes, backward for the bottom edge. The caller has already
    // dropped captions until the column fits, so the backward pass cannot push
    // anything back through the top.
    let mut y = top;
    for (k, h) in heights.iter().enumerate() {
        tops[k] = tops[k].max(y);
        y = tops[k] + h + LANE;
    }
    let mut y = bottom;
    for k in (0..heights.len()).rev() {
        tops[k] = tops[k].min(y - heights[k]);
        y = tops[k] - LANE;
    }
    tops
}

/// The four points of one leader.
///
/// It attaches at [`Request::span`] — the silhouette at the anchor's own height
/// — and bends no earlier than [`Request::cloud`]'s edge, so a lobed territory
/// gets a leader that both starts on ink and clears the whole shape before it
/// turns.
fn leader(req: &Request, label: Rect, side: Side) -> [Pos2; 4] {
    let cy = label.center().y;
    match side {
        Side::Right => {
            let p3 = Pos2::new(label.min.x, cy);
            let p2 = Pos2::new((p3.x - RUN).max(req.cloud.max.x), cy);
            let p0 = Pos2::new(req.span.1.min(req.cloud.max.x), req.at);
            // The stub never runs past the bend: a clamped gutter can leave
            // less room than `STUB` asks for, and a leader that doubles back on
            // itself reads as a mistake.
            let p1 = Pos2::new((p0.x + STUB).min(p2.x).max(p0.x), req.at);
            [p0, p1, p2, p3]
        }
        Side::Left => {
            let p3 = Pos2::new(label.max.x, cy);
            let p2 = Pos2::new((p3.x + RUN).min(req.cloud.min.x), cy);
            let p0 = Pos2::new(req.span.0.max(req.cloud.min.x), req.at);
            let p1 = Pos2::new((p0.x - STUB).max(p2.x).min(p0.x), req.at);
            [p0, p1, p2, p3]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds() -> Rect {
        Rect::from_min_size(Pos2::ZERO, Vec2::new(1200.0, 800.0))
    }

    /// A round cloud, whose span at its own centre is its full width.
    fn cloud(cx: f32, cy: f32, r: f32) -> Request {
        Request {
            cloud: Rect::from_center_size(Pos2::new(cx, cy), Vec2::splat(r * 2.0)),
            at: cy,
            span: (cx - r, cx + r),
            size: Vec2::new(150.0, 44.0),
        }
    }

    fn placed(out: &[Option<Callout>]) -> Vec<Callout> {
        out.iter().flatten().copied().collect()
    }

    /// The whole point of the shape: with room to spare, the leader is one
    /// straight horizontal line from the silhouette to the caption.
    #[test]
    fn an_uncrowded_callout_leaves_horizontally_and_stays_horizontal() {
        let req = [cloud(800.0, 400.0, 60.0)];
        let out = lay_out(&req, bounds());
        let c = out[0].expect("placed");
        assert_eq!(c.side, Side::Right);
        for p in c.leader {
            assert!(
                (p.y - 400.0).abs() < 0.01,
                "leader is not horizontal: {:?}",
                c.leader
            );
        }
        assert!(
            c.leader[0].x >= 860.0 - 0.01,
            "the leader starts inside the cloud: {:?}",
            c.leader
        );
        assert!(c.leader[3].x <= c.label.min.x + 0.01);
    }

    /// A cloud on the left half is captioned on its left, so no leader crosses
    /// the city it is pointing at.
    #[test]
    fn a_cloud_takes_the_side_of_the_map_it_is_already_on() {
        let out = lay_out(
            &[cloud(400.0, 300.0, 50.0), cloud(800.0, 300.0, 50.0)],
            bounds(),
        );
        let left = out[0].expect("left");
        assert_eq!(left.side, Side::Left);
        assert_eq!(out[1].expect("right").side, Side::Right);
        assert!(
            left.label.max.x <= 350.0 - STUB - RUN + 0.01,
            "the caption is not clear of its own cloud: {:?}",
            left.label
        );
    }

    /// A cloud hard against the edge it would prefer flips rather than pushing
    /// its caption off the map.
    #[test]
    fn a_cloud_against_the_edge_flips_to_the_side_with_room() {
        let out = lay_out(&[cloud(1180.0, 400.0, 40.0)], bounds());
        let c = out[0].expect("placed");
        assert_eq!(c.side, Side::Left);
        assert!(bounds().contains_rect(c.label), "{:?}", c.label);
    }

    /// The failure this module is for. Four clouds at the same height on the
    /// same side is four captions wanting one row of pixels.
    #[test]
    fn captions_at_the_same_height_never_overlap() {
        let reqs: Vec<Request> = (0..4i16)
            .map(|i| cloud(700.0 + f32::from(i) * 10.0, 400.0, 40.0))
            .collect();
        let out = lay_out(&reqs, bounds());
        let all = placed(&out);
        assert_eq!(all.len(), 4, "a caption was dropped with room to spare");
        for (i, a) in all.iter().enumerate() {
            for b in all.iter().skip(i + 1) {
                assert!(
                    !a.label.intersects(b.label),
                    "{:?} overlaps {:?}",
                    a.label,
                    b.label
                );
            }
        }
    }

    /// Displacement is shared. Two captions colliding head-on move by the same
    /// amount in opposite directions rather than one of them keeping its place
    /// — which is what the cluster merge buys over a plain downward sweep.
    #[test]
    fn a_collision_pushes_both_captions_symmetrically() {
        let out = lay_out(
            &[cloud(700.0, 400.0, 40.0), cloud(720.0, 400.0, 40.0)],
            bounds(),
        );
        let a = out[0].expect("first").label.center().y;
        let b = out[1].expect("second").label.center().y;
        assert!(
            ((400.0 - a) + (400.0 - b)).abs() < 0.5,
            "displacement is lopsided: {a} and {b} about 400"
        );
        assert!((b - a).abs() >= 44.0 + LANE - 0.01, "{a} {b}");
    }

    /// Leaders must not cross, or two clouds swap identities at a glance. The
    /// stack is assigned in cloud order, so the caption order matches.
    #[test]
    fn leaders_never_cross() {
        let reqs: Vec<Request> = (0..5i16)
            .map(|i| cloud(700.0, 380.0 + f32::from(i) * 12.0, 40.0))
            .collect();
        let out = lay_out(&reqs, bounds());
        let mut by_cloud: Vec<(f32, f32)> = out
            .iter()
            .flatten()
            .map(|c| (c.leader[0].y, c.label.center().y))
            .collect();
        assert_eq!(by_cloud.len(), 5);
        by_cloud.sort_by(|a, b| a.0.total_cmp(&b.0));
        for pair in by_cloud.windows(2) {
            assert!(
                pair[0].1 <= pair[1].1,
                "a lower cloud got a higher caption: {pair:?}"
            );
        }
    }

    /// Every caption stays on the map, including one whose cloud is off the top
    /// of it.
    #[test]
    fn a_caption_never_leaves_the_viewport() {
        let reqs = [
            cloud(700.0, -400.0, 40.0),
            cloud(700.0, 1400.0, 40.0),
            cloud(700.0, 400.0, 40.0),
        ];
        let out = lay_out(&reqs, bounds());
        for c in placed(&out) {
            assert!(
                bounds().contains_rect(c.label),
                "{:?} is outside {:?}",
                c.label,
                bounds()
            );
        }
    }

    /// PRD §10.4 caps the sky, but a small pane can still be asked for more
    /// captions than it has rows. What goes is the tail, because the caller
    /// hands them over worst-first.
    #[test]
    fn a_full_column_drops_from_the_back() {
        let short = Rect::from_min_size(Pos2::ZERO, Vec2::new(1200.0, 190.0));
        let reqs: Vec<Request> = (0..6i16)
            .map(|i| cloud(700.0, 95.0, 30.0 + f32::from(i)))
            .collect();
        let out = lay_out(&reqs, short);
        assert!(out[0].is_some(), "the most important caption went first");
        assert!(out[5].is_none(), "the column did not spend its budget");
        for (i, a) in placed(&out).iter().enumerate() {
            for b in placed(&out).iter().skip(i + 1) {
                assert!(!a.label.intersects(b.label));
            }
        }
    }

    /// A caption wider than the map is not drawable at any position, and is
    /// dropped rather than clamped into something unreadable.
    #[test]
    fn a_caption_too_large_for_the_map_is_dropped() {
        let mut req = cloud(600.0, 400.0, 20.0);
        req.size.x = 4_000.0;
        assert!(lay_out(&[req], bounds())[0].is_none());
    }

    /// Stability: the placer is a pure function, so an unmoved camera lays out
    /// identically and the captions do not shimmer.
    #[test]
    fn the_same_input_lays_out_the_same_way() {
        let reqs: Vec<Request> = (0..5i16)
            .map(|i| {
                cloud(
                    400.0 + f32::from(i) * 90.0,
                    300.0 + f32::from(i) * 8.0,
                    45.0,
                )
            })
            .collect();
        assert_eq!(lay_out(&reqs, bounds()), lay_out(&reqs, bounds()));
    }

    /// The bend is the only non-horizontal segment, and it exists only when the
    /// caption was actually displaced.
    #[test]
    fn only_the_bend_is_ever_diagonal() {
        let reqs = [cloud(700.0, 400.0, 40.0), cloud(700.0, 404.0, 40.0)];
        for c in placed(&lay_out(&reqs, bounds())) {
            assert!(
                (c.leader[0].y - c.leader[1].y).abs() < 0.01,
                "{:?}",
                c.leader
            );
            assert!(
                (c.leader[2].y - c.leader[3].y).abs() < 0.01,
                "{:?}",
                c.leader
            );
            assert!((c.leader[2].y - c.label.center().y).abs() < 0.01);
        }
    }

    /// A lobed territory: a wide box, but narrow ink at the anchor's height.
    /// The leader has to start on the ink and still clear the box before it
    /// bends, or the dot floats in open map with a gap behind it.
    #[test]
    fn a_lobed_cloud_attaches_to_ink_and_still_clears_its_box() {
        let req = Request {
            // Two lobes 400 px apart; the anchor is over the left one.
            cloud: Rect::from_min_max(Pos2::new(500.0, 340.0), Pos2::new(900.0, 460.0)),
            at: 360.0,
            span: (500.0, 580.0),
            size: Vec2::new(150.0, 44.0),
        };
        let c = lay_out(&[req], bounds())[0].expect("placed");
        assert_eq!(c.side, Side::Right);
        assert!(
            (c.leader[0].x - 580.0).abs() < 0.01,
            "the leader did not start on the silhouette: {:?}",
            c.leader
        );
        assert!(
            c.leader[2].x >= 900.0 - 0.01,
            "the leader bent before it was clear of the cloud: {:?}",
            c.leader
        );
        assert!(
            c.label.min.x >= 900.0,
            "the caption sits on its own cloud: {:?}",
            c.label
        );
    }

    /// The stack's contract on its own: boxes come out in the order they went
    /// in, separated, and inside the range.
    #[test]
    fn the_stack_separates_and_orders_and_clamps() {
        let wanted = [10.0, 12.0, 14.0, 500.0];
        let heights = [30.0, 30.0, 30.0, 30.0];
        let tops = stack(&wanted, &heights, 0.0, 400.0);
        for (k, pair) in tops.windows(2).enumerate() {
            assert!(
                pair[1] - (pair[0] + heights[k]) >= LANE - 0.01,
                "{tops:?} is not separated"
            );
        }
        assert!(tops[0] >= -0.01, "{tops:?} left the top");
        assert!(tops[3] + heights[3] <= 400.01, "{tops:?} left the bottom");
    }

    /// A column with room for everything leaves everything exactly where it
    /// wanted to be. The de-overlap must not be a cost the uncrowded case pays.
    #[test]
    fn the_stack_moves_nothing_that_does_not_collide() {
        let wanted = [100.0, 300.0, 500.0];
        let heights = [40.0, 40.0, 40.0];
        let tops = stack(&wanted, &heights, 0.0, 800.0);
        for (t, w) in tops.iter().zip(wanted) {
            assert!(
                (t + 20.0 - w).abs() < 0.01,
                "{tops:?} drifted from {wanted:?}"
            );
        }
    }
}
