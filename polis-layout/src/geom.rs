//! `f64` planar geometry for the middle of the pipeline.
//!
//! # Why a second geometry type
//!
//! [`crate::Point`] and [`crate::Polygon`] are `f32`: they are the cross-crate
//! contract, and `f32` is what the renderer uploads. The *interior* of the
//! pipeline cannot be `f32`.
//!
//! The road network is the boundary network of the settled ground — a Voronoi
//! diagram built by half-plane clipping (see [`crate::voronoi`]). Two adjacent
//! cells compute the same corner from the same two bisector equations but in a
//! different clip order, and the corner is only welded into one road junction
//! if the two computations land within the weld tolerance of each other. Near-
//! cocircular sites — four plots almost on a common circle, which accretion
//! produces constantly because it settles plots at a fixed minimum separation —
//! push that disagreement up by orders of magnitude. In `f32` the disagreement
//! exceeds the weld tolerance, the corner fails to weld, and the graph silently
//! loses the cycle that the whole design exists to produce.
//!
//! So: **half-plane clipping and everything feeding it stays `f64`, and the
//! result is quantised to the `f32` grid only at stage boundaries** (ADR-0053).
//! [`qp`] is that boundary — it runs [`crate::determinism::quantize_f64`] on
//! both coordinates, so a value that reaches the output has already been
//! snapped to the same 0.001 grid the snapshot prints at.

// The middle of the pipeline is numeric geometry, and five lint families fire on
// nearly every line of it without telling us anything:
//
// * the `cast_*` family — every cast here lands in a bucket index or a quantised
//   sort key that is clamped or wrapped on purpose;
// * `float_cmp` — exact float comparison is how a determinism tie is broken
//   (PRD §7.4), and an approximate comparison there would be the bug;
// * `many_single_char_names` and `similar_names` — `a`, `b`, `c`, `n`, `p` are
//   the names the geometry itself uses;
// * `too_many_lines` — a pipeline stage read as one ordered sequence is clearer
//   than the same code cut into fragments each called once;
// * `assigning_clones` — the buffers reassigned here are rebuilt from scratch,
//   so `clone_from` would save nothing.
#![allow(
    clippy::assigning_clones,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::float_cmp,
    clippy::many_single_char_names,
    clippy::similar_names,
    clippy::too_many_lines
)]

use std::collections::BTreeMap;

use crate::determinism::{narrow, quantize_f64};
use crate::{Point, Polygon};

/// A planar point in the pipeline's internal `f64` space.
pub(crate) type Pt = [f64; 2];

/// `a - b`.
#[inline]
pub(crate) fn sub(a: Pt, b: Pt) -> Pt {
    [a[0] - b[0], a[1] - b[1]]
}

/// `a + b`.
#[inline]
pub(crate) fn add(a: Pt, b: Pt) -> Pt {
    [a[0] + b[0], a[1] + b[1]]
}

/// `a * s`.
#[inline]
pub(crate) fn mul(a: Pt, s: f64) -> Pt {
    [a[0] * s, a[1] * s]
}

/// Dot product.
#[inline]
pub(crate) fn dot(a: Pt, b: Pt) -> f64 {
    a[0] * b[0] + a[1] * b[1]
}

/// Two-dimensional cross product (the `z` of the 3-D one).
#[inline]
pub(crate) fn cross(a: Pt, b: Pt) -> f64 {
    a[0] * b[1] - a[1] * b[0]
}

/// Squared length.
#[inline]
pub(crate) fn len2(a: Pt) -> f64 {
    dot(a, a)
}

/// Length.
#[inline]
pub(crate) fn len(a: Pt) -> f64 {
    len2(a).sqrt()
}

/// Distance between two points.
#[inline]
pub(crate) fn dist(a: Pt, b: Pt) -> f64 {
    len(sub(a, b))
}

/// Squared distance between two points.
#[inline]
pub(crate) fn dist2(a: Pt, b: Pt) -> f64 {
    len2(sub(a, b))
}

/// Unit vector, or the zero vector for a degenerate input.
#[inline]
pub(crate) fn norm(a: Pt) -> Pt {
    let l = len(a);
    if l > 1e-12 {
        [a[0] / l, a[1] / l]
    } else {
        [0.0, 0.0]
    }
}

/// Left-hand perpendicular.
#[inline]
pub(crate) fn perp(a: Pt) -> Pt {
    [-a[1], a[0]]
}

/// The stage boundary: quantise both coordinates onto the snapshot grid.
#[inline]
pub(crate) fn qp(a: Pt) -> Pt {
    [quantize_f64(a[0]), quantize_f64(a[1])]
}

/// Linear interpolation.
#[inline]
pub(crate) fn lerp(a: Pt, b: Pt, t: f64) -> Pt {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]
}

/// Quantise, then narrow to the `f32` public [`Point`].
#[inline]
pub(crate) fn to_point(a: Pt) -> Point {
    Point::new(narrow(quantize_f64(a[0])), narrow(quantize_f64(a[1])))
}

/// The `f64` view of a public [`Point`].
#[inline]
pub(crate) fn from_point(p: Point) -> Pt {
    [f64::from(p.x), f64::from(p.y)]
}

/// Quantise a ring and narrow it to the public [`Polygon`].
pub(crate) fn to_polygon(ring: &[Pt]) -> Polygon {
    Polygon::new(ring.iter().copied().map(to_point).collect())
}

/// The `f64` view of a public [`Polygon`].
pub(crate) fn from_polygon(poly: &Polygon) -> Vec<Pt> {
    poly.vertices.iter().copied().map(from_point).collect()
}

/// Twice the signed area. Positive for counter-clockwise winding.
pub(crate) fn signed_area2(poly: &[Pt]) -> f64 {
    let n = poly.len();
    if n < 3 {
        return 0.0;
    }
    let mut acc = 0.0;
    for i in 0..n {
        let a = poly[i];
        let b = poly[(i + 1) % n];
        acc += a[0] * b[1] - b[0] * a[1];
    }
    acc
}

/// Unsigned area.
pub(crate) fn area(poly: &[Pt]) -> f64 {
    signed_area2(poly).abs() * 0.5
}

/// Area-weighted centroid, falling back to the vertex mean when degenerate.
pub(crate) fn centroid(poly: &[Pt]) -> Pt {
    let n = poly.len();
    if n == 0 {
        return [0.0, 0.0];
    }
    let mean = || {
        let mut c = [0.0, 0.0];
        for p in poly {
            c = add(c, *p);
        }
        mul(c, 1.0 / n as f64)
    };
    if n < 3 {
        return mean();
    }
    let a2 = signed_area2(poly);
    if a2.abs() < 1e-12 {
        return mean();
    }
    let mut cx = 0.0;
    let mut cy = 0.0;
    for i in 0..n {
        let a = poly[i];
        let b = poly[(i + 1) % n];
        let f = a[0] * b[1] - b[0] * a[1];
        cx += (a[0] + b[0]) * f;
        cy += (a[1] + b[1]) * f;
    }
    [cx / (3.0 * a2), cy / (3.0 * a2)]
}

/// Axis-aligned bounds of a point set. Infinite for an empty one.
pub(crate) fn bounds(pts: &[Pt]) -> (Pt, Pt) {
    let mut lo = [f64::INFINITY, f64::INFINITY];
    let mut hi = [f64::NEG_INFINITY, f64::NEG_INFINITY];
    for p in pts {
        lo[0] = lo[0].min(p[0]);
        lo[1] = lo[1].min(p[1]);
        hi[0] = hi[0].max(p[0]);
        hi[1] = hi[1].max(p[1]);
    }
    (lo, hi)
}

/// Winding-number point-in-polygon. Correct for non-convex rings.
pub(crate) fn contains(poly: &[Pt], p: Pt) -> bool {
    let n = poly.len();
    if n < 3 {
        return false;
    }
    let mut wn = 0i32;
    for i in 0..n {
        let a = poly[i];
        let b = poly[(i + 1) % n];
        if a[1] <= p[1] {
            if b[1] > p[1] && cross(sub(b, a), sub(p, a)) > 0.0 {
                wn += 1;
            }
        } else if b[1] <= p[1] && cross(sub(b, a), sub(p, a)) < 0.0 {
            wn -= 1;
        }
    }
    wn != 0
}

/// Clip a convex ring by the half-plane `dot(n, x) <= c` (Sutherland–Hodgman).
///
/// This is the primitive the whole road substrate rests on, and it is the one
/// that must stay `f64` — see the module documentation.
pub(crate) fn clip_halfplane(poly: &[Pt], n: Pt, c: f64) -> Vec<Pt> {
    let m = poly.len();
    if m == 0 {
        return Vec::new();
    }
    let mut out: Vec<Pt> = Vec::with_capacity(m + 2);
    for i in 0..m {
        let a = poly[i];
        let b = poly[(i + 1) % m];
        let da = dot(n, a) - c;
        let db = dot(n, b) - c;
        let ain = da <= 0.0;
        let bin = db <= 0.0;
        if ain {
            out.push(a);
        }
        if ain != bin {
            let t = da / (da - db);
            if t.is_finite() {
                out.push(lerp(a, b, t.clamp(0.0, 1.0)));
            }
        }
    }
    dedupe_ring(out, 1e-9)
}

/// Drop consecutive (and wrap-around) duplicate vertices.
pub(crate) fn dedupe_ring(mut v: Vec<Pt>, eps: f64) -> Vec<Pt> {
    if v.len() < 2 {
        return v;
    }
    let e2 = eps * eps;
    let mut out: Vec<Pt> = Vec::with_capacity(v.len());
    for p in v.drain(..) {
        if out.last().is_none_or(|q| dist2(*q, p) > e2) {
            out.push(p);
        }
    }
    while out.len() > 1 && dist2(out[0], *out.last().expect("non-empty")) <= e2 {
        out.pop();
    }
    out
}

/// True when the ring never changes turn direction.
pub(crate) fn is_convex(poly: &[Pt]) -> bool {
    let n = poly.len();
    if n < 4 {
        return true;
    }
    let mut sign = 0i32;
    for i in 0..n {
        let a = poly[i];
        let b = poly[(i + 1) % n];
        let c = poly[(i + 2) % n];
        let z = cross(sub(b, a), sub(c, b));
        if z.abs() < 1e-12 {
            continue;
        }
        let s = if z > 0.0 { 1 } else { -1 };
        if sign == 0 {
            sign = s;
        } else if sign != s {
            return false;
        }
    }
    true
}

/// How far from the cut a vertex may be and still count as on it.
const ON_LINE_EPS: f64 = 1e-12;

/// Split a simple ring by the line `dot(n, x) = c`.
///
/// Returns `(pieces on the negative side, pieces on the positive side)`.
/// Sutherland–Hodgman is exact for a convex subject and much cheaper, so the
/// general re-walk below only runs for genuinely non-convex rings — which is
/// what a pruned block is.
pub(crate) fn split_ring(poly: &[Pt], n: Pt, c: f64) -> (Vec<Vec<Pt>>, Vec<Vec<Pt>>) {
    if is_convex(poly) {
        let neg = clip_halfplane(poly, n, c);
        let pos = clip_halfplane(poly, [-n[0], -n[1]], -c);
        let mut a = Vec::new();
        let mut b = Vec::new();
        if neg.len() >= 3 {
            a.push(neg);
        }
        if pos.len() >= 3 {
            b.push(pos);
        }
        return (a, b);
    }
    // 1. The ring with every crossing materialised as a vertex.
    let m = poly.len();
    let mut aug: Vec<(Pt, f64)> = Vec::with_capacity(m * 2);
    for i in 0..m {
        let a = poly[i];
        let b = poly[(i + 1) % m];
        let da = dot(n, a) - c;
        let db = dot(n, b) - c;
        aug.push((a, if da.abs() < ON_LINE_EPS { 0.0 } else { da }));
        if (da < -ON_LINE_EPS && db > ON_LINE_EPS) || (da > ON_LINE_EPS && db < -ON_LINE_EPS) {
            let t = da / (da - db);
            aug.push((lerp(a, b, t.clamp(0.0, 1.0)), 0.0));
        }
    }
    let k = aug.len();
    // 2. The crossings, ordered along the cut. Consecutive pairs bound the
    //    intervals of the line that lie **inside** the ring, and those intervals
    //    are the closing segments for both sides.
    let dirv = perp(n);
    let mut on: Vec<usize> = (0..k).filter(|&i| aug[i].1 == 0.0).collect();
    if on.len() < 2 || !on.len().is_multiple_of(2) {
        // A tangent cut: nothing was actually divided.
        return (vec![poly.to_vec()], Vec::new());
    }
    on.sort_by(|&i, &j| {
        dot(dirv, aug[i].0)
            .total_cmp(&dot(dirv, aug[j].0))
            .then_with(|| i.cmp(&j))
    });
    let mut partner = vec![usize::MAX; k];
    for pair in on.chunks(2) {
        partner[pair[0]] = pair[1];
        partner[pair[1]] = pair[0];
    }

    let mut neg: Vec<Vec<Pt>> = Vec::new();
    let mut pos: Vec<Vec<Pt>> = Vec::new();
    for sign in [-1.0f64, 1.0] {
        let inside = |i: usize| aug[i].1 * sign <= 0.0;
        let Some(first_out) = (0..k).find(|&i| !inside(i)) else {
            // Every vertex is on this side: the cut did not divide anything.
            let whole = dedupe_ring(aug.iter().map(|(p, _)| *p).collect(), 1e-9);
            if whole.len() >= 3 {
                if sign < 0.0 {
                    neg.push(whole);
                } else {
                    pos.push(whole);
                }
            }
            continue;
        };
        // 3. Maximal runs of vertices on this side. Each begins and ends on the
        //    cut, because a run can only be entered and left by crossing it.
        let mut chains: Vec<Vec<usize>> = Vec::new();
        let mut current: Vec<usize> = Vec::new();
        for step in 1..=k {
            let i = (first_out + step) % k;
            if inside(i) {
                current.push(i);
            } else if !current.is_empty() {
                chains.push(std::mem::take(&mut current));
            }
        }
        if !current.is_empty() {
            chains.push(current);
        }
        // 4. Stitch chain ends to chain starts along the inside intervals.
        let mut starts: BTreeMap<usize, usize> = BTreeMap::new();
        for (ci, chain) in chains.iter().enumerate() {
            starts.insert(chain[0], ci);
        }
        let mut used = vec![false; chains.len()];
        for ci in 0..chains.len() {
            if used[ci] {
                continue;
            }
            let mut ring: Vec<Pt> = Vec::new();
            let mut cur = ci;
            for _ in 0..=chains.len() {
                used[cur] = true;
                for &vi in &chains[cur] {
                    ring.push(aug[vi].0);
                }
                let end = *chains[cur].last().expect("a chain is never empty");
                let p = partner[end];
                if p == usize::MAX {
                    break;
                }
                match starts.get(&p) {
                    Some(&next) if !used[next] => cur = next,
                    _ => break,
                }
            }
            let ring = dedupe_ring(ring, 1e-9);
            if ring.len() >= 3 && area(&ring) > 1e-12 {
                if sign < 0.0 {
                    neg.push(ring);
                } else {
                    pos.push(ring);
                }
            }
        }
    }
    (neg, pos)
}

/// Longest principal axis of a ring, from the inertia tensor.
///
/// This is the axis PRD §7.2 step 4 subdivides along. It is returned as a unit
/// direction with a canonical sign, so two runs agree about which way it points.
pub(crate) fn longest_axis(poly: &[Pt]) -> Pt {
    let c = centroid(poly);
    let mut sxx = 0.0;
    let mut syy = 0.0;
    let mut sxy = 0.0;
    for p in poly {
        let d = sub(*p, c);
        sxx += d[0] * d[0];
        syy += d[1] * d[1];
        sxy += d[0] * d[1];
    }
    let tr = sxx + syy;
    let det = sxx * syy - sxy * sxy;
    let disc = (tr * tr * 0.25 - det).max(0.0).sqrt();
    // Not `tr.mul_add(0.5, disc)`: a fused multiply-add rounds once where the
    // unfused pair rounds twice, and `determinism.rs` rule 5 keeps it out.
    let l1 = tr * 0.5 + disc;
    let v = if sxy.abs() > 1e-12 {
        [l1 - syy, sxy]
    } else if sxx >= syy {
        [1.0, 0.0]
    } else {
        [0.0, 1.0]
    };
    let v = norm(v);
    if len2(v) < 0.5 {
        return [1.0, 0.0];
    }
    // Canonical sign: the axis is a line, not an arrow, and the two ends must
    // not be chosen by rounding noise.
    if v[0] < 0.0 || (v[0] == 0.0 && v[1] < 0.0) {
        [-v[0], -v[1]]
    } else {
        v
    }
}

/// `(min, max)` of the ring's projection onto `d`.
pub(crate) fn extent_along(poly: &[Pt], d: Pt) -> (f64, f64) {
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for p in poly {
        let t = dot(d, *p);
        lo = lo.min(t);
        hi = hi.max(t);
    }
    (lo, hi)
}

/// Long extent over short extent, measured on the principal axes.
pub(crate) fn aspect_ratio(poly: &[Pt]) -> f64 {
    let a = longest_axis(poly);
    let b = perp(a);
    let (l0, l1) = extent_along(poly, a);
    let (s0, s1) = extent_along(poly, b);
    let l = l1 - l0;
    let s = s1 - s0;
    if s < 1e-9 || l < 1e-9 {
        return 1e9;
    }
    (l / s).max(s / l)
}

/// Distance from a point to a segment.
pub(crate) fn dist_to_seg(p: Pt, a: Pt, b: Pt) -> f64 {
    let ab = sub(b, a);
    let l = len2(ab);
    if l < 1e-18 {
        return dist(p, a);
    }
    let t = (dot(sub(p, a), ab) / l).clamp(0.0, 1.0);
    dist(p, add(a, mul(ab, t)))
}

/// Shortest distance from a point to a ring's boundary.
pub(crate) fn dist_to_boundary(poly: &[Pt], p: Pt) -> f64 {
    let n = poly.len();
    let mut best = f64::INFINITY;
    for i in 0..n {
        best = best.min(dist_to_seg(p, poly[i], poly[(i + 1) % n]));
    }
    best
}

/// Erode a ring inward by `s` by clipping with every edge's inward half-plane.
///
/// Returns an empty ring when nothing survives, which is the caller's signal
/// that the parcel is too small to build on.
pub(crate) fn erode(poly: &[Pt], s: f64) -> Vec<Pt> {
    let n = poly.len();
    if n < 3 {
        return Vec::new();
    }
    if s <= 0.0 {
        return poly.to_vec();
    }
    let ccw = signed_area2(poly) > 0.0;
    let mut lines: Vec<(Pt, f64)> = Vec::with_capacity(n);
    for i in 0..n {
        let a = poly[i];
        let b = poly[(i + 1) % n];
        let e = norm(sub(b, a));
        if len2(e) < 0.5 {
            continue;
        }
        let nm = if ccw { [e[1], -e[0]] } else { [-e[1], e[0]] };
        lines.push((nm, dot(nm, a) - s));
    }
    if lines.len() < 3 {
        return Vec::new();
    }
    let mut out = poly.to_vec();
    for (nm, c) in &lines {
        out = clip_halfplane(&out, *nm, *c);
        if out.len() < 3 {
            return Vec::new();
        }
    }
    out
}

/// Erode a ring by a per-edge distance.
///
/// The setback a parcel needs is not uniform: the edge that lies on the block
/// boundary **is** a road centre line and needs the full road half-width, while
/// an interior lot line only needs a garden fence. Eroding uniformly by the
/// larger of the two throws away most of a small parcel, which is the direct
/// cause of a city whose ground is 90 % empty.
///
/// `setback(i)` is the inset for the edge from vertex `i` to vertex `i + 1`. For
/// a non-convex ring this over-erodes, which is the safe direction: the result
/// is always contained in the true offset region.
pub(crate) fn erode_per_edge(poly: &[Pt], setback: &dyn Fn(usize) -> f64) -> Vec<Pt> {
    let n = poly.len();
    if n < 3 {
        return Vec::new();
    }
    let ccw = signed_area2(poly) > 0.0;
    let mut out = poly.to_vec();
    for i in 0..n {
        let a = poly[i];
        let b = poly[(i + 1) % n];
        let e = norm(sub(b, a));
        if len2(e) < 0.5 {
            continue;
        }
        let nm = if ccw { [e[1], -e[0]] } else { [-e[1], e[0]] };
        out = clip_halfplane(&out, nm, dot(nm, a) - setback(i));
        if out.len() < 3 {
            return Vec::new();
        }
    }
    out
}

/// Rotate a ring about a fixed point by a precomputed `(sin, cos)`.
///
/// The angle never appears here: [`crate::determinism::det_sin_cos`] produced
/// the pair on the quantised trig grid, and passing it in is what keeps an
/// unquantised transcendental out of the output (PRD §7.4).
pub(crate) fn rotate_about(poly: &[Pt], c: Pt, sn: f64, cs: f64) -> Vec<Pt> {
    poly.iter()
        .map(|p| {
            let d = sub(*p, c);
            [c[0] + d[0] * cs - d[1] * sn, c[1] + d[0] * sn + d[1] * cs]
        })
        .collect()
}

/// The counter-clockwise convex hull of a point set (Andrew's monotone chain).
///
/// Written out in `f64` rather than taken from a library because the result is
/// the city limit, which reaches the golden file.
pub(crate) fn convex_hull(pts: &[Pt]) -> Vec<Pt> {
    let mut sorted: Vec<Pt> = pts
        .iter()
        .copied()
        .filter(|p| p[0].is_finite() && p[1].is_finite())
        .collect();
    if sorted.len() < 3 {
        return sorted;
    }
    sorted.sort_by(|a, b| a[0].total_cmp(&b[0]).then_with(|| a[1].total_cmp(&b[1])));
    sorted.dedup_by(|a, b| dist2(*a, *b) == 0.0);
    if sorted.len() < 3 {
        return sorted;
    }
    let turn = |o: Pt, a: Pt, b: Pt| cross(sub(a, o), sub(b, o));
    let mut lower: Vec<Pt> = Vec::with_capacity(sorted.len());
    for &p in &sorted {
        while lower.len() >= 2 && turn(lower[lower.len() - 2], lower[lower.len() - 1], p) <= 0.0 {
            lower.pop();
        }
        lower.push(p);
    }
    let mut upper: Vec<Pt> = Vec::with_capacity(sorted.len());
    for &p in sorted.iter().rev() {
        while upper.len() >= 2 && turn(upper[upper.len() - 2], upper[upper.len() - 1], p) <= 0.0 {
            upper.pop();
        }
        upper.push(p);
    }
    lower.pop();
    upper.pop();
    lower.extend(upper);
    lower
}

/// A proper segment crossing, excluding shared endpoints.
///
/// This is the planarity check: a crossing without a node.
pub(crate) fn segments_properly_cross(a0: Pt, a1: Pt, b0: Pt, b1: Pt) -> bool {
    let eps = 1e-9;
    for x in [a0, a1] {
        for y in [b0, b1] {
            if dist2(x, y) < eps {
                return false;
            }
        }
    }
    let d1 = cross(sub(a1, a0), sub(b0, a0));
    let d2 = cross(sub(a1, a0), sub(b1, a0));
    let d3 = cross(sub(b1, b0), sub(a0, b0));
    let d4 = cross(sub(b1, b0), sub(a1, b0));
    ((d1 > eps && d2 < -eps) || (d1 < -eps && d2 > eps))
        && ((d3 > eps && d4 < -eps) || (d3 < -eps && d4 > eps))
}

/// A point provably inside a ring, maximising `min(clearance from the ring,
/// clearance from `avoid` minus ``back_off``)`.
///
/// Passing the block ring as `avoid` and the road half-width as `back_off` is
/// what makes "no building touches a road" hold **by construction**: the
/// footprint is grown outward from a point that already has the setback, and
/// the returned clearance caps how far it may grow.
pub(crate) fn interior_point_avoiding(
    ring: &[Pt],
    avoid: Option<&[Pt]>,
    back_off: f64,
) -> Option<(Pt, f64)> {
    if ring.len() < 3 {
        return None;
    }
    let score = |t: Pt| -> f64 {
        let a = dist_to_boundary(ring, t);
        match avoid {
            Some(av) => a.min(dist_to_boundary(av, t) - back_off),
            None => a,
        }
    };
    let c = centroid(ring);
    let mut best: Option<(f64, Pt)> = if contains(ring, c) {
        Some((score(c), c))
    } else {
        None
    };
    let offer = |t: Pt, best: &mut Option<(f64, Pt)>| {
        if !contains(ring, t) {
            return;
        }
        let d = score(t);
        let better = match *best {
            None => true,
            // Ties broken on the quantised coordinate so the choice is a
            // property of the geometry, not of the probe order.
            Some((bd, bp)) => d > bd || (d == bd && (qp(t)[0], qp(t)[1]) < (qp(bp)[0], qp(bp)[1])),
        };
        if better {
            *best = Some((d, t));
        }
    };
    let n = ring.len();
    for i in 0..n {
        let a = ring[i];
        let b = ring[(i + 1) % n];
        let jmax = if n > 8 { 1 } else { n };
        for jj in 0..jmax {
            let j = (i + 2 + jj) % n;
            if j == i || j == (i + 1) % n {
                continue;
            }
            offer(mul(add(add(a, b), ring[j]), 1.0 / 3.0), &mut best);
        }
        offer(lerp(mul(add(a, b), 0.5), c, 0.55), &mut best);
    }
    best.map(|(d, p)| (p, d))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]

    use super::*;

    fn square(s: f64) -> Vec<Pt> {
        vec![[0.0, 0.0], [s, 0.0], [s, s], [0.0, s]]
    }

    #[test]
    fn area_and_centroid_of_a_square() {
        let sq = square(2.0);
        assert!((area(&sq) - 4.0).abs() < 1e-12);
        let c = centroid(&sq);
        assert!((c[0] - 1.0).abs() < 1e-12 && (c[1] - 1.0).abs() < 1e-12);
        assert!(signed_area2(&sq) > 0.0, "the fixture is counter-clockwise");
    }

    #[test]
    fn half_plane_clipping_halves_a_square() {
        let sq = square(2.0);
        let half = clip_halfplane(&sq, [1.0, 0.0], 1.0);
        assert!((area(&half) - 2.0).abs() < 1e-9, "{half:?}");
    }

    #[test]
    fn splitting_a_convex_ring_conserves_area() {
        let sq = square(2.0);
        let (neg, pos) = split_ring(&sq, [1.0, 0.0], 0.75);
        let total: f64 = neg.iter().chain(pos.iter()).map(|r| area(r)).sum();
        assert!((total - 4.0).abs() < 1e-9);
        assert_eq!(neg.len(), 1);
        assert_eq!(pos.len(), 1);
    }

    #[test]
    fn splitting_a_non_convex_ring_conserves_area() {
        // An L, cut across the foot.
        let l: Vec<Pt> = vec![
            [0.0, 0.0],
            [3.0, 0.0],
            [3.0, 1.0],
            [1.0, 1.0],
            [1.0, 3.0],
            [0.0, 3.0],
        ];
        assert!(!is_convex(&l));
        let before = area(&l);
        let (neg, pos) = split_ring(&l, [0.0, 1.0], 0.5);
        let total: f64 = neg.iter().chain(pos.iter()).map(|r| area(r)).sum();
        assert!((total - before).abs() < 1e-9, "{total} vs {before}");
    }

    #[test]
    fn eroding_shrinks_and_then_vanishes() {
        let sq = square(2.0);
        let small = erode(&sq, 0.5);
        assert!((area(&small) - 1.0).abs() < 1e-9, "{small:?}");
        assert!(erode(&sq, 1.5).is_empty(), "over-erosion must vanish");
    }

    #[test]
    fn containment_and_winding() {
        let sq = square(2.0);
        assert!(contains(&sq, [1.0, 1.0]));
        assert!(!contains(&sq, [3.0, 1.0]));
        let mut reversed = sq.clone();
        reversed.reverse();
        assert!(
            contains(&reversed, [1.0, 1.0]),
            "winding number must not care about orientation"
        );
    }

    #[test]
    fn longest_axis_of_a_slab_is_its_long_side() {
        let slab: Vec<Pt> = vec![[0.0, 0.0], [8.0, 0.0], [8.0, 1.0], [0.0, 1.0]];
        let a = longest_axis(&slab);
        assert!(a[0].abs() > 0.99, "{a:?}");
        assert!(aspect_ratio(&slab) > 7.0);
    }

    #[test]
    fn the_axis_sign_is_canonical() {
        let slab: Vec<Pt> = vec![[0.0, 0.0], [8.0, 0.0], [8.0, 1.0], [0.0, 1.0]];
        let mut flipped = slab.clone();
        flipped.reverse();
        assert_eq!(longest_axis(&slab), longest_axis(&flipped));
    }

    #[test]
    fn proper_crossings_exclude_shared_endpoints() {
        assert!(segments_properly_cross(
            [0.0, 0.0],
            [2.0, 2.0],
            [0.0, 2.0],
            [2.0, 0.0]
        ));
        assert!(!segments_properly_cross(
            [0.0, 0.0],
            [2.0, 2.0],
            [0.0, 0.0],
            [2.0, 0.0]
        ));
    }

    #[test]
    fn an_interior_point_keeps_the_road_setback() {
        let block = square(6.0);
        let lot: Vec<Pt> = vec![[0.0, 0.0], [3.0, 0.0], [3.0, 3.0], [0.0, 3.0]];
        let (p, clear) = interior_point_avoiding(&lot, Some(&block), 0.5).expect("an interior");
        assert!(contains(&lot, p));
        assert!(dist_to_boundary(&block, p) >= 0.5 + clear - 1e-9);
        assert!(clear > 0.0);
    }

    #[test]
    fn quantisation_is_idempotent_at_the_stage_boundary() {
        let p = [1.234_567_89, -9.876_543_21];
        assert_eq!(qp(qp(p)), qp(p));
        let back = from_point(to_point(p));
        assert!(dist(back, qp(p)) < 1e-6, "{back:?}");
    }
}
