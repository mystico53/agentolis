//! Deterministic f64 geometry. Only +,-,*,/ and sqrt (all IEEE-754 exactly
//! rounded); every transcendental goes through polis_layout::determinism.

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Pt {
    pub x: f64,
    pub y: f64,
}

pub const fn pt(x: f64, y: f64) -> Pt {
    Pt { x, y }
}

impl Pt {
    pub fn add(self, o: Pt) -> Pt {
        pt(self.x + o.x, self.y + o.y)
    }
    pub fn sub(self, o: Pt) -> Pt {
        pt(self.x - o.x, self.y - o.y)
    }
    pub fn mul(self, k: f64) -> Pt {
        pt(self.x * k, self.y * k)
    }
    pub fn dot(self, o: Pt) -> f64 {
        self.x * o.x + self.y * o.y
    }
    pub fn cross(self, o: Pt) -> f64 {
        self.x * o.y - self.y * o.x
    }
    pub fn len(self) -> f64 {
        (self.x * self.x + self.y * self.y).sqrt()
    }
    pub fn dist(self, o: Pt) -> f64 {
        self.sub(o).len()
    }
    pub fn norm(self) -> Pt {
        let l = self.len();
        if l <= 0.0 {
            pt(1.0, 0.0)
        } else {
            pt(self.x / l, self.y / l)
        }
    }
    pub fn perp(self) -> Pt {
        pt(-self.y, self.x)
    }
    pub fn lerp(self, o: Pt, t: f64) -> Pt {
        pt(self.x + (o.x - self.x) * t, self.y + (o.y - self.y) * t)
    }
}

pub fn signed_area(poly: &[Pt]) -> f64 {
    if poly.len() < 3 {
        return 0.0;
    }
    let mut acc = 0.0;
    for i in 0..poly.len() {
        let a = poly[i];
        let b = poly[(i + 1) % poly.len()];
        acc += a.x * b.y - b.x * a.y;
    }
    acc * 0.5
}

pub fn area(poly: &[Pt]) -> f64 {
    signed_area(poly).abs()
}

pub fn centroid(poly: &[Pt]) -> Pt {
    let a = signed_area(poly);
    if a.abs() < 1e-12 {
        if poly.is_empty() {
            return pt(0.0, 0.0);
        }
        let mut s = pt(0.0, 0.0);
        for p in poly {
            s = s.add(*p);
        }
        return s.mul(1.0 / poly.len() as f64);
    }
    let mut cx = 0.0;
    let mut cy = 0.0;
    for i in 0..poly.len() {
        let p = poly[i];
        let q = poly[(i + 1) % poly.len()];
        let f = p.x * q.y - q.x * p.y;
        cx += (p.x + q.x) * f;
        cy += (p.y + q.y) * f;
    }
    pt(cx / (6.0 * a), cy / (6.0 * a))
}

pub fn perimeter(poly: &[Pt]) -> f64 {
    let mut s = 0.0;
    for i in 0..poly.len() {
        s += poly[i].dist(poly[(i + 1) % poly.len()]);
    }
    s
}

/// Keep the half-plane `dot(p, n) <= s`. Sutherland-Hodgman.
pub fn clip_halfplane(poly: &[Pt], n: Pt, s: f64) -> Vec<Pt> {
    let mut out: Vec<Pt> = Vec::with_capacity(poly.len() + 4);
    if poly.is_empty() {
        return out;
    }
    for i in 0..poly.len() {
        let a = poly[i];
        let b = poly[(i + 1) % poly.len()];
        let da = a.dot(n) - s;
        let db = b.dot(n) - s;
        if da <= 0.0 {
            out.push(a);
        }
        if (da < 0.0 && db > 0.0) || (da > 0.0 && db < 0.0) {
            let t = da / (da - db);
            out.push(a.lerp(b, t));
        }
    }
    out
}

/// Longest chord direction (unit), by exhaustive vertex-pair search.
pub fn longest_axis(poly: &[Pt]) -> Pt {
    let mut best = 0.0;
    let mut dir = pt(1.0, 0.0);
    for i in 0..poly.len() {
        for j in (i + 1)..poly.len() {
            let d = poly[j].sub(poly[i]);
            let l2 = d.dot(d);
            if l2 > best {
                best = l2;
                dir = d;
            }
        }
    }
    dir.norm()
}

/// Longest chord length.
pub fn diameter(poly: &[Pt]) -> f64 {
    let mut best = 0.0f64;
    for i in 0..poly.len() {
        for j in (i + 1)..poly.len() {
            best = best.max(poly[j].dist(poly[i]));
        }
    }
    best
}

pub fn bounds(poly: &[Pt]) -> (Pt, Pt) {
    let mut lo = pt(f64::MAX, f64::MAX);
    let mut hi = pt(f64::MIN, f64::MIN);
    for p in poly {
        lo.x = lo.x.min(p.x);
        lo.y = lo.y.min(p.y);
        hi.x = hi.x.max(p.x);
        hi.y = hi.y.max(p.y);
    }
    (lo, hi)
}

/// Oriented-bounding-box aspect ratio along the longest axis (>= 1).
pub fn obb_aspect(poly: &[Pt]) -> f64 {
    if poly.len() < 3 {
        return f64::INFINITY;
    }
    let u = longest_axis(poly);
    let v = u.perp();
    let mut ulo = f64::MAX;
    let mut uhi = f64::MIN;
    let mut vlo = f64::MAX;
    let mut vhi = f64::MIN;
    for p in poly {
        let a = p.dot(u);
        let b = p.dot(v);
        ulo = ulo.min(a);
        uhi = uhi.max(a);
        vlo = vlo.min(b);
        vhi = vhi.max(b);
    }
    let w = uhi - ulo;
    let h = vhi - vlo;
    if h <= 1e-9 {
        return f64::INFINITY;
    }
    (w / h).max(h / w)
}

/// Monotone-chain convex hull, CCW. Deterministic: the sort is total and the
/// comparisons are exact.
///
/// The city rim goes through this because chord-splitting a *convex* polygon
/// always yields two convex polygons, and a convex face guarantees a chord
/// stays inside it. That single invariant is what makes the whole subdivision
/// planar by construction instead of by hope.
pub fn convex_hull(points: &[Pt]) -> Vec<Pt> {
    let mut p: Vec<Pt> = points.to_vec();
    p.sort_by(|a, b| {
        a.x.partial_cmp(&b.x)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.y.partial_cmp(&b.y).unwrap_or(std::cmp::Ordering::Equal))
    });
    p.dedup_by(|a, b| a.x == b.x && a.y == b.y);
    if p.len() < 3 {
        return p;
    }
    let mut hull: Vec<Pt> = Vec::with_capacity(p.len() * 2);
    for &q in p.iter().chain(p.iter().rev()) {
        while hull.len() >= 2 {
            let n = hull.len();
            if hull[n - 1].sub(hull[n - 2]).cross(q.sub(hull[n - 2])) <= 0.0 {
                hull.pop();
            } else {
                break;
            }
        }
        hull.push(q);
        if hull.len() == p.len() && q.x == p[p.len() - 1].x && q.y == p[p.len() - 1].y {
            // finished the lower hull; continue into the upper
        }
    }
    hull.pop();
    // Rebuild cleanly: lower then upper.
    let mut lower: Vec<Pt> = Vec::new();
    for &q in &p {
        while lower.len() >= 2 {
            let n = lower.len();
            if lower[n - 1].sub(lower[n - 2]).cross(q.sub(lower[n - 2])) <= 0.0 {
                lower.pop();
            } else {
                break;
            }
        }
        lower.push(q);
    }
    let mut upper: Vec<Pt> = Vec::new();
    for &q in p.iter().rev() {
        while upper.len() >= 2 {
            let n = upper.len();
            if upper[n - 1].sub(upper[n - 2]).cross(q.sub(upper[n - 2])) <= 0.0 {
                upper.pop();
            } else {
                break;
            }
        }
        upper.push(q);
    }
    lower.pop();
    upper.pop();
    lower.extend(upper);
    lower
}

pub fn point_in_poly(poly: &[Pt], p: Pt) -> bool {
    let mut inside = false;
    let n = poly.len();
    let mut j = n - 1;
    for i in 0..n {
        let a = poly[i];
        let b = poly[j];
        if (a.y > p.y) != (b.y > p.y) {
            let t = (p.y - a.y) / (b.y - a.y);
            if p.x < a.x + t * (b.x - a.x) {
                inside = !inside;
            }
        }
        j = i;
    }
    inside
}

/// Inset a simple polygon by `d`. Miter offset with a centroid-scale fallback
/// when the miter degenerates (which it does on thin or reflex shapes).
pub fn inset(poly: &[Pt], d: f64) -> Option<Vec<Pt>> {
    if poly.len() < 3 || d <= 0.0 {
        return if poly.len() >= 3 {
            Some(poly.to_vec())
        } else {
            None
        };
    }
    let a0 = signed_area(poly);
    if a0.abs() < 1e-9 {
        return None;
    }
    let sign = if a0 > 0.0 { 1.0 } else { -1.0 };
    let n = poly.len();
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let prev = poly[(i + n - 1) % n];
        let cur = poly[i];
        let next = poly[(i + 1) % n];
        let e0 = cur.sub(prev);
        let e1 = next.sub(cur);
        if e0.len() < 1e-9 || e1.len() < 1e-9 {
            continue;
        }
        // inward normals for CCW rings are left-perpendiculars
        let n0 = e0.norm().perp().mul(sign);
        let n1 = e1.norm().perp().mul(sign);
        let bis = n0.add(n1);
        let bl = bis.len();
        if bl < 1e-6 {
            continue;
        }
        let bis = bis.mul(1.0 / bl);
        let cosh = bis.dot(n0);
        if cosh.abs() < 0.20 {
            // near-spike corner: miter would shoot to infinity
            continue;
        }
        out.push(cur.add(bis.mul(d / cosh)));
    }
    if out.len() < 3 {
        return centroid_shrink(poly, d);
    }
    let a1 = signed_area(&out);
    if a1 * a0 <= 0.0 || a1.abs() >= a0.abs() || self_intersects(&out) {
        return centroid_shrink(poly, d);
    }
    Some(out)
}

/// Inset by a *different* distance per edge, by clipping with each edge's
/// inward-offset half-plane.
///
/// For a convex polygon this is exact; for a non-convex one it yields the
/// polygon's kernel, which is still strictly inside. Either way the result never
/// crosses a road, which is what the setback is for.
pub fn inset_edges(poly: &[Pt], d: &[f64]) -> Option<Vec<Pt>> {
    if poly.len() < 3 || d.len() < poly.len() {
        return None;
    }
    let a0 = signed_area(poly);
    if a0.abs() < 1e-9 {
        return None;
    }
    let sign = if a0 > 0.0 { 1.0 } else { -1.0 };
    let mut cur = poly.to_vec();
    for i in 0..poly.len() {
        let a = poly[i];
        let b = poly[(i + 1) % poly.len()];
        let e = b.sub(a);
        if e.len() < 1e-9 {
            continue;
        }
        // inward normal for the ring's own winding
        let m = e.norm().perp().mul(sign);
        // keep dot(p, m) >= dot(a, m) + d  <=>  dot(p, -m) <= -(dot(a,m)+d)
        let s = -(a.dot(m) + d[i]);
        cur = clip_halfplane(&cur, m.mul(-1.0), s);
        if cur.len() < 3 {
            return None;
        }
    }
    if area(&cur) < 1e-6 {
        return None;
    }
    Some(cur)
}

/// Drop vertices that sit within `tol` of the chord between their neighbours.
///
/// Road polylines carry one vertex every few units so the warp can bend them;
/// a block ring inherits all of them, and a twenty-gon makes a mess of lot
/// layout. This collapses the gentle curves back into street frontages without
/// moving the boundary by more than `tol`.
pub fn simplify(poly: &[Pt], tol: f64) -> Vec<Pt> {
    if poly.len() <= 4 {
        return poly.to_vec();
    }
    let mut cur = poly.to_vec();
    loop {
        let n = cur.len();
        if n <= 4 {
            break;
        }
        let mut worst = f64::MAX;
        let mut worst_i = usize::MAX;
        for i in 0..n {
            let a = cur[(i + n - 1) % n];
            let b = cur[i];
            let c = cur[(i + 1) % n];
            let d2 = seg_dist2(b, a, c);
            if d2 < worst {
                worst = d2;
                worst_i = i;
            }
        }
        if worst_i == usize::MAX || worst > tol * tol {
            break;
        }
        cur.remove(worst_i);
    }
    cur
}

/// [`simplify`], carrying a per-edge attribute through the merge.
///
/// This has to run *before* the inset, not after: the warp leaves every block
/// ring gently wavy, and clipping a wavy ring by all of its own edge half-planes
/// collapses it to its convex kernel — which is how a block ends up with a
/// built ring a third of the size it should be.
pub fn simplify_with(poly: &[Pt], attr: &[f64], tol: f64) -> (Vec<Pt>, Vec<f64>) {
    let mut p = poly.to_vec();
    let mut a = attr.to_vec();
    loop {
        let n = p.len();
        if n <= 4 {
            break;
        }
        let mut worst = f64::MAX;
        let mut wi = usize::MAX;
        for i in 0..n {
            let d2 = seg_dist2(p[i], p[(i + n - 1) % n], p[(i + 1) % n]);
            if d2 < worst {
                worst = d2;
                wi = i;
            }
        }
        if wi == usize::MAX || worst > tol * tol {
            break;
        }
        // edge (wi-1, wi) and (wi, wi+1) merge; the survivor keeps the wider road
        let prev = (wi + n - 1) % n;
        a[prev] = a[prev].max(a[wi]);
        p.remove(wi);
        a.remove(wi);
    }
    (p, a)
}

/// Miter inset that keeps a 1:1 vertex correspondence with the input.
///
/// The correspondence is the point: the ring between a polygon and this inset
/// decomposes exactly into one trapezoid per edge, which is how city blocks get
/// street-fronting lots instead of a fan of triangles.
pub fn miter_inset_paired(poly: &[Pt], d: f64) -> Option<Vec<Pt>> {
    let n = poly.len();
    if n < 3 {
        return None;
    }
    let a0 = signed_area(poly);
    if a0.abs() < 1e-9 {
        return None;
    }
    let sign = if a0 > 0.0 { 1.0 } else { -1.0 };
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let prev = poly[(i + n - 1) % n];
        let cur = poly[i];
        let next = poly[(i + 1) % n];
        let e0 = cur.sub(prev);
        let e1 = next.sub(cur);
        if e0.len() < 1e-9 || e1.len() < 1e-9 {
            return None;
        }
        let n0 = e0.norm().perp().mul(sign);
        let n1 = e1.norm().perp().mul(sign);
        let bis = n0.add(n1);
        let bl = bis.len();
        if bl < 1e-9 {
            return None;
        }
        let bis = bis.mul(1.0 / bl);
        let cosh = bis.dot(n0);
        // Clamp the miter so a sharp corner does not shoot to infinity; the
        // result is checked for validity by the caller.
        let step = if cosh.abs() < 0.30 { d * 3.0 } else { d / cosh };
        out.push(cur.add(bis.mul(step)));
    }
    let a1 = signed_area(&out);
    if a1 * a0 <= 0.0 || a1.abs() >= a0.abs() || self_intersects(&out) {
        return None;
    }
    Some(out)
}

/// The convex kernel: the intersection of every edge's interior half-plane,
/// inset by `d`. Always inside the polygon when it is non-empty.
pub fn kernel(poly: &[Pt], d: f64) -> Option<Vec<Pt>> {
    let dd = vec![d; poly.len()];
    inset_edges(poly, &dd)
}

fn centroid_shrink(poly: &[Pt], d: f64) -> Option<Vec<Pt>> {
    let c = centroid(poly);
    let mut rmin = f64::MAX;
    for p in poly {
        rmin = rmin.min(p.dist(c));
    }
    if rmin <= d * 1.05 {
        return None;
    }
    let k = (rmin - d) / rmin;
    Some(poly.iter().map(|p| c.add(p.sub(c).mul(k))).collect())
}

pub fn self_intersects(poly: &[Pt]) -> bool {
    let n = poly.len();
    if n < 4 {
        return false;
    }
    for i in 0..n {
        let a = poly[i];
        let b = poly[(i + 1) % n];
        for j in (i + 2)..n {
            if i == 0 && j == n - 1 {
                continue;
            }
            let c = poly[j];
            let d = poly[(j + 1) % n];
            if segs_cross(a, b, c, d) {
                return true;
            }
        }
    }
    false
}

/// Proper crossing (interiors intersect), endpoints touching excluded.
pub fn segs_cross(a: Pt, b: Pt, c: Pt, d: Pt) -> bool {
    let d1 = b.sub(a).cross(c.sub(a));
    let d2 = b.sub(a).cross(d.sub(a));
    let d3 = d.sub(c).cross(a.sub(c));
    let d4 = d.sub(c).cross(b.sub(c));
    ((d1 > 0.0 && d2 < 0.0) || (d1 < 0.0 && d2 > 0.0))
        && ((d3 > 0.0 && d4 < 0.0) || (d3 < 0.0 && d4 > 0.0))
}

/// Squared distance from point to segment.
pub fn seg_dist2(p: Pt, a: Pt, b: Pt) -> f64 {
    let ab = b.sub(a);
    let l2 = ab.dot(ab);
    if l2 <= 1e-12 {
        return p.sub(a).dot(p.sub(a));
    }
    let mut t = p.sub(a).dot(ab) / l2;
    t = t.clamp(0.0, 1.0);
    let q = a.add(ab.mul(t));
    p.sub(q).dot(p.sub(q))
}

/// Rotate a unit vector by an angle using the deterministic trig table.
pub fn rotate(v: Pt, radians: f64) -> Pt {
    let (s, c) = polis_layout::determinism::det_sin_cos(radians);
    pt(v.x * c - v.y * s, v.x * s + v.y * c)
}
