//! Polygon geometry. Everything is f64 internally and quantised at the
//! boundaries via `polis_layout::determinism::quantize_f64`, so that no
//! unquantised transcendental ever reaches the layout output (PRD §7.4).

use polis_layout::determinism::quantize_f64;

pub type P = [f64; 2];

#[inline]
pub fn sub(a: P, b: P) -> P {
    [a[0] - b[0], a[1] - b[1]]
}
#[inline]
pub fn add(a: P, b: P) -> P {
    [a[0] + b[0], a[1] + b[1]]
}
#[inline]
pub fn mul(a: P, s: f64) -> P {
    [a[0] * s, a[1] * s]
}
#[inline]
pub fn dot(a: P, b: P) -> f64 {
    a[0] * b[0] + a[1] * b[1]
}
#[inline]
pub fn cross(a: P, b: P) -> f64 {
    a[0] * b[1] - a[1] * b[0]
}
#[inline]
pub fn len2(a: P) -> f64 {
    dot(a, a)
}
#[inline]
pub fn len(a: P) -> f64 {
    len2(a).sqrt()
}
#[inline]
pub fn dist(a: P, b: P) -> f64 {
    len(sub(a, b))
}
#[inline]
pub fn dist2(a: P, b: P) -> f64 {
    len2(sub(a, b))
}
#[inline]
pub fn norm(a: P) -> P {
    let l = len(a);
    if l > 1e-12 {
        [a[0] / l, a[1] / l]
    } else {
        [0.0, 0.0]
    }
}
#[inline]
pub fn perp(a: P) -> P {
    [-a[1], a[0]]
}
#[inline]
pub fn qp(a: P) -> P {
    [quantize_f64(a[0]), quantize_f64(a[1])]
}
#[inline]
pub fn lerp(a: P, b: P, t: f64) -> P {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]
}

/// Twice the signed area. Positive for counter-clockwise winding.
pub fn signed_area2(poly: &[P]) -> f64 {
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

pub fn area(poly: &[P]) -> f64 {
    signed_area2(poly).abs() * 0.5
}

pub fn centroid(poly: &[P]) -> P {
    let n = poly.len();
    if n == 0 {
        return [0.0, 0.0];
    }
    if n < 3 {
        let mut c = [0.0, 0.0];
        for p in poly {
            c = add(c, *p);
        }
        return mul(c, 1.0 / n as f64);
    }
    let a2 = signed_area2(poly);
    if a2.abs() < 1e-12 {
        let mut c = [0.0, 0.0];
        for p in poly {
            c = add(c, *p);
        }
        return mul(c, 1.0 / n as f64);
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

pub fn perimeter(poly: &[P]) -> f64 {
    let n = poly.len();
    if n < 2 {
        return 0.0;
    }
    (0..n).map(|i| dist(poly[i], poly[(i + 1) % n])).sum()
}

pub fn bounds(poly: &[P]) -> (P, P) {
    let mut lo = [f64::INFINITY, f64::INFINITY];
    let mut hi = [f64::NEG_INFINITY, f64::NEG_INFINITY];
    for p in poly {
        lo[0] = lo[0].min(p[0]);
        lo[1] = lo[1].min(p[1]);
        hi[0] = hi[0].max(p[0]);
        hi[1] = hi[1].max(p[1]);
    }
    (lo, hi)
}

/// Winding-number point-in-polygon. Robust for non-convex rings.
pub fn contains(poly: &[P], p: P) -> bool {
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

/// Clip a convex-or-not ring by the half-plane `dot(n, x) <= c`
/// (Sutherland-Hodgman). Correct for convex subjects, which is all we feed it.
pub fn clip_halfplane(poly: &[P], n: P, c: f64) -> Vec<P> {
    let m = poly.len();
    if m == 0 {
        return Vec::new();
    }
    let mut out: Vec<P> = Vec::with_capacity(m + 2);
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

pub fn dedupe_ring(mut v: Vec<P>, eps: f64) -> Vec<P> {
    if v.len() < 2 {
        return v;
    }
    let e2 = eps * eps;
    let mut out: Vec<P> = Vec::with_capacity(v.len());
    for p in v.drain(..) {
        if out.last().is_none_or(|q| dist2(*q, p) > e2) {
            out.push(p);
        }
    }
    while out.len() > 1 && dist2(out[0], *out.last().unwrap()) <= e2 {
        out.pop();
    }
    out
}

/// Split a simple ring by the line `dot(n, x) = c` into the pieces on each
/// side. Handles non-convex rings with any number of crossings by collecting
/// the crossings, sorting them along the line and re-walking the boundary.
///
/// Returns `(negative_side_pieces, positive_side_pieces)`.
pub fn split_ring(poly: &[P], n: P, c: f64) -> (Vec<Vec<P>>, Vec<Vec<P>>) {
    // The general algorithm is only needed for genuinely non-convex rings;
    // Sutherland-Hodgman is exact for convex ones and much cheaper.
    if is_convex(poly) {
        let neg = clip_halfplane(poly, n, c);
        let pn = [-n[0], -n[1]];
        let pos = clip_halfplane(poly, pn, -c);
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
    // General case: build an augmented ring with the crossing points inserted,
    // then walk it, jumping between crossings along the cut line.
    let m = poly.len();
    let mut aug: Vec<(P, f64)> = Vec::with_capacity(m * 2);
    for i in 0..m {
        let a = poly[i];
        let b = poly[(i + 1) % m];
        let da = dot(n, a) - c;
        let db = dot(n, b) - c;
        aug.push((a, da));
        if (da < 0.0 && db > 0.0) || (da > 0.0 && db < 0.0) {
            let t = da / (da - db);
            let x = lerp(a, b, t.clamp(0.0, 1.0));
            aug.push((x, 0.0));
        }
    }
    let k = aug.len();
    // Index the on-line vertices, ordered along the cut direction.
    let dirv = perp(n);
    let mut on: Vec<usize> = (0..k).filter(|&i| aug[i].1.abs() < 1e-12).collect();
    on.sort_by(|&i, &j| {
        let a = dot(dirv, aug[i].0);
        let b = dot(dirv, aug[j].0);
        a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal)
    });
    if on.len() < 2 || on.len() % 2 != 0 {
        // Degenerate (tangent) cut: give up and return the whole ring.
        return (vec![poly.to_vec()], Vec::new());
    }
    // Pair consecutive on-line vertices; that pairing is the set of chords.
    let mut mate = vec![usize::MAX; k];
    for pair in on.chunks(2) {
        mate[pair[0]] = pair[1];
        mate[pair[1]] = pair[0];
    }
    let mut used = vec![false; k];
    let mut neg = Vec::new();
    let mut pos = Vec::new();
    for start in 0..k {
        if used[start] {
            continue;
        }
        let mut ring: Vec<P> = Vec::new();
        let mut i = start;
        let mut guard = 0;
        let mut side = 0.0f64;
        loop {
            guard += 1;
            if guard > 4 * k + 8 {
                ring.clear();
                break;
            }
            if used[i] {
                break;
            }
            used[i] = true;
            ring.push(aug[i].0);
            if side == 0.0 && aug[i].1.abs() >= 1e-12 {
                side = aug[i].1;
            }
            let nxt = (i + 1) % k;
            // At a chord endpoint whose successor leaves our side, jump.
            if mate[i] != usize::MAX && !ring.is_empty() {
                let sn = aug[nxt].1;
                if (side < 0.0 && sn > 1e-12) || (side > 0.0 && sn < -1e-12) {
                    let j = mate[i];
                    if !used[j] {
                        ring.push(aug[j].0);
                        used[j] = true;
                        i = (j + 1) % k;
                        continue;
                    }
                    break;
                }
            }
            i = nxt;
            if i == start {
                break;
            }
        }
        let ring = dedupe_ring(ring, 1e-9);
        if ring.len() >= 3 && area(&ring) > 1e-9 {
            let cen = centroid(&ring);
            if dot(n, cen) - c <= 0.0 {
                neg.push(ring);
            } else {
                pos.push(ring);
            }
        }
    }
    (neg, pos)
}

pub fn is_convex(poly: &[P]) -> bool {
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

/// Longest principal axis of a ring, via the inertia tensor. Returns a unit
/// direction. This is the axis PRD §7.2 step 4 subdivides along.
pub fn longest_axis(poly: &[P]) -> P {
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
    // Principal direction of the 2x2 symmetric matrix [[sxx,sxy],[sxy,syy]].
    let tr = sxx + syy;
    let det = sxx * syy - sxy * sxy;
    let disc = (tr * tr * 0.25 - det).max(0.0).sqrt();
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
        [1.0, 0.0]
    } else {
        v
    }
}

/// Extent of a ring along a direction: (min, max) of the projection.
pub fn extent_along(poly: &[P], d: P) -> (f64, f64) {
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for p in poly {
        let t = dot(d, *p);
        lo = lo.min(t);
        hi = hi.max(t);
    }
    (lo, hi)
}

/// Aspect ratio: long extent over short extent, measured on the principal axes.
pub fn aspect_ratio(poly: &[P]) -> f64 {
    let a = longest_axis(poly);
    let b = perp(a);
    let (l0, l1) = extent_along(poly, a);
    let (s0, s1) = extent_along(poly, b);
    let l = l1 - l0;
    let s = s1 - s0;
    if s < 1e-9 {
        return 1e9;
    }
    (l / s).max(s / l)
}

/// Shortest distance from a point to a ring's boundary.
pub fn dist_to_boundary(poly: &[P], p: P) -> f64 {
    let n = poly.len();
    let mut best = f64::INFINITY;
    for i in 0..n {
        best = best.min(dist_to_seg(p, poly[i], poly[(i + 1) % n]));
    }
    best
}

pub fn dist_to_seg(p: P, a: P, b: P) -> f64 {
    let ab = sub(b, a);
    let l = len2(ab);
    if l < 1e-18 {
        return dist(p, a);
    }
    let t = (dot(sub(p, a), ab) / l).clamp(0.0, 1.0);
    dist(p, add(a, mul(ab, t)))
}

/// Erode a ring inward by `s`, by moving every edge inward along its inward
/// normal and re-intersecting consecutive edges. Falls back to a centroid
/// scale when the naive offset self-intersects.
pub fn erode(poly: &[P], s: f64) -> Vec<P> {
    let n = poly.len();
    if n < 3 || s <= 0.0 {
        return poly.to_vec();
    }
    let ccw = signed_area2(poly) > 0.0;
    let mut lines: Vec<(P, f64)> = Vec::with_capacity(n);
    for i in 0..n {
        let a = poly[i];
        let b = poly[(i + 1) % n];
        let e = norm(sub(b, a));
        if len2(e) < 0.5 {
            continue;
        }
        // Outward normal.
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

/// Scale a ring about a fixed point.
pub fn scale_about(poly: &[P], c: P, k: f64) -> Vec<P> {
    poly.iter()
        .map(|p| [c[0] + (p[0] - c[0]) * k, c[1] + (p[1] - c[1]) * k])
        .collect()
}

/// Rotate a ring about a fixed point by a precomputed (sin, cos).
pub fn rotate_about(poly: &[P], c: P, sn: f64, cs: f64) -> Vec<P> {
    poly.iter()
        .map(|p| {
            let d = sub(*p, c);
            [c[0] + d[0] * cs - d[1] * sn, c[1] + d[0] * sn + d[1] * cs]
        })
        .collect()
}

/// Proper segment intersection test, excluding shared endpoints. This is the
/// planarity check: a crossing without a node.
pub fn segments_properly_cross(a0: P, a1: P, b0: P, b1: P) -> bool {
    let eps = 1e-9;
    // Shared endpoints are legal in a planar graph.
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
