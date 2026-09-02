//! Deterministic polygon utilities: area, clipping, offsetting, subdivision.
//!
//! Every value that reaches a layout output is quantised through
//! `polis_layout::determinism::quantize_f64` before it is stored, so no
//! unquantised transcendental ever lands in the city (PRD §7.4).

use polis_layout::determinism as det;

pub type Pt = crate::raster::P2;

pub fn pt(x: f64, y: f64) -> Pt {
    Pt::new(x, y)
}

pub fn sub(a: Pt, b: Pt) -> Pt {
    pt(a.x - b.x, a.y - b.y)
}
pub fn add(a: Pt, b: Pt) -> Pt {
    pt(a.x + b.x, a.y + b.y)
}
pub fn scale(a: Pt, s: f64) -> Pt {
    pt(a.x * s, a.y * s)
}
pub fn dot(a: Pt, b: Pt) -> f64 {
    a.x * b.x + a.y * b.y
}
pub fn cross(a: Pt, b: Pt) -> f64 {
    a.x * b.y - a.y * b.x
}
pub fn len(a: Pt) -> f64 {
    (a.x * a.x + a.y * a.y).sqrt()
}
pub fn dist(a: Pt, b: Pt) -> f64 {
    len(sub(a, b))
}
pub fn dist2(a: Pt, b: Pt) -> f64 {
    let d = sub(a, b);
    d.x * d.x + d.y * d.y
}
pub fn norm(a: Pt) -> Pt {
    let l = len(a);
    if l < 1e-12 {
        pt(1.0, 0.0)
    } else {
        scale(a, 1.0 / l)
    }
}
pub fn q(p: Pt) -> Pt {
    pt(det::quantize_f64(p.x), det::quantize_f64(p.y))
}

pub fn signed_area(poly: &[Pt]) -> f64 {
    let mut a = 0.0;
    for i in 0..poly.len() {
        let p = poly[i];
        let n = poly[(i + 1) % poly.len()];
        a += p.x * n.y - n.x * p.y;
    }
    a * 0.5
}

pub fn area(poly: &[Pt]) -> f64 {
    signed_area(poly).abs()
}

pub fn centroid(poly: &[Pt]) -> Pt {
    let a = signed_area(poly);
    if a.abs() < 1e-12 {
        let n = poly.len().max(1) as f64;
        let mut s = pt(0.0, 0.0);
        for p in poly {
            s = add(s, *p);
        }
        return scale(s, 1.0 / n);
    }
    let (mut cx, mut cy) = (0.0, 0.0);
    for i in 0..poly.len() {
        let p = poly[i];
        let n = poly[(i + 1) % poly.len()];
        let f = p.x * n.y - n.x * p.y;
        cx += (p.x + n.x) * f;
        cy += (p.y + n.y) * f;
    }
    pt(cx / (6.0 * a), cy / (6.0 * a))
}

pub fn make_ccw(poly: &mut Vec<Pt>) {
    if signed_area(poly) < 0.0 {
        poly.reverse();
    }
}

pub fn contains(poly: &[Pt], p: Pt) -> bool {
    let mut inside = false;
    let n = poly.len();
    for i in 0..n {
        let a = poly[i];
        let b = poly[(i + 1) % n];
        if (a.y > p.y) != (b.y > p.y) {
            let t = (p.y - a.y) / (b.y - a.y);
            if p.x < a.x + (b.x - a.x) * t {
                inside = !inside;
            }
        }
    }
    inside
}

/// Longest chord direction and the perpendicular extent, used to pick a split axis.
pub fn long_axis(poly: &[Pt]) -> (Pt, f64) {
    let mut best = (pt(1.0, 0.0), 0.0f64);
    for i in 0..poly.len() {
        for j in (i + 1)..poly.len() {
            let d = dist2(poly[i], poly[j]);
            if d > best.1 {
                best = (norm(sub(poly[j], poly[i])), d);
            }
        }
    }
    (best.0, best.1.sqrt())
}

/// Aspect ratio via the rotating-calipers minimum-area bounding box.
pub fn aspect_ratio(poly: &[Pt]) -> f64 {
    if poly.len() < 3 {
        return 999.0;
    }
    let mut best = f64::MAX;
    let mut best_ratio = 1.0;
    for i in 0..poly.len() {
        let a = poly[i];
        let b = poly[(i + 1) % poly.len()];
        let e = sub(b, a);
        if len(e) < 1e-9 {
            continue;
        }
        let u = norm(e);
        let v = pt(-u.y, u.x);
        let (mut lo_u, mut hi_u) = (f64::MAX, f64::MIN);
        let (mut lo_v, mut hi_v) = (f64::MAX, f64::MIN);
        for p in poly {
            let du = dot(*p, u);
            let dv = dot(*p, v);
            lo_u = lo_u.min(du);
            hi_u = hi_u.max(du);
            lo_v = lo_v.min(dv);
            hi_v = hi_v.max(dv);
        }
        let (w, h) = (hi_u - lo_u, hi_v - lo_v);
        let a2 = w * h;
        if a2 < best {
            best = a2;
            best_ratio = if w >= h {
                w / h.max(1e-9)
            } else {
                h / w.max(1e-9)
            };
        }
    }
    best_ratio.min(999.0)
}

/// Sutherland-Hodgman clip against the half-plane `dot(p, n) <= c`.
pub fn clip_halfplane(poly: &[Pt], n: Pt, c: f64) -> Vec<Pt> {
    let mut out = Vec::with_capacity(poly.len() + 2);
    let m = poly.len();
    if m == 0 {
        return out;
    }
    for i in 0..m {
        let a = poly[i];
        let b = poly[(i + 1) % m];
        let da = dot(a, n) - c;
        let db = dot(b, n) - c;
        if da <= 0.0 {
            out.push(a);
        }
        if (da <= 0.0) != (db <= 0.0) {
            let t = da / (da - db);
            out.push(pt(a.x + (b.x - a.x) * t, a.y + (b.y - a.y) * t));
        }
    }
    out
}

/// Inset with per-edge distances, mitred, with a centroid-shrink fallback.
///
/// `d[i]` applies to the edge `poly[i] -> poly[i+1]`. The polygon must be CCW.
pub fn inset_variable(poly: &[Pt], d: &[f64]) -> Vec<Pt> {
    let n = poly.len();
    if n < 3 {
        return Vec::new();
    }
    let a0 = signed_area(poly);
    if a0 <= 0.0 {
        return Vec::new();
    }
    // Clip against every inward-offset edge half-plane. Robust for convex and
    // mildly concave cells, and never self-intersects.
    let mut out: Vec<Pt> = poly.to_vec();
    for i in 0..n {
        let a = poly[i];
        let b = poly[(i + 1) % n];
        let e = sub(b, a);
        if len(e) < 1e-9 {
            continue;
        }
        let u = norm(e);
        // CCW polygon: outward normal is (u.y, -u.x).
        let outward = pt(u.y, -u.x);
        let c = dot(a, outward) - d[i];
        out = clip_halfplane(&out, outward, c);
        if out.len() < 3 {
            return Vec::new();
        }
    }
    dedup(&mut out);
    if out.len() < 3 || signed_area(&out) <= 1e-9 {
        return Vec::new();
    }
    out
}

pub fn inset_uniform(poly: &[Pt], d: f64) -> Vec<Pt> {
    let ds = vec![d; poly.len()];
    inset_variable(poly, &ds)
}

pub fn dedup(poly: &mut Vec<Pt>) {
    let mut out: Vec<Pt> = Vec::with_capacity(poly.len());
    for p in poly.iter() {
        if out.last().map_or(true, |l| dist2(*l, *p) > 1e-12) {
            out.push(*p);
        }
    }
    while out.len() > 1 && dist2(out[0], *out.last().unwrap()) <= 1e-12 {
        out.pop();
    }
    *poly = out;
}

/// Scale a polygon toward its centroid.
pub fn shrink_to(poly: &[Pt], factor: f64) -> Vec<Pt> {
    let c = centroid(poly);
    poly.iter()
        .map(|p| add(c, scale(sub(*p, c), factor)))
        .collect()
}

pub fn rotate_about(poly: &[Pt], c: Pt, radians: f64) -> Vec<Pt> {
    let (s, co) = det::det_sin_cos(radians);
    poly.iter()
        .map(|p| {
            let d = sub(*p, c);
            add(c, pt(d.x * co - d.y * s, d.x * s + d.y * co))
        })
        .collect()
}

/// Douglas-Peucker with a fixed tolerance. Endpoints are preserved exactly.
pub fn simplify(pts: &[Pt], tol: f64) -> Vec<Pt> {
    if pts.len() < 3 {
        return pts.to_vec();
    }
    let mut keep = vec![false; pts.len()];
    keep[0] = true;
    keep[pts.len() - 1] = true;
    let mut stack = vec![(0usize, pts.len() - 1)];
    while let Some((i, j)) = stack.pop() {
        if j <= i + 1 {
            continue;
        }
        let (a, b) = (pts[i], pts[j]);
        let ab = sub(b, a);
        let l = len(ab);
        let mut best = (0usize, -1.0f64);
        for (k, p) in pts.iter().enumerate().take(j).skip(i + 1) {
            let d = if l < 1e-12 {
                dist(*p, a)
            } else {
                (cross(ab, sub(*p, a)) / l).abs()
            };
            if d > best.1 {
                best = (k, d);
            }
        }
        if best.1 > tol {
            keep[best.0] = true;
            stack.push((i, best.0));
            stack.push((best.0, j));
        }
    }
    pts.iter()
        .zip(keep)
        .filter(|(_, k)| *k)
        .map(|(p, _)| *p)
        .collect()
}

/// Chaikin corner cutting with fixed endpoints. Turns a simplified staircase
/// into a road that curves.
pub fn chaikin_open(pts: &[Pt], iters: u32) -> Vec<Pt> {
    let mut cur = pts.to_vec();
    for _ in 0..iters {
        if cur.len() < 3 {
            break;
        }
        let mut next = Vec::with_capacity(cur.len() * 2);
        next.push(cur[0]);
        for i in 0..cur.len() - 1 {
            let a = cur[i];
            let b = cur[i + 1];
            next.push(pt(a.x * 0.75 + b.x * 0.25, a.y * 0.75 + b.y * 0.25));
            next.push(pt(a.x * 0.25 + b.x * 0.75, a.y * 0.25 + b.y * 0.75));
        }
        next.push(cur[cur.len() - 1]);
        cur = next;
    }
    cur
}

pub fn polyline_len(pts: &[Pt]) -> f64 {
    let mut l = 0.0;
    for i in 0..pts.len().saturating_sub(1) {
        l += dist(pts[i], pts[i + 1]);
    }
    l
}

/// True when segments `p1p2` and `p3p4` cross at an interior point of both.
pub fn segments_cross(p1: Pt, p2: Pt, p3: Pt, p4: Pt) -> bool {
    let d1 = cross(sub(p4, p3), sub(p1, p3));
    let d2 = cross(sub(p4, p3), sub(p2, p3));
    let d3 = cross(sub(p2, p1), sub(p3, p1));
    let d4 = cross(sub(p2, p1), sub(p4, p1));
    const E: f64 = 1e-9;
    ((d1 > E && d2 < -E) || (d1 < -E && d2 > E)) && ((d3 > E && d4 < -E) || (d3 < -E && d4 > E))
}

/// Recursively split `poly` into `k` sub-polygons of roughly equal area.
///
/// The cut direction is the long axis, perturbed by a seeded angle so the lots
/// are irregular rather than a grid. Deterministic in `seed` alone.
pub fn subdivide(poly: &[Pt], k: usize, seed: u64) -> Vec<Vec<Pt>> {
    if k <= 1 || poly.len() < 3 {
        return vec![poly.to_vec()];
    }
    let total = area(poly);
    if total < 1e-6 {
        return vec![poly.to_vec()];
    }
    let kl = k / 2;
    let target = total * (kl as f64) / (k as f64);

    let (axis, _) = long_axis(poly);
    let mut rng = det::SeededRng::for_seed(seed, "lotsplit");
    let jitter = rng.range_f64(-0.22, 0.22);
    let (s, c) = det::det_sin_cos(jitter);
    let n = pt(axis.x * c - axis.y * s, axis.x * s + axis.y * c);

    let (mut lo, mut hi) = (f64::MAX, f64::MIN);
    for p in poly {
        let d = dot(*p, n);
        lo = lo.min(d);
        hi = hi.max(d);
    }
    let mut a = lo;
    let mut b = hi;
    let mut cut = (lo + hi) * 0.5;
    for _ in 0..22 {
        cut = (a + b) * 0.5;
        let left = clip_halfplane(poly, n, cut);
        let la = if left.len() >= 3 { area(&left) } else { 0.0 };
        if la < target {
            a = cut;
        } else {
            b = cut;
        }
    }
    let left = clip_halfplane(poly, n, cut);
    let right = clip_halfplane(poly, scale(n, -1.0), -cut);
    let (left, right) = if left.len() >= 3 && right.len() >= 3 {
        (left, right)
    } else {
        // The long axis refused; try the perpendicular before giving up.
        let n2 = pt(-n.y, n.x);
        let (mut lo2, mut hi2) = (f64::MAX, f64::MIN);
        for p in poly {
            let d = dot(*p, n2);
            lo2 = lo2.min(d);
            hi2 = hi2.max(d);
        }
        let cut2 = lo2 + (hi2 - lo2) * (kl as f64) / (k as f64);
        let l2 = clip_halfplane(poly, n2, cut2);
        let r2 = clip_halfplane(poly, scale(n2, -1.0), -cut2);
        if l2.len() >= 3 && r2.len() >= 3 {
            (l2, r2)
        } else {
            return vec![poly.to_vec()];
        }
    };
    let mut out = subdivide(&left, kl, det::combine_seeds(seed, 0x11));
    out.extend(subdivide(&right, k - kl, det::combine_seeds(seed, 0x22)));
    out
}

/// Distance from a point to a polygon's boundary.
pub fn dist_to_boundary(poly: &[Pt], p: Pt) -> f64 {
    let mut best = f64::MAX;
    for i in 0..poly.len() {
        let a = poly[i];
        let b = poly[(i + 1) % poly.len()];
        let ab = sub(b, a);
        let l2 = dot(ab, ab);
        let d = if l2 < 1e-12 {
            dist(p, a)
        } else {
            let t = (dot(sub(p, a), ab) / l2).clamp(0.0, 1.0);
            dist(p, add(a, scale(ab, t)))
        };
        best = best.min(d);
    }
    best
}

/// Inset that also works on strongly concave cells.
///
/// The half-plane intersection is exact for convex cells and collapses on
/// concave ones, so it is validated against the original area and falls back to
/// a binary-searched shape-preserving shrink. The shrink target is the *minimum*
/// clearance, so nothing built inside can reach the road.
pub fn inset_robust(poly: &[Pt], ds: &[f64]) -> Vec<Pt> {
    let a0 = area(poly);
    if a0 < 1e-6 || poly.len() < 3 {
        return Vec::new();
    }
    // 1. Mitred offset: the true inset for anything but a sharp reflex corner.
    let m = inset_miter(poly, ds);
    if m.len() >= 3 && area(&m) > 0.12 * a0 {
        return m;
    }
    // 2. Half-plane intersection: exact for a convex cell.
    let clipped = inset_variable(poly, ds);
    if clipped.len() >= 3 && area(&clipped) > 0.24 * a0 {
        return clipped;
    }
    let d = ds.iter().copied().fold(0.0f64, f64::max);
    // Shrink toward the pole of inaccessibility (the deepest interior point),
    // not the centroid: on a concave cell the centroid can sit outside, and
    // scaling toward it then pushes vertices through the boundary.
    let pole = pole_of_inaccessibility(poly);
    let (mut lo, mut hi) = (0.02f64, 1.0f64);
    let mut best: Vec<Pt> = Vec::new();
    for _ in 0..16 {
        let f = (lo + hi) * 0.5;
        let cand: Vec<Pt> = poly
            .iter()
            .map(|p| add(pole, scale(sub(*p, pole), f)))
            .collect();
        let ok = cand
            .iter()
            .all(|p| contains(poly, *p) && dist_to_boundary(poly, *p) >= d);
        if ok {
            best = cand;
            lo = f;
        } else {
            hi = f;
        }
    }
    if best.len() >= 3 && area(&best) > 1e-6 {
        best
    } else {
        Vec::new()
    }
}

/// The interior point farthest from the boundary, found by grid refinement.
/// Deterministic: a fixed lattice, a fixed number of refinements.
pub fn pole_of_inaccessibility(poly: &[Pt]) -> Pt {
    let (mut lo, mut hi) = (pt(f64::MAX, f64::MAX), pt(f64::MIN, f64::MIN));
    for p in poly {
        lo = pt(lo.x.min(p.x), lo.y.min(p.y));
        hi = pt(hi.x.max(p.x), hi.y.max(p.y));
    }
    let mut best = (f64::MIN, centroid(poly));
    let (mut ox, mut oy) = (lo.x, lo.y);
    let (mut w, mut h) = (hi.x - lo.x, hi.y - lo.y);
    for _ in 0..4 {
        let mut round = (f64::MIN, best.1);
        for i in 0..=10 {
            for j in 0..=10 {
                let p = pt(ox + w * f64::from(i) / 10.0, oy + h * f64::from(j) / 10.0);
                if !contains(poly, p) {
                    continue;
                }
                let d = dist_to_boundary(poly, p);
                if d > round.0 {
                    round = (d, p);
                }
            }
        }
        if round.0 > best.0 {
            best = round;
        }
        w /= 4.0;
        h /= 4.0;
        ox = best.1.x - w * 0.5;
        oy = best.1.y - h * 0.5;
    }
    best.1
}

/// The minimum-area oriented bounding box, as four corners in CCW order.
pub fn min_area_box(poly: &[Pt]) -> Vec<Pt> {
    if poly.len() < 3 {
        return poly.to_vec();
    }
    let mut best_area = f64::MAX;
    let mut best: Vec<Pt> = Vec::new();
    for i in 0..poly.len() {
        let a = poly[i];
        let b = poly[(i + 1) % poly.len()];
        let e = sub(b, a);
        if len(e) < 1e-9 {
            continue;
        }
        let u = norm(e);
        let v = pt(-u.y, u.x);
        let (mut lo_u, mut hi_u) = (f64::MAX, f64::MIN);
        let (mut lo_v, mut hi_v) = (f64::MAX, f64::MIN);
        for p in poly {
            let du = dot(*p, u);
            let dv = dot(*p, v);
            lo_u = lo_u.min(du);
            hi_u = hi_u.max(du);
            lo_v = lo_v.min(dv);
            hi_v = hi_v.max(dv);
        }
        let ar = (hi_u - lo_u) * (hi_v - lo_v);
        if ar < best_area {
            best_area = ar;
            let c = |du: f64, dv: f64| pt(u.x * du + v.x * dv, u.y * du + v.y * dv);
            best = vec![
                c(lo_u, lo_v),
                c(hi_u, lo_v),
                c(hi_u, hi_v),
                c(lo_u, hi_v),
            ];
        }
    }
    let mut b = best;
    if signed_area(&b) < 0.0 {
        b.reverse();
    }
    b
}

/// Clip  to the convex-ish region  edge by edge.
pub fn clip_to(subject: &[Pt], clipper: &[Pt]) -> Vec<Pt> {
    let mut out = subject.to_vec();
    let n = clipper.len();
    for i in 0..n {
        if out.len() < 3 {
            return Vec::new();
        }
        let a = clipper[i];
        let b = clipper[(i + 1) % n];
        let e = sub(b, a);
        if len(e) < 1e-9 {
            continue;
        }
        let u = norm(e);
        let outward = pt(u.y, -u.x);
        out = clip_halfplane(&out, outward, dot(a, outward));
    }
    dedup(&mut out);
    out
}

/// Mitred inward offset with per-edge distances, validated against the source.
///
/// This is the offset that keeps a concave cell's area: shrinking toward an
/// interior point preserves shape but throws away most of the ground, which is
/// what makes a city look like a nature reserve.
pub fn inset_miter(poly: &[Pt], ds: &[f64]) -> Vec<Pt> {
    let n = poly.len();
    if n < 3 || signed_area(poly) <= 0.0 {
        return Vec::new();
    }
    let dmax = ds.iter().copied().fold(0.0f64, f64::max);
    let mut out: Vec<Pt> = Vec::with_capacity(n);
    for i in 0..n {
        let prev = poly[(i + n - 1) % n];
        let cur = poly[i];
        let next = poly[(i + 1) % n];
        let u1 = norm(sub(cur, prev));
        let u2 = norm(sub(next, cur));
        // CCW (positive shoelace): inward normal of edge dir u is (-u.y, u.x).
        let n1 = pt(-u1.y, u1.x);
        let n2 = pt(-u2.y, u2.x);
        let d1 = ds[(i + n - 1) % n];
        let d2 = ds[i];
        let bis = add(scale(n1, d1), scale(n2, d2));
        if len(bis) < 1e-9 {
            continue;
        }
        let m = norm(bis);
        let denom = dot(m, n2);
        if denom.abs() < 0.18 {
            continue; // near-spike corner: drop the vertex rather than shoot a miter
        }
        let step = (d2 / denom).clamp(-3.5 * dmax, 3.5 * dmax);
        out.push(add(cur, scale(m, step)));
    }
    // Keep only vertices that really are inside with the clearance we asked for.
    let keep: Vec<Pt> = out
        .into_iter()
        .filter(|p| contains(poly, *p) && dist_to_boundary(poly, *p) >= dmax * 0.94)
        .collect();
    let mut keep = keep;
    dedup(&mut keep);
    if keep.len() < 3 || signed_area(&keep) <= 0.0 {
        return Vec::new();
    }
    // Validate: every edge of the result must stay inside with clearance.
    for i in 0..keep.len() {
        let a = keep[i];
        let b = keep[(i + 1) % keep.len()];
        for t in 1..5 {
            let f = f64::from(t) / 5.0;
            let mid = pt(a.x + (b.x - a.x) * f, a.y + (b.y - a.y) * f);
            if !contains(poly, mid) || dist_to_boundary(poly, mid) < dmax * 0.90 {
                return Vec::new();
            }
        }
    }
    // And it must be simple.
    for i in 0..keep.len() {
        for j in (i + 2)..keep.len() {
            if i == 0 && j == keep.len() - 1 {
                continue;
            }
            if segments_cross(
                keep[i],
                keep[(i + 1) % keep.len()],
                keep[j],
                keep[(j + 1) % keep.len()],
            ) {
                return Vec::new();
            }
        }
    }
    keep
}
